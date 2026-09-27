//! The terminal interface: set up, run, tear down.
//!
//! The loop here does three things and nothing else - read keys, read the daemon's pushed
//! snapshots, and redraw when either changed something. Drawing only on a change lets a
//! paused player sit on screen indefinitely at no cost; never blocking on a command lets an
//! eleven-second seek happen without the interface appearing to hang.

pub mod anim;
pub mod app;
pub mod draw;
pub mod panes;
pub mod theme;

use std::time::{Duration, Instant};

use anyhow::Result;
use mfp_core::protocol::{Command, DownloadState, Event, Frame, StateSnapshot};
use ratatui::crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::client::{Client, ClientError};

use app::{App, Focus, Mode};

/// The longest the loop sleeps with nothing moving.
///
/// Not how quickly a keypress is answered - the wait returns the instant the terminal or
/// the daemon has anything - only how long a terminal resize, which arrives as a signal
/// rather than as readable bytes, can sit unnoticed.
const RESTING_WAIT: Duration = Duration::from_millis(250);

/// How often a lost connection is retried.
const RECONNECT_EVERY: Duration = Duration::from_secs(2);

/// Seconds a short seek moves.
const SEEK_SHORT: f64 = 30.0;

/// Seconds a long seek moves.
const SEEK_LONG: f64 = 300.0;

/// Draws the interface until the user quits.
///
/// Quitting stops playback and ends the daemon with it: the interface is the player, so
/// leaving a process behind still holding the audio device is not what quitting means.
pub fn run(mut client: Client) -> Result<()> {
    client.subscribe().map_err(anyhow::Error::from)?;
    let snapshot = client.status().unwrap_or_else(|_| StateSnapshot::stopped());
    let catalog = client
        .catalog()
        .unwrap_or_else(|_| mfp_core::model::Catalog {
            episodes: Vec::new(),
            info: Vec::new(),
            fetched_at: 0,
            enriched: false,
        });

    let mut app = App::new(catalog, snapshot);
    app.update = crate::update::notice::available();
    if app.catalog.episodes.is_empty() {
        app.complain("the daemon has no catalog yet");
    }

    let mut terminal = ratatui::try_init()?;
    install_signal_restore();
    let outcome = event_loop(&mut terminal, &mut client, &mut app);
    ratatui::restore();
    outcome
}

/// The signal that asked this process to end, or 0. Set by [`caught`], acted on by the
/// event loop.
static CAUGHT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Arranges for the terminal to be restored on the signals that would otherwise end the
/// process without unwinding.
///
/// `ratatui::try_init`'s panic hook covers a panic and an early return, but a `SIGTERM`
/// from a `pkill` or a `SIGHUP` from a closing session kills the process outright: raw mode
/// stays on, the alternate screen stays up, and the shell that inherits the terminal is
/// unusable until the user blind-types `reset`.
fn install_signal_restore() {
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `caught` is a plain `extern "C"` function of the right shape, and the
        // only thing it does is store to an atomic, which is async-signal-safe
        unsafe { libc::signal(signal, caught as *const () as libc::sighandler_t) };
    }
}

/// Records the signal and returns.
///
/// Restoring the terminal from inside the handler is the obvious thing and the wrong one:
/// it writes to stdout, and a signal landing while the draw already holds that lock would
/// deadlock the process it was meant to end. The loop wakes on the `EINTR` the signal
/// causes and does the work where locks are allowed.
extern "C" fn caught(signal: libc::c_int) {
    CAUGHT.store(signal, std::sync::atomic::Ordering::Relaxed);
}

/// Restores the terminal and dies of the signal that was caught, so the parent sees the
/// process killed by `SIGTERM` rather than exited cleanly. Never returns when one was.
fn honour_caught_signal() {
    let signal = CAUGHT.load(std::sync::atomic::Ordering::Relaxed);
    if signal == 0 {
        return;
    }
    ratatui::restore();
    // SAFETY: restoring the default disposition and re-raising is the documented way to
    // die of the signal that was caught, and both calls take a signal number and nothing
    // that could dangle
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    client: &mut Client,
    app: &mut App,
) -> Result<()> {
    let mut dirty = true;
    let mut last_tick = Instant::now();
    let mut last_retry = Instant::now();
    let mut autostarted = false;
    let tick = Duration::from_millis(anim::TICK_MS);
    // Started here rather than waited for: a session lasts long enough that the answer can
    // arrive into a frame, which is why the interface never spends the grace period a
    // one-line subcommand does
    let mut check = crate::update::notice::spawn();

    while !app.quit {
        honour_caught_signal();

        // Everything already buffered, before anything blocks: crossterm and the client each
        // hold bytes of their own, and a descriptor with nothing left to read says nothing
        // about whether those are empty
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                event::Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(client, app, key);
                }
                // A resize redraws without changing anything
                event::Event::Resize(_, _) => {}
                _ => continue,
            }
            dirty = true;
        }

        loop {
            match client.poll_frame(Duration::ZERO) {
                Ok(Some(frame)) => {
                    dirty |= apply(app, frame);
                    app.disconnected = false;
                }
                Ok(None) => break,
                Err(_) => {
                    if !app.disconnected {
                        app.disconnected = true;
                        app.complain("lost the daemon, retrying");
                        dirty = true;
                    }
                    break;
                }
            }
        }

        if app.expire_status() {
            dirty = true;
        }
        if crate::update::notice::landed(&mut check) {
            app.update = crate::update::notice::available();
            dirty = true;
        }
        if app.quit {
            break;
        }
        if dirty {
            terminal.draw(|frame| draw::draw(frame, app))?;
            dirty = false;
        }

        if app.disconnected && last_retry.elapsed() >= RECONNECT_EVERY {
            last_retry = Instant::now();
            // A daemon that simply died is started once, the case worth healing; every retry
            // after that only reattaches. `connect` spawns a daemon and then blocks up to
            // two seconds on it, so against one that dies as it comes up, retrying that way
            // would spend every one of those seconds unable to read a key, quit included
            let attempt = if autostarted {
                Client::reattach()
            } else {
                autostarted = true;
                Client::connect()
            };

            if let Ok(mut fresh) = attempt
                && fresh.subscribe().is_ok()
            {
                if let Ok(snapshot) = fresh.status() {
                    app.snapshot = snapshot;
                }
                if let Ok(catalog) = fresh.catalog() {
                    app.catalog = catalog;
                    app.refilter();
                }
                *client = fresh;
                app.disconnected = false;
                // The next loss gets its own restart
                autostarted = false;
                app.rescan_cache();
                dirty = true;
                continue;
            }
        }

        // At rest this blocks until something actually happens, so a paused player costs
        // four wakeups a second and no redraws at all
        let animating = app.animating();
        let budget = match (animating, app.disconnected) {
            (_, true) => RECONNECT_EVERY.saturating_sub(last_retry.elapsed()),
            (true, false) => tick.saturating_sub(last_tick.elapsed()),
            (false, false) => RESTING_WAIT,
        };
        // A closed socket polls readable forever, so a lost daemon is left out of the wait
        // entirely; otherwise every iteration returns at once and the retry interval spins
        let socket = (!app.disconnected).then(|| client.as_raw_fd());
        wait(socket, budget)?;

        if animating && last_tick.elapsed() >= tick {
            last_tick = Instant::now();
            app.tick = app.tick.wrapping_add(1);
            dirty = true;
        } else if !animating {
            last_tick = Instant::now();
        }
    }

    Ok(())
}

/// Blocks until the terminal or the daemon has something to say, or `timeout` elapses.
///
/// One call over both descriptors rather than a short timeout on each in turn: alternating
/// timeouts wake the loop continuously whether or not anything happened, and the
/// interface's cost at rest is exactly the number of times it wakes.
///
/// `socket` is `None` while the daemon is unreachable, because a closed socket is reported
/// readable and would turn every wait into no wait at all.
fn wait(socket: Option<std::os::fd::RawFd>, timeout: Duration) -> Result<()> {
    let mut fds = Vec::with_capacity(2);
    // Only when it is a terminal. Crossterm reads keys from `/dev/tty` when stdin is not
    // one, so with stdin redirected this descriptor is a file or `/dev/null`, permanently
    // readable at EOF: polling it would return every wait at once and spin the loop at full
    // speed for the life of the program
    if keyboard_is_stdin() {
        fds.push(libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        });
    }
    if let Some(fd) = socket {
        fds.push(libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
    }

    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // With nothing to watch there is no reason to enter the kernel at all: sleeping the
    // budget is what `poll` would have done with an empty set anyway
    if fds.is_empty() {
        std::thread::sleep(timeout);
        return Ok(());
    }

    // SAFETY: `fds` is a live local `Vec` that outlives the call, so `as_mut_ptr` is valid
    // for reads and writes of exactly `fds.len()` `pollfd`s for its whole duration; the
    // length is at most 2 and so cannot truncate in the `nfds_t` cast; `millis` is clamped
    // to `i32::MAX` and can never be negative, so this cannot become an unbounded block;
    // and `poll` does not retain the pointer past its return
    if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) } < 0 {
        let error = std::io::Error::last_os_error();
        // A signal woke us - a resize, most often. The caller loops and reads whatever
        // arrived, exactly as it would have on a normal return
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
    Ok(())
}

fn keyboard_is_stdin() -> bool {
    // SAFETY: `isatty` reads no memory, only the kernel's idea of a descriptor, and
    // `STDIN_FILENO` is always a valid descriptor number to ask about
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

/// Folds one frame from the daemon into the interface's copy of the world, reporting whether
/// anything on screen would look different for it.
///
/// The daemon pushes a snapshot several times a second whether or not one changed, so a
/// frame that says nothing new must not cost a redraw: otherwise a player paused all
/// afternoon repaints the terminal a quarter of a million times.
fn apply(app: &mut App, frame: Frame) -> bool {
    let Frame::Event(event) = frame else {
        // A late response to a command already given up on; the snapshot that matters will
        // arrive as an event
        return false;
    };
    let Event::State { state } = event.event else {
        // An event kind only a newer daemon knows: nothing on screen changes for it
        return false;
    };
    if state == app.snapshot {
        return false;
    }

    let was_complete = completed(&app.snapshot);
    let had_episode = app.loaded_id().map(str::to_owned);
    app.snapshot = state;

    if completed(&app.snapshot) != was_complete {
        app.rescan_cache();
    }

    // Following the daemon's own next/previous keeps the cursor on what is playing, but
    // only when it was already there: a user who has scrolled away is browsing
    if had_episode.is_some()
        && had_episode.as_deref() != app.loaded_id()
        && app.selection().map(|episode| episode.id().into_owned()) == had_episode
    {
        app.select_loaded();
    }

    true
}

fn completed(snapshot: &StateSnapshot) -> Vec<String> {
    snapshot
        .downloads
        .iter()
        .filter(|download| download.state == DownloadState::Completed)
        .map(|download| download.slug.clone())
        .collect()
}

// == Keys ==

/// Ends the session: the daemon persists its position and exits, then the interface does.
///
/// The daemon's answer is not waited on. It is going away either way, and a client that
/// hung on the way out would be a worse failure than one that left a moment early.
fn quit(client: &mut Client, app: &mut App) {
    let _ = client.request(Command::Shutdown);
    app.quit = true;
}

fn handle_key(client: &mut Client, app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        quit(client, app);
        return;
    }

    match app.mode {
        Mode::Search => search_key(app, key),
        Mode::Help => {
            // Any key closes it, so nothing has to be learned to get out of the list of
            // things to learn
            app.mode = Mode::Normal;
        }
        Mode::Normal => normal_key(client, app, key),
    }
}

fn search_key(app: &mut App, key: KeyEvent) {
    // A modified character is a command, not text. Without this, `ctrl-u` and `ctrl-w` -
    // the two a reflex reaches for in any text field - type a `u` and a `w` into the
    // query instead of clearing it
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        match key.code {
            KeyCode::Char('u') => app.clear_query(),
            KeyCode::Char('w') => app.pop_query_word(),
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => app.cancel_search(),
        KeyCode::Enter => app.accept_search(),
        KeyCode::Backspace => app.pop_query(),
        KeyCode::Down => app.move_selection(1),
        KeyCode::Up => app.move_selection(-1),
        KeyCode::Char(character) => app.push_query(character),
        _ => {}
    }
}

fn normal_key(client: &mut Client, app: &mut App, key: KeyEvent) {
    // Measured from the focused pane as it last drew, so "half a page" is half of what is on
    // screen rather than a constant that is half of some other terminal's page
    let page = app.page();
    let half = (page / 2).max(1);

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('d') => scroll(app, half),
            KeyCode::Char('u') => scroll(app, -half),
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Char('q') => quit(client, app),
        KeyCode::Char('?') => app.mode = Mode::Help,
        KeyCode::Tab => app.focus = app.focus.next(),
        KeyCode::BackTab => app.focus = app.focus.previous(),
        KeyCode::Char('/') => app.begin_search(),

        KeyCode::Char('j') | KeyCode::Down => scroll(app, 1),
        KeyCode::Char('k') | KeyCode::Up => scroll(app, -1),
        KeyCode::PageDown => scroll(app, page),
        KeyCode::PageUp => scroll(app, -page),
        KeyCode::Char('g') | KeyCode::Home => match app.focus {
            Focus::Catalog => app.select_first(),
            Focus::Tracks => app.tracks_offset = 0,
        },
        KeyCode::Char('G') | KeyCode::End => match app.focus {
            Focus::Catalog => app.select_last(),
            Focus::Tracks => app.scroll_tracks_to_end(),
        },

        KeyCode::Enter => play_selected(client, app),
        KeyCode::Char(' ') => send(client, app, Command::Toggle),
        KeyCode::Char('u') => send(client, app, Command::Stop),
        KeyCode::Char('n') => send(client, app, Command::Next),
        KeyCode::Char('p') => send(client, app, Command::Previous),

        KeyCode::Char('l') | KeyCode::Right => seek(client, app, SEEK_SHORT),
        KeyCode::Char('h') | KeyCode::Left => seek(client, app, -SEEK_SHORT),
        KeyCode::Char('L') => seek(client, app, SEEK_LONG),
        KeyCode::Char('H') => seek(client, app, -SEEK_LONG),
        KeyCode::Esc => cancel_seek(client, app),

        KeyCode::Char('r') => random(client, app),
        KeyCode::Char('f') => favourite(client, app),
        KeyCode::Char('d') => download(client, app),
        KeyCode::Char('x') => remove(client, app),

        _ => {}
    }
}

fn scroll(app: &mut App, delta: isize) {
    match app.focus {
        Focus::Catalog => app.move_selection(delta),
        Focus::Tracks => app.scroll_tracks(delta),
    }
}

fn play_selected(client: &mut Client, app: &mut App) {
    let Some(episode) = app.selection() else {
        return;
    };
    let id = episode.id().into_owned();
    send(client, app, Command::Play { slug: Some(id) });
}

fn seek(client: &mut Client, app: &mut App, delta: f64) {
    if app.snapshot.episode.is_none() {
        return;
    }
    if !app.snapshot.seekable {
        app.complain("this episode cannot be sought yet");
        return;
    }
    send(
        client,
        app,
        Command::Seek {
            position_secs: None,
            delta_secs: Some(delta),
        },
    );
}

/// Abandons a seek in flight by seeking back to the position the daemon last reported.
///
/// The protocol has no cancel: a later seek supersedes an earlier one, so asking for where
/// playback already is *is* the cancel.
fn cancel_seek(client: &mut Client, app: &mut App) {
    if app.snapshot.seek_target_secs.is_none() {
        return;
    }
    send(
        client,
        app,
        Command::Seek {
            position_secs: Some(app.snapshot.position_secs),
            delta_secs: None,
        },
    );
}

fn random(client: &mut Client, app: &mut App) {
    if app.matches.is_empty() {
        return;
    }
    let pick = pseudo_random(app.matches.len());
    app.selected = pick;
    app.tracks_offset = 0;
    play_selected(client, app);
}

fn favourite(client: &mut Client, app: &mut App) {
    let Some(episode) = app.selection() else {
        return;
    };
    let id = episode.id().into_owned();

    let command = match app.is_favourite(&id) {
        true => Command::Unfavourite { slug: id },
        false => Command::Favourite { slug: id },
    };
    send(client, app, command);
}

fn download(client: &mut Client, app: &mut App) {
    let Some(episode) = app.selection() else {
        return;
    };
    let id = episode.id().into_owned();

    if app.cached.contains(&id) {
        return;
    }
    send(client, app, Command::Download { slug: id });
}

/// Cancels a transfer in flight, or deletes a complete local copy. One key, because from
/// the user's side both mean "stop having this on disk".
fn remove(client: &mut Client, app: &mut App) {
    let Some(episode) = app.selection() else {
        return;
    };
    let id = episode.id().into_owned();

    let in_flight = app.download_of(&id).is_some_and(|progress| {
        matches!(
            progress.state,
            DownloadState::Queued | DownloadState::Running
        )
    });

    if in_flight {
        send(client, app, Command::CancelDownload { slug: id });
    } else if app.cached.contains(&id) {
        send(client, app, Command::DeleteDownload { slug: id });
        app.rescan_cache();
    }
}

/// Sends a command, reporting only failure.
///
/// Success says so by changing the screen: the pushed snapshot that follows renames the
/// player, moves the state word, or marks the row. Only a refusal needs words.
fn send(client: &mut Client, app: &mut App, command: Command) {
    let _ = accepted(client, app, command);
}

/// True when the daemon took the command. A refusal or a lost connection reaches the
/// header either way, because that is the case the caller cannot see for itself.
fn accepted(client: &mut Client, app: &mut App, command: Command) -> bool {
    match client.request(command) {
        Ok(_) => true,
        Err(ClientError::Rejected(error)) => {
            app.complain(error.message);
            false
        }
        Err(error) => {
            app.disconnected = true;
            app.complain(error.to_string());
            false
        }
    }
}

/// An index below `bound`, from the clock.
///
/// Good enough for picking an episode and not worth a dependency: the only property required is
/// that pressing `r` twice rarely gives the same answer.
fn pseudo_random(bound: usize) -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos() as u64);
    // The low bits of a nanosecond clock are the well-mixed ones
    let mixed = nanos.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 32;
    (mixed as usize) % bound.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_random_pick_is_always_inside_the_list() {
        for bound in 1..40 {
            for _ in 0..20 {
                assert!(pseudo_random(bound) < bound);
            }
        }
    }

    #[test]
    fn a_random_pick_from_an_empty_list_is_not_a_division_by_zero() {
        assert_eq!(pseudo_random(0), 0);
    }

    fn searching_app(query: &str) -> App {
        let catalog = mfp_core::model::Catalog {
            episodes: Vec::new(),
            info: Vec::new(),
            fetched_at: 0,
            enriched: false,
        };
        let mut app = App::new(catalog, StateSnapshot::stopped());
        app.begin_search();
        for character in query.chars() {
            app.push_query(character);
        }
        app
    }

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn a_bare_character_is_typed_into_the_query() {
        let mut app = searching_app("los");
        search_key(&mut app, press(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(app.query, "losc");
    }

    /// The reflex in any text field. Before this, both typed their own letter into the
    /// query instead of editing it.
    #[test]
    fn a_control_character_edits_the_query_rather_than_being_typed_into_it() {
        let mut app = searching_app("sea island");
        search_key(&mut app, press(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.query, "sea ");

        search_key(&mut app, press(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.query, "");
    }

    #[test]
    fn a_modified_character_with_no_meaning_is_ignored_rather_than_typed() {
        let mut app = searching_app("los");
        search_key(&mut app, press(KeyCode::Char('d'), KeyModifiers::CONTROL));
        search_key(&mut app, press(KeyCode::Char('x'), KeyModifiers::ALT));
        assert_eq!(app.query, "los", "a modified key was typed into the query");
    }

    #[test]
    fn completed_downloads_are_extracted_in_order() {
        let mut snapshot = StateSnapshot::stopped();
        snapshot.downloads = vec![
            mfp_core::protocol::DownloadProgress {
                slug: "a".into(),
                downloaded_bytes: 1,
                total_bytes: 1,
                state: DownloadState::Completed,
                error: None,
            },
            mfp_core::protocol::DownloadProgress {
                slug: "b".into(),
                downloaded_bytes: 0,
                total_bytes: 1,
                state: DownloadState::Running,
                error: None,
            },
        ];
        assert_eq!(completed(&snapshot), vec!["a".to_owned()]);
    }
}
