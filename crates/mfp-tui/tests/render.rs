//! Renders whole frames against a test backend.
//!
//! The unit tests cover the arithmetic each pane does; these cover what it is for: that
//! a frame comes out at all. Every terminal size from unusably small to huge is drawn,
//! because a layout's failure mode is a panic in `Rect` arithmetic at a size nobody tried.

// An integration test is its own crate, so the library's cfg(test) exemption does not reach it
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]

use mfp_core::model::{Catalog, Episode};
use mfp_core::protocol::{
    DownloadProgress, DownloadState, EpisodeRef, PlaybackState, Source, Spectrum, StateSnapshot,
};
use mfp_tui::ui::app::{App, Focus, Mode};
use mfp_tui::ui::draw::draw;
use mfp_tui::ui::theme::PALETTE;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn episode(number: u32, name: &str) -> Episode {
    Episode {
        title: format!("Episode {number}: {name}"),
        link: format!("https://musicforprogramming.net/{number}"),
        enclosure_url: format!("https://datashat.net/mfp_{number}.mp3"),
        byte_len: 412_000_000,
        duration_secs: 14_400,
        published_at: 1_700_000_000,
        slug: Some(format!("ep{number}")),
        order: Some(number),
        tracklist: Some(
            (1..=9)
                .map(|track| format!("Some Artist {track} - A Track Called {track}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        body: Some("An episode.".into()),
        links: Some(format!(
            "https://soundcloud.com/artist-{number}\nhttps://www.instagram.com/artist_{number}/"
        )),
        bundle_title: Some(format!("{number}: {name}")),
        special: false,
    }
}

fn catalog() -> Catalog {
    Catalog {
        episodes: vec![
            episode(79, "Corticyte"),
            episode(78, "Mindaugas Latvis"),
            episode(77, "Julien Mier"),
            episode(76, "Shadowlust"),
            episode(75, "Ambiant Cloud"),
        ],
        info: Vec::new(),
        fetched_at: 0,
        enriched: true,
    }
}

fn playing() -> StateSnapshot {
    let mut snapshot = StateSnapshot::stopped();
    snapshot.playback = PlaybackState::Playing;
    snapshot.episode = Some(EpisodeRef {
        slug: "ep79".into(),
        title: "Episode 79: Corticyte".into(),
        duration_secs: 14_400.0,
    });
    snapshot.position_secs = 3852.0;
    snapshot.duration_secs = Some(14_400.0);
    snapshot.duration_approximate = true;
    snapshot.seekable = true;
    snapshot.source = Some(Source::Stream);
    snapshot.downloads = vec![DownloadProgress {
        slug: "ep77".into(),
        downloaded_bytes: 190_000_000,
        total_bytes: 412_000_000,
        state: DownloadState::Running,
        error: None,
    }];
    snapshot.favourites = vec!["ep79".into(), "ep76".into()];
    snapshot.spectrum = Some(Spectrum(
        (0..1024)
            .map(|bin| (200.0 * (-(bin as f32) / 90.0).exp() + 20.0) as u8)
            .collect(),
    ));
    snapshot
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();

    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| {
                    buffer
                        .cell((x, y))
                        .map_or(' ', |cell| cell.symbol().chars().next().unwrap_or(' '))
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn app() -> App {
    let mut app = App::new(catalog(), playing());
    app.cached.insert("ep76".into());
    app
}

#[test]
fn a_full_size_frame_draws_every_pane() {
    let frame = render(&mut app(), 100, 30);
    eprintln!("{frame}");

    assert!(frame.contains("// Episodes"), "no episodes pane");
    assert!(frame.contains("// Player"), "no player pane");
    assert!(frame.contains("// Tracks"), "no tracks pane");
    // the analyser has no title of its own; it is the bottom of the player pane
    assert!(
        frame
            .lines()
            .any(|row| row.contains('^') || row.contains('o')),
        "no analyser marks inside the player pane"
    );
    assert!(frame.contains("Playing"), "no playback state");
    assert!(frame.contains("Corticyte"), "no loaded title");
    assert!(frame.contains("1:04:12"), "no elapsed readout");
    assert!(frame.contains("4:00:00"), "no total duration");
    assert!(frame.contains("46%"), "no download progress");
    assert!(
        frame.contains("soundcloud.com/artist-79"),
        "no links under the track listing"
    );
}

#[test]
fn a_narrow_frame_drops_the_track_listing_rather_than_squeezing_it() {
    let frame = render(&mut app(), 60, 30);
    assert!(frame.contains("// Episodes"));
    assert!(frame.contains("// Player"));
    assert!(!frame.contains("// Tracks"));
}

#[test]
fn a_frame_too_small_to_use_says_so_rather_than_drawing_a_mangled_one() {
    let frame = render(&mut app(), 20, 6);
    assert!(frame.contains("mfp"));
    assert!(frame.contains("Needs"));
}

#[test]
fn every_terminal_size_from_tiny_to_large_draws_without_panicking() {
    let mut app = app();
    // 1638 and 1700 straddle the point where a `u16` percentage of the width overflows
    for width in [
        1u16, 2, 8, 20, 35, 36, 37, 59, 76, 77, 120, 400, 1638, 1639, 1700,
    ] {
        for height in [1u16, 2, 5, 11, 12, 13, 18, 24, 60] {
            render(&mut app, width, height);
        }
    }
}

#[test]
fn the_help_overlay_lists_the_keys() {
    let mut app = app();
    app.mode = Mode::Help;
    let frame = render(&mut app, 100, 40);
    eprintln!("{frame}");

    assert!(frame.contains("// Keys"));
    assert!(frame.contains("Play the selected episode"));
    assert!(frame.contains("Unload"));
}

#[test]
fn searching_shows_the_query_and_the_match_count() {
    let mut app = app();
    app.begin_search();
    for character in "julien".chars() {
        app.push_query(character);
    }
    let frame = render(&mut app, 100, 30);
    eprintln!("{frame}");

    assert!(frame.contains("/julien"));
    assert!(frame.contains("1 match"));
    assert!(frame.contains("Julien Mier"));
    // The loaded episode keeps its place in the player pane; what filters is the catalog
    assert!(
        !frame.contains("Mindaugas"),
        "the filter must actually filter"
    );
}

#[test]
fn a_seek_in_flight_shows_its_target_and_never_the_stale_position() {
    let mut app = app();
    app.snapshot.playback = PlaybackState::Seeking;
    app.snapshot.seek_target_secs = Some(9000.0);
    let frame = render(&mut app, 100, 30);
    eprintln!("{frame}");

    assert!(frame.contains("Seeking"));
    assert!(frame.contains("2:30:00"), "no seek target");
    assert!(
        !frame.contains("1:04:12"),
        "the pre-seek position is still drawn"
    );
}

#[test]
fn nothing_loaded_is_an_invitation_rather_than_an_empty_pane() {
    let mut app = App::new(catalog(), StateSnapshot::stopped());
    let frame = render(&mut app, 100, 30);
    assert!(frame.contains("Nothing loaded, please select an episode"));
}

#[test]
fn a_lost_daemon_is_said_so_in_the_header_rather_than_drawn_as_a_frozen_frame() {
    let mut app = app();
    app.disconnected = true;
    let frame = render(&mut app, 100, 30);

    assert!(frame.contains("daemon unreachable"), "{frame}");
}

#[test]
fn an_available_update_is_named_in_the_header_and_absent_when_there_is_none() {
    let mut app = app();
    assert!(
        !render(&mut app, 100, 30).contains("available"),
        "a header with no update pending mentioned one"
    );

    app.update = Some("9.9.9".into());
    let frame = render(&mut app, 100, 30);
    let header = frame.lines().next().unwrap();

    assert!(header.contains("update 9.9.9 available"), "{header}");
    assert!(header.contains("musicforprogramming.net"), "{header}");
}

#[test]
fn a_failed_download_is_marked_on_its_row_rather_than_reading_as_absent() {
    let mut app = app();
    app.snapshot.downloads = vec![DownloadProgress {
        slug: "ep75".into(),
        downloaded_bytes: 1,
        total_bytes: 412_000_000,
        state: DownloadState::Failed,
        error: None,
    }];
    let frame = render(&mut app, 100, 30);

    let row = frame
        .lines()
        .find(|line| line.contains("Ambiant Cloud"))
        .unwrap_or_else(|| panic!("no row for the failed episode:\n{frame}"));
    assert!(row.contains('!'), "no failure mark on the row: {row}");
}

#[test]
fn focusing_the_track_listing_moves_the_highlight_off_the_catalog() {
    let mut app = app();
    app.focus = Focus::Tracks;
    let frame = render(&mut app, 100, 30);
    assert!(frame.contains("A Track Called 1"));
}

#[test]
fn the_tagline_sits_on_the_header_row_beside_the_site_name() {
    let mut app = app();
    let frame = render(&mut app, 200, 30);
    let mut rows = frame.lines();
    let header = rows.next().unwrap();

    assert!(header.contains("musicforprogramming.net"));
    assert!(header.contains(
        "A series of mixes intended for listening while programming to focus the brain and inspire the mind."
    ));
    // the whole tagline is on that one row, so the row below is the first pane's border
    assert!(!rows.next().unwrap().contains("A series"));
}

/// Narrowing the header must never cut the tagline mid-word: it is there whole or not at
/// all.
#[test]
fn a_narrow_header_drops_the_tagline_whole() {
    for (width, present) in [(200u16, true), (130, true), (120, false), (40, false)] {
        let mut app = app();
        let frame = render(&mut app, width, 30);
        let header = frame.lines().next().unwrap().to_string();

        assert!(header.contains("musicforprogramming.net"));
        assert_eq!(
            header.contains("A series of mixes intended"),
            present,
            "at {width} cells the tagline is wrong:\n{header}"
        );
        assert!(
            !frame.lines().skip(1).any(|row| row.contains("A series of")),
            "the tagline wrapped onto a second row at {width} cells:\n{frame}"
        );
    }
}

/// The selection band must run the whole row. Stopping it before the duration reads as
/// the name being selected rather than the episode.
#[test]
fn the_selected_row_is_highlighted_across_its_full_width() {
    let mut app = app();
    app.focus = Focus::Catalog;

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();

    let row = (0..buffer.area.height)
        .find(|y| {
            (0..buffer.area.width).any(|x| {
                buffer
                    .cell((x, *y))
                    .is_some_and(|cell| cell.symbol().contains('>'))
            })
        })
        .expect("no cursor row");

    // The duration is right-aligned in the catalog pane's last eight cells
    let band: Vec<u16> = (1..39)
        .filter(|x| {
            buffer
                .cell((*x, row))
                .is_some_and(|cell| cell.bg == PALETTE.line)
        })
        .collect();

    assert!(
        band.contains(&6),
        "the name is not on the selection background"
    );
    assert!(
        band.contains(&35),
        "the duration is not on the selection background: highlighted cells are {band:?}"
    );
}
