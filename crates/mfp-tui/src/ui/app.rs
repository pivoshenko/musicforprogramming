//! Everything the interface knows, and every rule for changing it.
//!
//! The interface holds no authoritative playback state: `snapshot` is the last thing the
//! daemon said and is never edited locally, so a keypress sends a command and waits for the
//! daemon to report the consequence rather than drawing what was asked for.
//!
//! Selection, focus, scroll, and filter are the interface's own, because no other client
//! has an opinion about them.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use mfp_core::model::{Catalog, Episode};
use mfp_core::protocol::{DownloadProgress, DownloadState, PlaybackState, StateSnapshot};

use super::theme::{PALETTE, Theme};

/// How long an action's confirmation stays on the footer.
const STATUS_LINGER: Duration = Duration::from_millis(2200);

/// Rows a page key moves before any pane has drawn and reported its real height.
const DEFAULT_PAGE: usize = 8;

/// What the tracks pane shows for an episode with neither a listing nor a description.
const NO_TRACKLIST: &str = "no track listing for this episode";

/// Which pane the list keys act on.
///
/// The player is drawn but never focused: its controls are global keys that work whatever
/// is focused, so giving it a focus state would only create one in which `j` does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Catalog,
    Tracks,
}

impl Focus {
    pub fn next(self) -> Self {
        match self {
            Self::Catalog => Self::Tracks,
            Self::Tracks => Self::Catalog,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Catalog => Self::Tracks,
            Self::Tracks => Self::Catalog,
        }
    }
}

/// What typing does right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    Help,
}

/// What an episode's local copy is doing, collapsed from the cache scan and the snapshot
/// into the one thing a row has to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Local {
    None,
    /// A transfer accepted but not yet finished, and how far along it is.
    Downloading(f64),
    Cached,
    Failed,
}

/// A failure worth telling the user about, carried in the header until it expires.
///
/// Only failures. An action that worked says so by changing what is on screen, and a
/// footer that flickers between a confirmation and the key legend costs the legend more
/// than the confirmation was worth.
#[derive(Debug, Clone)]
pub struct Status {
    pub text: String,
    pub at: Instant,
}

pub struct App {
    pub catalog: Catalog,
    pub snapshot: StateSnapshot,
    pub focus: Focus,
    pub mode: Mode,
    /// The live search query. Empty means unfiltered, which is not the same as inactive:
    /// the mode says whether keys are being captured.
    pub query: String,
    /// Indices into `catalog.episodes` that match `query`, in catalog order.
    pub matches: Vec<usize>,
    /// Index into `matches`, not into the catalog.
    pub selected: usize,
    /// The first visible row of the catalog pane, kept so the selection stays on screen
    /// across resizes without the list jumping.
    pub catalog_offset: usize,
    pub tracks_offset: usize,
    /// Rows each pane had the last time it drew, so a page key can move by a page.
    ///
    /// Written by the panes as they lay themselves out: the height is theirs and changes on
    /// every resize. The starting value only serves a key pressed before the first frame.
    pub catalog_height: usize,
    pub tracks_height: usize,
    /// Episodes with a complete file in the audio cache, by identifier.
    pub cached: BTreeSet<String>,
    /// Frames since launch, at [`super::anim::TICK_MS`]. Drives every moving thing.
    pub tick: u64,
    pub status: Option<Status>,
    /// The version a newer release carries, when the last version check found one. Not a
    /// [`Status`]: it is not a failure, and it never expires - an update stays available.
    pub update: Option<String>,
    /// Set when the daemon stops answering, so the interface can say so rather than
    /// silently drawing a frozen snapshot.
    pub disconnected: bool,
    pub quit: bool,
    /// The episode selected when search began, so cancelling can restore it even though
    /// filtering changed every index.
    restore: Option<String>,
}

impl App {
    pub fn new(catalog: Catalog, snapshot: StateSnapshot) -> Self {
        let mut app = Self {
            matches: (0..catalog.episodes.len()).collect(),
            catalog,
            snapshot,
            focus: Focus::Catalog,
            mode: Mode::Normal,
            query: String::new(),
            selected: 0,
            catalog_offset: 0,
            tracks_offset: 0,
            catalog_height: DEFAULT_PAGE,
            tracks_height: DEFAULT_PAGE,
            cached: BTreeSet::new(),
            tick: 0,
            status: None,
            update: None,
            disconnected: false,
            quit: false,
            restore: None,
        };
        app.rescan_cache();
        app.select_loaded();
        app
    }

    pub fn theme(&self) -> &'static Theme {
        &PALETTE
    }

    // == Reading state ==

    /// The episode the cursor is on.
    pub fn selection(&self) -> Option<&Episode> {
        let index = *self.matches.get(self.selected)?;
        self.catalog.episodes.get(index)
    }

    /// The identifier of the episode the daemon has loaded, whatever it is doing with it.
    pub fn loaded_id(&self) -> Option<&str> {
        self.snapshot.episode.as_ref().map(|ep| ep.slug.as_str())
    }

    pub fn is_favourite(&self, id: &str) -> bool {
        self.snapshot.favourites.iter().any(|slug| slug == id)
    }

    pub fn download_of(&self, id: &str) -> Option<&DownloadProgress> {
        self.snapshot
            .downloads
            .iter()
            .find(|download| download.slug == id)
    }

    /// What to draw in an episode row's local-copy column.
    ///
    /// The cache scan and the snapshot disagree in one direction only: a file on disk is
    /// cached whatever the snapshot says, because a transfer from an earlier session leaves
    /// no entry in this one's snapshot.
    pub fn local(&self, id: &str) -> Local {
        if self.cached.contains(id) {
            return Local::Cached;
        }
        match self.download_of(id) {
            Some(progress) => match progress.state {
                DownloadState::Queued => Local::Downloading(0.0),
                DownloadState::Running => {
                    let fraction = if progress.total_bytes == 0 {
                        0.0
                    } else {
                        progress.downloaded_bytes as f64 / progress.total_bytes as f64
                    };
                    Local::Downloading(fraction.clamp(0.0, 1.0))
                }
                DownloadState::Completed => Local::Cached,
                DownloadState::Failed => Local::Failed,
                DownloadState::Cancelled => Local::None,
                // a state only a newer daemon knows says nothing about a local copy
                _ => Local::None,
            },
            None => Local::None,
        }
    }

    /// The selected episode's track listing, or its description when it has none, followed by
    /// the episode's own links, and which of the two the listing is.
    ///
    /// The state layer owns this rather than the pane because scrolling needs the line count
    /// before the pane has been laid out.
    pub fn track_lines(&self) -> (&'static str, Vec<TrackLine>) {
        let Some(episode) = self.selection() else {
            return ("Tracks", Vec::new());
        };

        let (heading, mut lines): (&'static str, Vec<TrackLine>) =
            match episode.tracklist.as_deref() {
                Some(listing) if !listing.trim().is_empty() => (
                    "Tracks",
                    listing
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(|line| TrackLine::Numbered(line.to_owned()))
                        .collect(),
                ),
                _ => (
                    "Notes",
                    episode
                        .body
                        .as_deref()
                        .unwrap_or(NO_TRACKLIST)
                        .lines()
                        .map(|line| TrackLine::Numbered(line.to_owned()))
                        .collect(),
                ),
            };

        lines.extend(link_lines(episode));
        (heading, lines)
    }

    /// How many lines [`Self::track_lines`] would return, without building them.
    ///
    /// Scrolling needs the count on every keypress, the lines only when something draws, and
    /// the listing is a few hundred owned strings.
    pub fn track_line_count(&self) -> usize {
        let Some(episode) = self.selection() else {
            return 0;
        };

        let listing = match episode.tracklist.as_deref() {
            Some(listing) if !listing.trim().is_empty() => listing
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
            _ => episode
                .body
                .as_deref()
                .unwrap_or(NO_TRACKLIST)
                .lines()
                .count(),
        };

        listing + link_lines(episode).len()
    }

    /// Moves the track listing, never past its last line. The pane clamps again once it
    /// knows its own height, since that is the bound that changes on a resize.
    pub fn scroll_tracks(&mut self, delta: isize) {
        let last = self.track_line_count().saturating_sub(1);
        self.tracks_offset = self.tracks_offset.saturating_add_signed(delta).min(last);
    }

    pub fn scroll_tracks_to_end(&mut self) {
        self.tracks_offset = self.track_line_count().saturating_sub(1);
    }

    /// Rows the focused pane last drew, which is what a page key moves by. Never zero, so
    /// a page key on a pane too short to show a row still moves the cursor.
    pub fn page(&self) -> isize {
        let height = match self.focus {
            Focus::Catalog => self.catalog_height,
            Focus::Tracks => self.tracks_height,
        };
        height.clamp(1, isize::MAX as usize) as isize
    }

    /// Whether anything on screen is moving, which decides between ticking at twenty frames
    /// a second and sitting entirely still.
    ///
    /// A paused interface redraws on input and daemon events and otherwise not at all. A
    /// standing status message is deliberately not counted: it does not animate, and its
    /// expiry is noticed by the loop's own poll rather than by a tick.
    pub fn animating(&self) -> bool {
        matches!(
            self.snapshot.playback,
            PlaybackState::Playing | PlaybackState::Loading | PlaybackState::Seeking
        ) || self.snapshot.downloads.iter().any(|download| {
            matches!(
                download.state,
                DownloadState::Queued | DownloadState::Running
            )
        })
    }

    /// The position to draw, which is never past the total and never the target of a seek
    /// the daemon has not confirmed.
    pub fn display_position(&self) -> Option<f64> {
        if self.snapshot.seek_target_secs.is_some() {
            return None;
        }
        let position = self.snapshot.position_secs;
        Some(match self.snapshot.duration_secs {
            Some(total) if position > total => total,
            _ => position,
        })
    }

    // == Changing state ==

    pub fn complain(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            at: Instant::now(),
        });
    }

    /// Drops an expired status. Returns whether anything changed, so the caller only
    /// redraws when it did.
    pub fn expire_status(&mut self) -> bool {
        match &self.status {
            Some(status) if status.at.elapsed() >= STATUS_LINGER => {
                self.status = None;
                true
            }
            _ => false,
        }
    }

    /// Re-reads which episodes have a complete file on disk.
    ///
    /// Called at startup and after any command that could have changed it, not every frame:
    /// it is a directory listing, and the answer changes a handful of times per session.
    pub fn rescan_cache(&mut self) {
        self.cached = scan_cache();
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.tracks_offset = 0;
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
        self.tracks_offset = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.matches.len().saturating_sub(1);
        self.tracks_offset = 0;
    }

    /// Puts the cursor on the episode with this identifier if it is in the current match
    /// set. False when it is filtered out.
    pub fn select_id(&mut self, id: &str) -> bool {
        let found = self.matches.iter().position(|index| {
            self.catalog
                .episodes
                .get(*index)
                .is_some_and(|episode| episode.id() == id)
        });
        match found {
            Some(index) => {
                self.selected = index;
                self.tracks_offset = 0;
                true
            }
            None => false,
        }
    }

    /// Puts the cursor on whatever the daemon has loaded, if anything.
    pub fn select_loaded(&mut self) {
        if let Some(id) = self.loaded_id().map(str::to_owned) {
            self.select_id(&id);
        }
    }

    // == Search ==

    pub fn begin_search(&mut self) {
        self.restore = self.selection().map(|episode| episode.id().into_owned());
        self.query.clear();
        self.mode = Mode::Search;
        self.refilter();
    }

    /// Leaves search with the filter standing, so the narrowed list can be navigated.
    pub fn accept_search(&mut self) {
        self.restore = None;
        self.mode = Mode::Normal;
    }

    /// Leaves search, clears the filter, and puts the cursor back where it was.
    pub fn cancel_search(&mut self) {
        self.query.clear();
        self.mode = Mode::Normal;
        self.refilter();
        if let Some(id) = self.restore.take() {
            self.select_id(&id);
        }
    }

    pub fn push_query(&mut self, character: char) {
        self.query.push(character);
        self.refilter();
    }

    pub fn pop_query(&mut self) {
        self.query.pop();
        self.refilter();
    }

    pub fn clear_query(&mut self) {
        self.query.clear();
        self.refilter();
    }

    /// Deletes the last word, with any spaces before it, as `ctrl-w` does in a shell.
    pub fn pop_query_word(&mut self) {
        let trimmed = self.query.trim_end();
        // By character rather than by byte: `truncate` panics on an index that is not a
        // character boundary, and a non-breaking space is three bytes wide
        let cut = trimmed
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8());
        self.query.truncate(cut);
        self.refilter();
    }

    /// Rebuilds the match set, keeping the cursor on the same episode where that episode
    /// still matches and putting it at the top where it does not.
    pub fn refilter(&mut self) {
        let was = self.selection().map(|episode| episode.id().into_owned());
        let needle = self.query.to_lowercase();

        self.matches = self
            .catalog
            .episodes
            .iter()
            .enumerate()
            .filter(|(_, episode)| needle.is_empty() || matches(episode, &needle))
            .map(|(index, _)| index)
            .collect();

        self.selected = 0;
        self.tracks_offset = 0;
        if let Some(id) = was {
            self.select_id(&id);
        }
    }
}

/// Whether an episode matches a lowercased query, over its title and its track listing.
///
/// The track listing is what makes this worth having: the catalog is 79 episodes with
/// near-identical titles, and what a listener remembers is an artist on one of them.
fn matches(episode: &Episode, needle: &str) -> bool {
    episode.site_title().to_lowercase().contains(needle)
        || episode
            .tracklist
            .as_deref()
            .is_some_and(|tracks| tracks.to_lowercase().contains(needle))
}

/// Every episode identifier with a complete `.mp3` in the audio cache.
///
/// Read from the directory rather than from the snapshot because the snapshot lists this
/// session's transfers only, and an episode downloaded last week must still show as local.
fn scan_cache() -> BTreeSet<String> {
    let Ok(dir) = mfp_core::paths::audio_cache_dir() else {
        return BTreeSet::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeSet::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.strip_suffix(".mp3").map(str::to_owned)
        })
        .collect()
}

// == Formatting ==

/// `H:MM:SS`, or `MM:SS` under an hour. The one place a duration becomes text.
pub fn hms(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "--:--".into();
    }
    let total = seconds as u64;
    let (hours, minutes, secs) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

/// The placeholder drawn where a duration is not known, which is never `0:00:00`.
pub const UNKNOWN_DURATION: &str = "--:--";

/// The episode's catalog number, as the site writes it: `79` out of `79: Corticyte`.
pub fn number_of(episode: &Episode) -> &str {
    let title = episode.site_title();
    match title.split_once(':') {
        Some((number, _)) if number.chars().all(|c| c.is_ascii_digit()) => number,
        _ => "",
    }
}

/// The episode's name without its catalog number: `Corticyte` out of `79: Corticyte`.
pub fn name_of(episode: &Episode) -> &str {
    let title = episode.site_title();
    match title.split_once(':') {
        Some((number, name)) if number.chars().all(|c| c.is_ascii_digit()) => name.trim(),
        _ => title,
    }
}

/// One row of the tracks pane.
///
/// The pane draws each kind differently, and the state layer decides which a row is, so the
/// two cannot disagree about what a row means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackLine {
    /// A track, or a line of the description standing in for one. Numbered by the pane, which
    /// relies on these all coming before any other kind.
    Numbered(String),
    /// A spacer between the listing and the links below it.
    Blank,
    /// One of the episode's links, as the site gives it.
    Link(String),
}

/// The episode's own links - the artist's SoundCloud, Bandcamp, or homepage as the site lists
/// them under the player - as the rows that close the tracks pane.
///
/// Appended to the listing rather than given a pane of their own: an episode has one or two,
/// and a fourth pane that is empty for the episodes with none costs more room than it repays.
/// A blank row rather than a label introduces them: a URL is already unmistakably a link.
fn link_lines(episode: &Episode) -> Vec<TrackLine> {
    let Some(links) = episode.links.as_deref() else {
        return Vec::new();
    };

    let links: Vec<TrackLine> = links
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| TrackLine::Link(line.to_owned()))
        .collect();

    if links.is_empty() {
        return Vec::new();
    }

    let mut lines = vec![TrackLine::Blank];
    lines.extend(links);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(number: u32, name: &str, tracklist: Option<&str>) -> Episode {
        Episode {
            title: format!("Episode {number}: {name}"),
            link: format!("https://musicforprogramming.net/{number}"),
            enclosure_url: format!("https://datashat.net/mfp_{number}.mp3"),
            byte_len: 400_000_000,
            duration_secs: 14_400,
            published_at: 1_700_000_000,
            slug: Some(format!("ep{number}")),
            order: Some(number),
            tracklist: tracklist.map(str::to_owned),
            body: None,
            links: None,
            bundle_title: Some(format!("{number}: {name}")),
            special: false,
        }
    }

    fn app() -> App {
        let catalog = Catalog {
            episodes: vec![
                episode(79, "Corticyte", Some("Datassette - Something")),
                episode(78, "Mindaugas", Some("Loscil - Sea Island")),
                episode(77, "Julien Mier", None),
            ],
            info: Vec::new(),
            fetched_at: 0,
            enriched: true,
        };
        App::new(catalog, StateSnapshot::stopped())
    }

    #[test]
    fn a_fresh_app_matches_every_episode() {
        assert_eq!(app().matches.len(), 3);
    }

    #[test]
    fn searching_narrows_by_title() {
        let mut app = app();
        app.begin_search();
        for character in "cortic".chars() {
            app.push_query(character);
        }
        assert_eq!(app.matches.len(), 1);
        assert_eq!(number_of(app.selection().unwrap()), "79");
    }

    #[test]
    fn searching_narrows_by_track_listing() {
        let mut app = app();
        app.begin_search();
        for character in "loscil".chars() {
            app.push_query(character);
        }
        assert_eq!(app.matches.len(), 1);
        assert_eq!(number_of(app.selection().unwrap()), "78");
    }

    #[test]
    fn search_is_case_insensitive() {
        let mut app = app();
        app.begin_search();
        for character in "LOSCIL".chars() {
            app.push_query(character);
        }
        assert_eq!(app.matches.len(), 1);
    }

    #[test]
    fn cancelling_search_restores_the_list_and_the_selection() {
        let mut app = app();
        app.move_selection(2);
        let before = app.selection().unwrap().id().into_owned();

        app.begin_search();
        for character in "cortic".chars() {
            app.push_query(character);
        }
        assert_eq!(app.matches.len(), 1);

        app.cancel_search();
        assert_eq!(app.matches.len(), 3);
        assert_eq!(app.selection().unwrap().id(), before);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn accepting_search_keeps_the_filter() {
        let mut app = app();
        app.begin_search();
        app.push_query('c');
        app.accept_search();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.matches.len() < 3);
    }

    #[test]
    fn a_query_matching_nothing_leaves_an_empty_match_set_and_no_selection() {
        let mut app = app();
        app.begin_search();
        for character in "zzzz".chars() {
            app.push_query(character);
        }
        assert!(app.matches.is_empty());
        assert!(app.selection().is_none());
        app.move_selection(1);
        assert!(
            app.selection().is_none(),
            "moving in an empty list must not panic"
        );
    }

    #[test]
    fn deleting_a_character_widens_the_filter_again() {
        let mut app = app();
        app.begin_search();
        for character in "corticz".chars() {
            app.push_query(character);
        }
        assert!(app.matches.is_empty());
        app.pop_query();
        assert_eq!(app.matches.len(), 1);
    }

    #[test]
    fn selection_is_clamped_at_both_ends() {
        let mut app = app();
        app.move_selection(-5);
        assert_eq!(app.selected, 0);
        app.move_selection(50);
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn a_position_past_an_approximate_total_is_clamped_to_it() {
        let mut app = app();
        app.snapshot.position_secs = 3661.0;
        app.snapshot.duration_secs = Some(3600.0);
        assert_eq!(app.display_position(), Some(3600.0));
    }

    #[test]
    fn an_in_flight_seek_suppresses_the_position_rather_than_drifting() {
        let mut app = app();
        app.snapshot.position_secs = 600.0;
        app.snapshot.seek_target_secs = Some(5400.0);
        assert_eq!(app.display_position(), None);
    }

    #[test]
    fn a_resting_interface_is_not_animating() {
        let app = app();
        assert!(!app.animating());
    }

    #[test]
    fn a_standing_status_message_does_not_make_a_resting_interface_tick() {
        let mut app = app();
        app.complain("this episode cannot be sought yet");
        assert!(!app.animating());
    }

    #[test]
    fn a_playing_interface_is_animating() {
        let mut app = app();
        app.snapshot.playback = PlaybackState::Playing;
        assert!(app.animating());
    }

    #[test]
    fn a_running_download_keeps_a_paused_interface_animating() {
        let mut app = app();
        app.snapshot.downloads.push(DownloadProgress {
            slug: "ep79".into(),
            downloaded_bytes: 10,
            total_bytes: 100,
            state: DownloadState::Running,
            error: None,
        });
        assert!(app.animating());
        assert_eq!(app.local("ep79"), Local::Downloading(0.1));
    }

    #[test]
    fn a_file_on_disk_outranks_a_snapshot_that_never_heard_of_it() {
        let mut app = app();
        app.cached.insert("ep79".into());
        assert_eq!(app.local("ep79"), Local::Cached);
        assert_eq!(app.local("ep78"), Local::None);
    }

    #[test]
    fn durations_are_formatted_with_hours_only_when_there_are_some() {
        assert_eq!(hms(0.0), "00:00");
        assert_eq!(hms(61.0), "01:01");
        assert_eq!(hms(3661.0), "1:01:01");
        assert_eq!(hms(f64::NAN), UNKNOWN_DURATION);
        assert_eq!(hms(-1.0), UNKNOWN_DURATION);
    }

    #[test]
    fn a_title_splits_into_a_number_and_a_name() {
        let episode = episode(79, "Corticyte", None);
        assert_eq!(number_of(&episode), "79");
        assert_eq!(name_of(&episode), "Corticyte");
    }

    #[test]
    fn a_title_with_no_number_keeps_all_of_itself_as_the_name() {
        let mut episode = episode(79, "Corticyte", None);
        episode.bundle_title = Some("Datassette Special".into());
        assert_eq!(number_of(&episode), "");
        assert_eq!(name_of(&episode), "Datassette Special");
    }

    #[test]
    fn the_track_listing_comes_from_the_tracklist_when_there_is_one() {
        let app = app();
        let (heading, lines) = app.track_lines();
        assert_eq!(heading, "Tracks");
        assert_eq!(
            lines,
            vec![TrackLine::Numbered("Datassette - Something".to_owned())]
        );
    }

    #[test]
    fn an_episode_with_no_tracklist_falls_back_to_its_description() {
        let mut app = app();
        app.move_selection(2);
        let (heading, lines) = app.track_lines();
        assert_eq!(heading, "Notes");
        assert_eq!(
            lines,
            vec![TrackLine::Numbered(
                "no track listing for this episode".to_owned()
            )]
        );
    }

    #[test]
    fn scrolling_the_track_listing_stops_at_its_last_line() {
        let mut app = app();
        app.scroll_tracks(50);
        assert_eq!(app.tracks_offset, 0, "a one-line listing cannot scroll");

        app.catalog.episodes[0].tracklist = Some(
            (0..9)
                .map(|n| format!("track {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        app.scroll_tracks(50);
        assert_eq!(app.tracks_offset, 8);
        app.scroll_tracks(-3);
        assert_eq!(app.tracks_offset, 5);
        app.scroll_tracks(-100);
        assert_eq!(
            app.tracks_offset, 0,
            "scrolling up past the top must not wrap"
        );
        app.scroll_tracks_to_end();
        assert_eq!(app.tracks_offset, 8);
    }

    #[test]
    fn moving_the_cursor_returns_the_track_listing_to_its_top() {
        let mut app = app();
        app.tracks_offset = 4;
        app.move_selection(1);
        assert_eq!(app.tracks_offset, 0);
    }

    #[test]
    fn focus_cycles_between_the_two_scrollable_panes() {
        assert_eq!(Focus::Catalog.next(), Focus::Tracks);
        assert_eq!(Focus::Tracks.next(), Focus::Catalog);
    }

    #[test]
    fn focus_cycles_backwards_as_well() {
        assert_eq!(Focus::Catalog.previous(), Focus::Tracks);
        assert_eq!(Focus::Tracks.previous(), Focus::Catalog);
    }

    #[test]
    fn a_page_is_the_focused_panes_own_height() {
        let mut app = app();
        app.catalog_height = 40;
        app.tracks_height = 7;

        assert_eq!(app.page(), 40);
        app.focus = Focus::Tracks;
        assert_eq!(app.page(), 7);
    }

    #[test]
    fn a_page_in_a_pane_too_short_to_show_a_row_still_moves_the_cursor() {
        let mut app = app();
        app.catalog_height = 0;
        assert_eq!(app.page(), 1);
    }

    #[test]
    fn an_episode_with_links_lists_them_under_its_listing() {
        let mut app = app();
        app.catalog.episodes[0].links =
            Some("https://soundcloud.com/matt-whitehead\n\nhttps://www.instagram.com/x/".into());

        let (_, lines) = app.track_lines();
        assert_eq!(
            lines,
            vec![
                TrackLine::Numbered("Datassette - Something".to_owned()),
                TrackLine::Blank,
                TrackLine::Link("https://soundcloud.com/matt-whitehead".to_owned()),
                TrackLine::Link("https://www.instagram.com/x/".to_owned()),
            ]
        );
    }

    #[test]
    fn the_track_line_count_matches_the_lines_it_would_build() {
        let mut app = app();
        for _ in 0..3 {
            assert_eq!(app.track_line_count(), app.track_lines().1.len());
            app.move_selection(1);
        }
    }

    #[test]
    fn clearing_the_query_widens_the_filter_back_to_everything() {
        let mut app = app();
        app.begin_search();
        for character in "cortic".chars() {
            app.push_query(character);
        }
        assert_eq!(app.matches.len(), 1);

        app.clear_query();

        assert!(app.query.is_empty());
        assert_eq!(app.matches.len(), 3);
    }

    #[test]
    fn deleting_a_word_takes_the_spaces_before_it_too() {
        let mut app = app();
        app.begin_search();
        for character in "sea island".chars() {
            app.push_query(character);
        }

        app.pop_query_word();

        assert_eq!(app.query, "sea ");
    }

    #[test]
    fn deleting_a_word_from_a_single_word_query_empties_it() {
        let mut app = app();
        app.begin_search();
        for character in "loscil".chars() {
            app.push_query(character);
        }

        app.pop_query_word();

        assert_eq!(app.query, "");
        assert_eq!(app.matches.len(), 3);
    }

    /// `String::truncate` panics on an index that is not a character boundary, and the
    /// separator a query is cut at need not be one byte wide.
    #[test]
    fn deleting_a_word_across_a_multibyte_space_is_not_a_panic() {
        let mut app = app();
        app.begin_search();
        for character in "sea\u{a0}island".chars() {
            app.push_query(character);
        }

        app.pop_query_word();

        assert_eq!(app.query, "sea\u{a0}");
    }
}
