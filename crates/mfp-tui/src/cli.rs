//! Argument parsing and the headless transport commands.
//!
//! A subcommand acts, prints one human-readable line to stdout, and exits: no alternate
//! screen, no raw mode, nothing drawn. Diagnostics go to stderr so stdout stays pipeable.

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mfp_core::protocol::{Command, PlaybackState, StateSnapshot};

use crate::client::{Client, ClientError};

pub const EXIT_OK: u8 = 0;

/// The daemon was reached but rejected the command or could not perform it.
pub const EXIT_REJECTED: u8 = 1;

/// Usage error: unknown subcommand, missing argument, or unparseable argument.
pub const EXIT_USAGE: u8 = 2;

/// The daemon was unreachable and could not be started.
pub const EXIT_UNREACHABLE: u8 = 3;

#[derive(Debug, Parser)]
#[command(
    name = "mfp",
    version,
    about = "The musicforprogramming.net terminal player"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Resume playback, or start the named episode
    Play { slug: Option<String> },
    /// Pause playback
    Pause,
    /// Toggle between playing and paused
    Toggle,
    /// Stop playback and unload the episode
    Stop,
    /// Play the next episode
    Next,
    /// Play the previous episode
    Prev,
    /// Seek by a relative offset such as +30 or -30, or to an absolute H:MM:SS
    Seek {
        #[arg(allow_hyphen_values = true)]
        target: String,
    },
    /// Start downloading the named episode and return without waiting for it
    Download { slug: String },
    /// Stop an in-progress download of the named episode
    CancelDownload { slug: String },
    /// Delete the local copy of the named episode
    Delete { slug: String },
    /// List the catalog
    List {
        #[arg(long)]
        json: bool,
    },
    /// Report the current playback state
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Stop playback and shut down the daemon
    Shutdown,
}

/// Parses argv and either runs a headless subcommand or launches the interface.
///
/// Everything below returns a plain `u8` so tests can assert the exit code for every
/// outcome directly; this is the one place that converts to [`ExitCode`].
pub fn run() -> ExitCode {
    ExitCode::from(run_code(std::env::args_os()))
}

fn run_code<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match Cli::try_parse_from(args) {
        Ok(cli) => match cli.command {
            None => launch_interface(),
            Some(command) => dispatch(command),
        },
        Err(error) => {
            use clap::error::ErrorKind;
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                print!("{error}");
                EXIT_OK
            } else {
                eprint!("{error}");
                EXIT_USAGE
            }
        }
    }
}

fn launch_interface() -> u8 {
    match Client::connect() {
        Ok(client) => match crate::ui::run(client) {
            Ok(()) => EXIT_OK,
            Err(error) => {
                eprintln!("{error}");
                1
            }
        },
        Err(error) => report_client_error(&error),
    }
}

fn dispatch(command: Commands) -> u8 {
    match command {
        Commands::Play { slug } => run_transport(Command::Play { slug }, "Playing."),
        Commands::Pause => run_simple(Command::Pause, "Paused."),
        Commands::Toggle => run_transport(Command::Toggle, "Toggled."),
        Commands::Stop => run_simple(Command::Stop, "Stopped."),
        Commands::Next => run_transport(Command::Next, "Playing the next episode."),
        Commands::Prev => run_transport(Command::Previous, "Playing the previous episode."),
        Commands::Seek { target } => match parse_seek(&target) {
            Ok(arg) => {
                let (command, message) = seek_command(arg);
                run_simple(command, &message)
            }
            Err(message) => usage_error(&message),
        },
        Commands::Download { slug } => {
            let message = format!("Downloading {slug}.");
            run_simple(Command::Download { slug }, &message)
        }
        Commands::CancelDownload { slug } => {
            let message = format!("Cancelled the download of {slug}.");
            run_simple(Command::CancelDownload { slug }, &message)
        }
        Commands::Delete { slug } => {
            let message = format!("Deleted the local copy of {slug}.");
            run_simple(Command::DeleteDownload { slug }, &message)
        }
        Commands::List { json } => run_list(json),
        Commands::Status { json } => run_status(json),
        Commands::Shutdown => run_simple(Command::Shutdown, "Shutting down."),
    }
}

fn usage_error(message: &str) -> u8 {
    eprintln!("{message}");
    EXIT_USAGE
}

/// Reports a [`ClientError`] to stderr and returns the exit code it maps to.
fn report_client_error(error: &ClientError) -> u8 {
    match error {
        ClientError::Unreachable(message) => {
            eprintln!("the daemon is unreachable: {message}");
            EXIT_UNREACHABLE
        }
        ClientError::Rejected(object) => {
            eprintln!("{}", object.message);
            EXIT_REJECTED
        }
    }
}

fn connect() -> Result<Client, u8> {
    Client::connect().map_err(|error| report_client_error(&error))
}

/// Sends `command` and prints `message` on success, needing nothing back from the daemon.
fn run_simple(command: Command, message: &str) -> u8 {
    let mut client = match connect() {
        Ok(client) => client,
        Err(code) => return code,
    };
    match client.request(command) {
        Ok(_) => {
            println!("{message}");
            EXIT_OK
        }
        Err(error) => report_client_error(&error),
    }
}

/// Sends `command`, then reports the playback state a follow-up `status` returns, or
/// `fallback` when that follow-up fails.
fn run_transport(command: Command, fallback: &str) -> u8 {
    let mut client = match connect() {
        Ok(client) => client,
        Err(code) => return code,
    };
    match client.request(command) {
        Ok(_) => {
            let message = client
                .status()
                .ok()
                .map(|state| describe_state(&state))
                .unwrap_or_else(|| fallback.to_string());
            println!("{message}");
            EXIT_OK
        }
        Err(error) => report_client_error(&error),
    }
}

fn run_status(json: bool) -> u8 {
    let mut client = match Client::connect() {
        Ok(client) => client,
        Err(error) => return report_status_error(&error, json),
    };
    match client.status() {
        Ok(state) => {
            if json {
                match serde_json::to_string(&state) {
                    Ok(line) => println!("{line}"),
                    Err(error) => return report_serialisation_error(&error, "status"),
                }
            } else {
                println!("{}", format_status_line(&state));
            }
            EXIT_OK
        }
        Err(error) => report_status_error(&error, json),
    }
}

/// `list` asks the daemon rather than its cache file, so the listing is what the daemon
/// serves from. A daemon with no catalog answers `catalog_unavailable`, a rejection rather
/// than an unreachable daemon.
fn run_list(json: bool) -> u8 {
    let mut client = match Client::connect() {
        Ok(client) => client,
        Err(error) => return report_status_error(&error, json),
    };

    match client.catalog() {
        Ok(catalog) => {
            if json {
                match serde_json::to_string(&catalog) {
                    Ok(line) => println!("{line}"),
                    Err(error) => return report_serialisation_error(&error, "the catalog"),
                }
            } else {
                for episode in &catalog.episodes {
                    println!("{}", format_catalog_line(episode));
                }
            }
            EXIT_OK
        }
        Err(error) => report_status_error(&error, json),
    }
}

/// Reports a [`ClientError`] for `status`/`list`, where an unreachable daemon must still
/// emit a valid JSON document on stdout under `--json`.
fn report_status_error(error: &ClientError, json: bool) -> u8 {
    match error {
        ClientError::Unreachable(message) => {
            if json {
                println!("{}", unreachable_json(message));
            } else {
                eprintln!("the daemon is unreachable: {message}");
            }
            EXIT_UNREACHABLE
        }
        ClientError::Rejected(object) => {
            eprintln!("{}", object.message);
            EXIT_REJECTED
        }
    }
}

/// Reports a `--json` document that could not be built. Nothing goes to stdout, so no
/// caller parses half a document as a whole one.
fn report_serialisation_error(error: &serde_json::Error, what: &str) -> u8 {
    eprintln!("cannot render {what} as JSON: {error}");
    EXIT_REJECTED
}

/// The JSON document `--json` emits for an unreachable daemon.
fn unreachable_json(message: &str) -> String {
    serde_json::json!({ "unreachable": true, "error": message }).to_string()
}

/// Either a relative offset in seconds or an absolute position in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SeekArg {
    Relative(f64),
    Absolute(f64),
}

/// Parses a `seek` argument: a signed relative offset such as `+30`/`-30`, or an absolute
/// `H:MM:SS` timestamp.
fn parse_seek(raw: &str) -> Result<SeekArg, String> {
    if raw.starts_with('+') || raw.starts_with('-') {
        return raw
            .parse::<f64>()
            .ok()
            // `f64::from_str` accepts `+inf` and `+NaN`, which are not offsets and which
            // the daemon would have to refuse across the socket
            .filter(|offset| offset.is_finite())
            .map(SeekArg::Relative)
            .ok_or_else(|| format!("'{raw}' is not a valid seek offset"));
    }
    parse_timestamp(raw)
        .map(SeekArg::Absolute)
        .ok_or_else(|| format!("'{raw}' is not a valid seek offset or H:MM:SS timestamp"))
}

/// Parses an `H:MM:SS` timestamp into seconds. Minutes and seconds must each fall in
/// `0..60`, and the whole must be a finite non-negative number of seconds.
fn parse_timestamp(raw: &str) -> Option<f64> {
    let [hours, minutes, seconds]: [&str; 3] =
        raw.split(':').collect::<Vec<_>>().try_into().ok()?;
    let hours: f64 = hours.parse().ok()?;
    let minutes: f64 = minutes.parse().ok()?;
    let seconds: f64 = seconds.parse().ok()?;
    if !(0.0..60.0).contains(&minutes) || !(0.0..60.0).contains(&seconds) {
        return None;
    }
    // The minute and second range checks reject non-finite values, but the hour has no
    // upper bound, so `inf:00:00` and `NaN:00:00` would otherwise parse
    let total = hours * 3600.0 + minutes * 60.0 + seconds;
    (total.is_finite() && total >= 0.0).then_some(total)
}

fn seek_command(arg: SeekArg) -> (Command, String) {
    match arg {
        SeekArg::Relative(delta) => (
            Command::Seek {
                position_secs: None,
                delta_secs: Some(delta),
            },
            format!("Seek by {delta:+.0}s requested."),
        ),
        SeekArg::Absolute(position) => (
            Command::Seek {
                position_secs: Some(position),
                delta_secs: None,
            },
            format!("Seek to {} requested.", format_hms(position)),
        ),
    }
}

/// A one-line description of a snapshot's transport state, for commands whose outcome the
/// daemon's `{"type":"ok"}` cannot name: it carries neither the resulting episode nor state.
fn describe_state(state: &StateSnapshot) -> String {
    match &state.episode {
        None => "Nothing is loaded.".to_string(),
        Some(episode) => {
            let verb = match state.playback {
                PlaybackState::Playing => "Playing",
                PlaybackState::Paused => "Paused",
                PlaybackState::Loading => "Loading",
                PlaybackState::Seeking => "Seeking within",
                PlaybackState::Stopped => "Stopped",
                PlaybackState::Error => "Playback error on",
                _ => "In an unknown state",
            };
            format!("{verb}: {}", episode.title)
        }
    }
}

/// The human-readable `status` line.
fn format_status_line(state: &StateSnapshot) -> String {
    let Some(episode) = &state.episode else {
        return "Stopped. Nothing loaded.".to_owned();
    };

    let elapsed = format_hms(state.position_secs);
    let total = match state.duration_secs {
        Some(total) if state.duration_approximate => format!("~{}", format_hms(total)),
        Some(total) => format_hms(total),
        None => "unknown".to_string(),
    };
    let seek_note = match state.seek_target_secs {
        Some(target) => format!(", seeking to {}", format_hms(target)),
        None => String::new(),
    };
    let verb = match state.playback {
        PlaybackState::Playing => "Playing",
        PlaybackState::Paused => "Paused",
        PlaybackState::Loading => "Loading",
        PlaybackState::Seeking => "Seeking",
        PlaybackState::Stopped => "Stopped",
        PlaybackState::Error => "Error",
        _ => "Unknown",
    };
    format!("{verb}: {} - {elapsed}/{total}{seek_note}", episode.title)
}

fn format_catalog_line(episode: &mfp_core::Episode) -> String {
    format!(
        "{}  {}  ({})",
        episode.id(),
        episode.title,
        format_hms(episode.duration_secs as f64)
    )
}

/// Formats seconds as `H:MM:SS`, clamped to non-negative.
fn format_hms(secs: f64) -> String {
    let total_seconds = secs.max(0.0).round() as u64;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use mfp_core::ErrorCode;
    use mfp_core::protocol::{DownloadProgress, DownloadState, EpisodeRef, ErrorObject, Source};

    use super::*;

    fn snapshot() -> StateSnapshot {
        let mut snapshot = StateSnapshot::stopped();
        snapshot.playback = PlaybackState::Playing;
        snapshot.episode = Some(EpisodeRef {
            slug: "seventynine".into(),
            title: "Episode 79".into(),
            duration_secs: 14_400.0,
        });
        snapshot.position_secs = 3_900.0;
        snapshot.duration_secs = Some(14_400.0);
        snapshot.duration_approximate = true;
        snapshot.seekable = true;
        snapshot.source = Some(Source::Stream);
        snapshot.downloads = vec![DownloadProgress {
            slug: "seventyeight".into(),
            downloaded_bytes: 1024,
            total_bytes: 4096,
            state: DownloadState::Running,
            error: None,
        }];
        snapshot
    }

    // == Clap Surface ==

    // Every subcommand parses to the shape the spec table names

    #[test]
    fn every_subcommand_parses() {
        let cases: &[&[&str]] = &[
            &["mfp", "play"],
            &["mfp", "play", "seventynine"],
            &["mfp", "pause"],
            &["mfp", "toggle"],
            &["mfp", "stop"],
            &["mfp", "next"],
            &["mfp", "prev"],
            &["mfp", "seek", "+30"],
            &["mfp", "seek", "-30"],
            &["mfp", "seek", "1:30:00"],
            &["mfp", "download", "seventynine"],
            &["mfp", "cancel-download", "seventynine"],
            &["mfp", "delete", "seventynine"],
            &["mfp", "list"],
            &["mfp", "list", "--json"],
            &["mfp", "status"],
            &["mfp", "status", "--json"],
            &["mfp", "shutdown"],
        ];
        for args in cases {
            Cli::try_parse_from(*args).unwrap_or_else(|e| panic!("{args:?} failed: {e}"));
        }
    }

    #[test]
    fn delete_sends_the_eviction_command_rather_than_touching_the_cache() {
        let Commands::Delete { slug } = Cli::try_parse_from(["mfp", "delete", "seventynine"])
            .unwrap()
            .command
            .unwrap()
        else {
            panic!("delete did not parse as the delete subcommand");
        };
        assert_eq!(slug, "seventynine");
    }

    #[test]
    fn delete_without_a_slug_is_a_usage_error() {
        assert_eq!(run_code(["mfp", "delete"]), EXIT_USAGE);
    }

    #[test]
    fn no_subcommand_launches_the_interface() {
        let cli = Cli::try_parse_from(["mfp"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn an_unknown_subcommand_is_a_usage_error() {
        let error = Cli::try_parse_from(["mfp", "teleport"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    // == Exit Codes ==

    // Each asserted directly, none touching a daemon

    #[test]
    fn an_unknown_subcommand_exits_with_the_usage_code() {
        assert_eq!(run_code(["mfp", "teleport"]), EXIT_USAGE);
    }

    #[test]
    fn a_missing_required_argument_exits_with_the_usage_code() {
        assert_eq!(run_code(["mfp", "seek"]), EXIT_USAGE);
    }

    #[test]
    fn an_unparseable_seek_argument_exits_with_the_usage_code() {
        assert_eq!(run_code(["mfp", "seek", "banana"]), EXIT_USAGE);
    }

    #[test]
    fn a_rejected_command_exits_with_the_rejected_code() {
        let error = ClientError::Rejected(ErrorObject {
            code: ErrorCode::NotPlaying,
            message: "nothing is loaded".into(),
        });
        assert_eq!(report_client_error(&error), EXIT_REJECTED);
        assert_eq!(report_status_error(&error, false), EXIT_REJECTED);
        assert_eq!(report_status_error(&error, true), EXIT_REJECTED);
    }

    #[test]
    fn an_unreachable_daemon_exits_with_the_unreachable_code() {
        let error = ClientError::Unreachable("the daemon could not be started".into());
        assert_eq!(report_client_error(&error), EXIT_UNREACHABLE);
        assert_eq!(report_status_error(&error, false), EXIT_UNREACHABLE);
        assert_eq!(report_status_error(&error, true), EXIT_UNREACHABLE);
    }

    #[test]
    fn a_missing_required_argument_is_a_usage_error() {
        let error = Cli::try_parse_from(["mfp", "seek"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    // == Seek Parsing ==

    #[test]
    fn seek_accepts_a_positive_relative_offset() {
        assert_eq!(parse_seek("+30"), Ok(SeekArg::Relative(30.0)));
    }

    #[test]
    fn seek_accepts_a_negative_relative_offset() {
        assert_eq!(parse_seek("-30"), Ok(SeekArg::Relative(-30.0)));
    }

    #[test]
    fn seek_accepts_an_absolute_timestamp() {
        assert_eq!(parse_seek("1:30:00"), Ok(SeekArg::Absolute(5_400.0)));
    }

    #[test]
    fn seek_rejects_a_malformed_argument_rather_than_panicking() {
        assert!(parse_seek("banana").is_err());
        assert!(parse_seek("30").is_err());
        assert!(parse_seek("1:70:00").is_err());
        assert!(parse_seek("1:00:70").is_err());
        assert!(parse_seek("1:30").is_err());
    }

    /// `f64::from_str` accepts these, so the parser has to refuse them itself rather than
    /// send a position the daemon cannot turn into a `Duration`.
    #[test]
    fn seek_rejects_a_target_that_is_not_a_finite_number_of_seconds() {
        for raw in [
            "inf:00:00",
            "infinity:00:00",
            "-inf:00:00",
            "NaN:00:00",
            "+inf",
            "+NaN",
            "-inf",
        ] {
            assert!(parse_seek(raw).is_err(), "'{raw}' was accepted");
        }
    }

    #[test]
    fn seek_command_labels_name_the_target() {
        let (command, message) = seek_command(SeekArg::Relative(-30.0));
        assert_eq!(
            command,
            Command::Seek {
                position_secs: None,
                delta_secs: Some(-30.0)
            }
        );
        assert_eq!(message, "Seek by -30s requested.");

        let (command, message) = seek_command(SeekArg::Absolute(5_400.0));
        assert_eq!(
            command,
            Command::Seek {
                position_secs: Some(5_400.0),
                delta_secs: None
            }
        );
        assert_eq!(message, "Seek to 1:30:00 requested.");
    }

    // == JSON Shape ==

    #[test]
    fn status_json_is_a_single_valid_document() {
        let json = serde_json::to_string(&snapshot()).unwrap();
        assert!(!json.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        for field in [
            "playback",
            "episode",
            "position_secs",
            "seek_target_secs",
            "duration_secs",
            "duration_approximate",
            "source",
            "downloads",
        ] {
            assert!(value.get(field).is_some(), "missing {field}");
        }
    }

    #[test]
    fn status_json_during_a_seek_reports_the_target_not_the_current_position() {
        let mut state = snapshot();
        state.playback = PlaybackState::Seeking;
        state.seek_target_secs = Some(5_400.0);
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["playback"], "seeking");
        assert_eq!(value["seek_target_secs"], 5_400.0);
    }

    #[test]
    fn unreachable_json_is_valid_and_describes_the_state() {
        let json = unreachable_json("the daemon could not be started");
        assert!(!json.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["unreachable"], true);
        assert_eq!(value["error"], "the daemon could not be started");
    }

    // == Human-Readable Formatting ==

    #[test]
    fn format_hms_formats_hours_minutes_seconds() {
        assert_eq!(format_hms(3_900.0), "1:05:00");
        assert_eq!(format_hms(0.0), "0:00:00");
    }

    #[test]
    fn format_hms_never_goes_negative() {
        assert_eq!(format_hms(-5.0), "0:00:00");
    }

    #[test]
    fn describe_state_names_nothing_loaded() {
        assert_eq!(
            describe_state(&StateSnapshot::stopped()),
            "Nothing is loaded."
        );
    }

    #[test]
    fn describe_state_names_the_loaded_episode() {
        assert_eq!(describe_state(&snapshot()), "Playing: Episode 79");
    }

    #[test]
    fn status_line_reports_an_approximate_total() {
        let line = format_status_line(&snapshot());
        assert!(line.contains("~4:00:00"), "{line}");
    }

    #[test]
    fn status_line_reports_an_unknown_total_as_a_placeholder_not_zero() {
        let mut state = snapshot();
        state.duration_secs = None;
        let line = format_status_line(&state);
        assert!(line.contains("unknown"), "{line}");
        assert!(!line.contains("0:00:00/"), "{line}");
    }

    #[test]
    fn status_line_reports_nothing_loaded() {
        let line = format_status_line(&StateSnapshot::stopped());
        assert!(line.starts_with("Stopped. Nothing loaded."), "{line}");
    }

    #[test]
    fn catalog_line_names_id_title_and_duration() {
        let episode = mfp_core::Episode {
            bundle_title: None,
            special: false,
            title: "Episode 79".into(),
            link: "https://musicforprogramming.net/79/".into(),
            enclosure_url: "https://example.com/79.mp3".into(),
            byte_len: 1,
            duration_secs: 3_900,
            published_at: 0,
            slug: Some("seventynine".into()),
            order: None,
            tracklist: None,
            body: None,
            links: None,
        };
        let line = format_catalog_line(&episode);
        assert!(line.contains("seventynine"), "{line}");
        assert!(line.contains("Episode 79"), "{line}");
        assert!(line.contains("1:05:00"), "{line}");
    }
}
