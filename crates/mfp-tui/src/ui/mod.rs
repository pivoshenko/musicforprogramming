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

const RESTING_WAIT: Duration = Duration::from_millis(250);

const RECONNECT_EVERY: Duration = Duration::from_secs(2);

const SEEK_SHORT: f64 = 30.0;

const SEEK_LONG: f64 = 300.0;

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
        app.complain("The daemon has no catalog yet");
    }

    let mut terminal = ratatui::try_init()?;
    install_signal_restore();
    let outcome = event_loop(&mut terminal, &mut client, &mut app);
    ratatui::restore();
    outcome
}

static CAUGHT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

fn install_signal_restore() {
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `caught` is a plain `extern "C"` function of the right shape, and the
        // only thing it does is store to an atomic, which is async-signal-safe
        unsafe { libc::signal(signal, caught as *const () as libc::sighandler_t) };
    }
}

extern "C" fn caught(signal: libc::c_int) {
    CAUGHT.store(signal, std::sync::atomic::Ordering::Relaxed);
}

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

    let mut check = crate::update::notice::spawn();

    while !app.quit {
        honour_caught_signal();

        while event::poll(Duration::ZERO)? {
            match event::read()? {
                event::Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(client, app, key);
                }

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
                        app.complain("Lost the daemon, retrying");
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

                autostarted = false;
                app.rescan_cache();
                dirty = true;
                continue;
            }
        }

        let animating = app.animating();
        let budget = match (animating, app.disconnected) {
            (_, true) => RECONNECT_EVERY.saturating_sub(last_retry.elapsed()),
            (true, false) => tick.saturating_sub(last_tick.elapsed()),
            (false, false) => RESTING_WAIT,
        };

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

fn wait(socket: Option<std::os::fd::RawFd>, timeout: Duration) -> Result<()> {
    let mut fds = Vec::with_capacity(2);

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

fn apply(app: &mut App, frame: Frame) -> bool {
    let Frame::Event(event) = frame else {
        return false;
    };
    let Event::State { state } = event.event else {
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

fn quit(client: &mut Client, app: &mut App) {
    let _ = client.request(Command::Shutdown);
    app.quit = true;
}

fn detach(app: &mut App) {
    app.quit = true;
}

fn handle_key(client: &mut Client, app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        detach(app);
        return;
    }

    match app.mode {
        Mode::Search => search_key(app, key),
        Mode::Help => {
            app.mode = Mode::Normal;
        }
        Mode::Normal => normal_key(client, app, key),
    }
}

fn search_key(app: &mut App, key: KeyEvent) {
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
        app.complain("This episode cannot be sought yet");
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

fn send(client: &mut Client, app: &mut App, command: Command) {
    let _ = accepted(client, app, command);
}

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

fn pseudo_random(bound: usize) -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos() as u64);

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

    fn recording_daemon(path: std::path::PathBuf) -> std::sync::mpsc::Receiver<Command> {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let listener = UnixListener::bind(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if let Ok(request) = serde_json::from_str::<mfp_core::protocol::Request>(&line) {
                    let response = mfp_core::protocol::Response::ok(request.id);
                    let mut out = &stream;
                    let _ = writeln!(out, "{}", serde_json::to_string(&response).unwrap());
                    let _ = tx.send(request.cmd);
                }
                line.clear();
            }
        });
        rx
    }

    fn connected(dir: &tempfile::TempDir) -> (Client, App, std::sync::mpsc::Receiver<Command>) {
        let path = dir.path().join("d.sock");
        let rx = recording_daemon(path.clone());
        let client = Client::connect_at(&path).unwrap();
        let catalog = mfp_core::model::Catalog {
            episodes: Vec::new(),
            info: Vec::new(),
            fetched_at: 0,
            enriched: false,
        };
        (client, App::new(catalog, StateSnapshot::stopped()), rx)
    }

    fn commands(rx: &std::sync::mpsc::Receiver<Command>, window: Duration) -> Vec<Command> {
        let deadline = Instant::now() + window;
        let mut seen = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now())
            && let Ok(command) = rx.recv_timeout(left)
        {
            seen.push(command);
        }
        seen
    }

    #[test]
    fn ctrl_c_detaches_and_leaves_playback_running() {
        let dir = tempfile::tempdir().unwrap();
        let (mut client, mut app, rx) = connected(&dir);

        handle_key(
            &mut client,
            &mut app,
            press(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert!(app.quit, "ctrl+c did not leave the interface");
        let sent = commands(&rx, Duration::from_millis(300));
        assert!(
            !sent.contains(&Command::Shutdown),
            "ctrl+c shut the daemon down rather than detaching: {sent:?}"
        );
    }

    #[test]
    fn q_quits_the_player_and_ends_the_daemon_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut client, mut app, rx) = connected(&dir);

        handle_key(
            &mut client,
            &mut app,
            press(KeyCode::Char('q'), KeyModifiers::NONE),
        );

        assert!(app.quit, "q did not leave the interface");
        let sent = commands(&rx, Duration::from_secs(2));
        assert!(
            sent.contains(&Command::Shutdown),
            "q left the daemon running: {sent:?}"
        );
    }

    #[test]
    fn detaching_asks_the_daemon_for_nothing_at_all() {
        let catalog = mfp_core::model::Catalog {
            episodes: Vec::new(),
            info: Vec::new(),
            fetched_at: 0,
            enriched: false,
        };
        let mut app = App::new(catalog, StateSnapshot::stopped());
        assert!(!app.quit);

        detach(&mut app);

        assert!(app.quit);
    }

    #[test]
    fn a_bare_character_is_typed_into_the_query() {
        let mut app = searching_app("los");
        search_key(&mut app, press(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(app.query, "losc");
    }

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
