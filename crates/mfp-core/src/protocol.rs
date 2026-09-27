//! The newline-delimited JSON wire protocol. This module is its definition.
//!
//! One JSON value per line in both directions. A client writes [`Request`] lines and reads
//! [`Frame`] lines, each either a [`Response`] echoing a request's `id` or an unsolicited
//! [`EventFrame`] carrying a full [`StateSnapshot`].
//!
//! Field names and value spellings are the contract: new fields may be added and readers
//! must ignore ones they do not recognise, but nothing already named may be renamed or
//! respelled.

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode, Result};
use crate::model::Catalog;

/// One request line: `{"id":1,"cmd":{"type":"pause"}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// A client-chosen number the response echoes.
    pub id: i64,
    pub cmd: Command,
}

/// The command surface the daemon accepts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Start the named episode, or resume the loaded one when no slug is given.
    Play {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slug: Option<String>,
    },
    Pause,
    Toggle,
    Stop,
    /// Seek to an absolute position or by a signed offset. Exactly one of the two.
    Seek {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position_secs: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delta_secs: Option<f64>,
    },
    Next,
    Previous,
    /// Begin or resume an offline download; returns before the transfer finishes.
    Download {
        slug: String,
    },
    CancelDownload {
        slug: String,
    },
    /// Remove an episode's local copy, so no client has to unlink a file the daemon is
    /// accounting for.
    DeleteDownload {
        slug: String,
    },
    /// Mark an episode a favourite. Marking one already marked is not an error.
    Favourite {
        slug: String,
    },
    /// Unmark a favourite. Unmarking one not marked is not an error.
    Unfavourite {
        slug: String,
    },
    ListFavourites,
    /// Read the interface's display preferences, and set whichever of them are given.
    ///
    /// One command rather than a read and a write: an omitted field is left alone, so a
    /// read is this command with the field absent and a client never has to read a
    /// preference back before changing it. These belong to the interface, not to playback,
    /// and setting one has no effect on what the daemon is playing.
    ///
    /// Unknown fields are ignored, so a client still sending the cookie notice's
    /// dismissal - a preference this interface no longer has - is answered rather than
    /// refused.
    Preferences {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inverted_palette: Option<bool>,
    },
    Status,
    /// Return the catalog the daemon holds, so no client has to read its cache file.
    ListCatalog,
    /// Start receiving pushed state events on this connection.
    Subscribe {
        /// Whether this connection wants snapshots pushed for spectrum changes.
        ///
        /// Defaults to false so a client that does not draw an analyser is never woken
        /// twenty times a second for a field it ignores.
        #[serde(default)]
        spectrum: bool,
    },
    /// Stop audio, persist state, and exit. The only command that ends the process.
    Shutdown,
}

/// Every `cmd.type` spelling the daemon recognises.
///
/// Used to tell an unrecognised command from a recognised one with bad arguments, which
/// the spec answers with different codes.
pub const COMMAND_TYPES: &[&str] = &[
    "play",
    "pause",
    "toggle",
    "stop",
    "seek",
    "next",
    "previous",
    "download",
    "cancel_download",
    "delete_download",
    "favourite",
    "unfavourite",
    "list_favourites",
    "preferences",
    "status",
    "list_catalog",
    "subscribe",
    "shutdown",
];

impl Command {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Play { .. } => "play",
            Self::Pause => "pause",
            Self::Toggle => "toggle",
            Self::Stop => "stop",
            Self::Seek { .. } => "seek",
            Self::Next => "next",
            Self::Previous => "previous",
            Self::Download { .. } => "download",
            Self::CancelDownload { .. } => "cancel_download",
            Self::DeleteDownload { .. } => "delete_download",
            Self::Favourite { .. } => "favourite",
            Self::Unfavourite { .. } => "unfavourite",
            Self::ListFavourites => "list_favourites",
            Self::Preferences { .. } => "preferences",
            Self::Status => "status",
            Self::ListCatalog => "list_catalog",
            Self::Subscribe { .. } => "subscribe",
            Self::Shutdown => "shutdown",
        }
    }

    /// Checks arguments that parsed structurally but must still fall inside their permitted
    /// domain.
    ///
    /// `seek` takes exactly one of `position_secs` or `delta_secs`, and that one must be a
    /// duration the engine can hold. Anything else is [`ErrorCode::InvalidParams`].
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Seek {
                position_secs,
                delta_secs,
            } => match (position_secs, delta_secs) {
                (Some(position), None) => seek_within_range(*position, 0.0),
                (None, Some(delta)) => seek_within_range(*delta, -SEEK_SECS_LIMIT),
                _ => Err(Error::InvalidParams(
                    "seek takes exactly one of position_secs or delta_secs".into(),
                )),
            },
            _ => Ok(()),
        }
    }
}

/// The exclusive bound on a seek, in seconds.
///
/// A seek becomes a [`std::time::Duration`] in the audio engine, and
/// `Duration::from_secs_f64` panics at or above 2^64 seconds and on a NaN, taking the audio
/// thread down for the rest of the process. The domain is refused here instead.
const SEEK_SECS_LIMIT: f64 = u64::MAX as f64;

/// Whether a seek argument is a duration the engine can hold.
fn seek_within_range(value: f64, minimum: f64) -> Result<()> {
    // both comparisons are false for a NaN, which is the point: it is as fatal to the
    // engine as an out-of-range magnitude
    if value >= minimum && value < SEEK_SECS_LIMIT {
        return Ok(());
    }
    Err(Error::InvalidParams(format!(
        "{value} is not a seekable number of seconds"
    )))
}

/// One response line. Carries exactly one of `result` or `error`, and echoes the
/// request's `id`, or `null` when the id could not be recovered from the line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    /// A successful response carrying `{"type":"ok"}`.
    pub fn ok(id: i64) -> Self {
        Self {
            id: Some(id),
            result: Some(ResultBody::Ok),
            error: None,
        }
    }

    pub fn state(id: i64, state: StateSnapshot) -> Self {
        Self {
            id: Some(id),
            result: Some(ResultBody::State { state }),
            error: None,
        }
    }

    pub fn catalog(id: i64, catalog: Catalog) -> Self {
        Self {
            id: Some(id),
            result: Some(ResultBody::Catalog { catalog }),
            error: None,
        }
    }

    /// A failure response. `id` is `None` when the request line was too malformed to
    /// recover one from.
    pub fn failure(id: Option<i64>, error: &Error) -> Self {
        Self {
            id,
            result: None,
            error: Some(ErrorObject::from(error)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResultBody {
    Ok,
    State { state: StateSnapshot },
    Catalog { catalog: Catalog },
    Favourites { favourites: Vec<String> },
    Preferences { preferences: Preferences },
}

/// The interface's own display preferences.
///
/// Recorded beside playback positions and favourites rather than in a second store of the
/// interface's own, but not part of [`StateSnapshot`]: they change when a user presses a
/// key, not while audio plays, and nothing subscribed needs them pushed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub inverted_palette: bool,
}

/// A structured failure. Clients branch on `code` and never parse `message`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: ErrorCode,
    pub message: String,
}

impl From<&Error> for ErrorObject {
    fn from(error: &Error) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
        }
    }
}

impl From<Error> for ErrorObject {
    fn from(error: Error) -> Self {
        Self::from(&error)
    }
}

/// One event line: `{"event":{"type":"state","state":{...}}}`. Events carry no `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventFrame {
    pub event: Event,
}

///
/// The size difference between the two variants buys nothing to box away: an event is
/// built, serialised, and dropped, and never held in bulk.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    State {
        state: StateSnapshot,
    },
    /// An event kind this release does not know, which a newer daemon pushed.
    #[serde(other)]
    Unknown,
}

/// One line read from the daemon, which is either a response or a pushed event.
///
/// Events are tried first: an event line has an `event` field and no response has one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Frame {
    Event(EventFrame),
    Response(Response),
}

/// What playback is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlaybackState {
    Stopped,
    Loading,
    Playing,
    Paused,
    /// A seek has been accepted and audio has not yet resumed at the target. Distinct from
    /// every other state so a client never shows a position that does not correspond to
    /// audible audio.
    Seeking,
    Error,
    /// A state this release does not know, which a newer daemon reported.
    #[serde(other)]
    Unknown,
}

/// Where the audio for the loaded episode is coming from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Source {
    Stream,
    Local,
    /// A source this release does not know, which a newer daemon reported.
    #[serde(other)]
    Unknown,
}

/// The loaded episode, as much of it as a snapshot needs to name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpisodeRef {
    /// The episode's stable identifier, as [`crate::model::Episode::id`] derives it.
    pub slug: String,
    pub title: String,
    pub duration_secs: f64,
}

/// What a download is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DownloadState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    /// A state this release does not know, which a newer daemon reported.
    #[serde(other)]
    Unknown,
}

/// One entry in a snapshot's `downloads` array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub slug: String,
    pub downloaded_bytes: u64,
    /// The byte length the feed declares for the enclosure.
    pub total_bytes: u64,
    pub state: DownloadState,
    /// Populated only when `state` is `failed`.
    pub error: Option<ErrorObject>,
}

/// How many frequency bins a [`Spectrum`] carries.
///
/// One per bin of a 2048-point FFT, which is what Web Audio's default `fftSize` yields and
/// therefore what the site's analyser is calibrated against.
pub const SPECTRUM_BINS: usize = 1024;

/// The lower bound of the decibel range a [`Spectrum`] maps onto `0..=255`.
pub const SPECTRUM_MIN_DB: f32 = -114.0;

/// The upper bound of the decibel range a [`Spectrum`] maps onto `0..=255`.
pub const SPECTRUM_MAX_DB: f32 = -30.0;

/// The exponential smoothing applied against the previous frame.
pub const SPECTRUM_SMOOTHING: f32 = 0.666;

/// A frequency spectrum of the audio currently being played.
///
/// Each byte is one bin's magnitude in decibels, scaled linearly from [`SPECTRUM_MIN_DB`]
/// to [`SPECTRUM_MAX_DB`] onto `0..=255` and clamped - the same contract as Web Audio's
/// `getByteFrequencyData`, so a client can run the site's own mapping arithmetic unchanged.
/// Bins run from lowest frequency to highest.
///
/// Carried base64-encoded rather than as an array of 1024 numbers: roughly 1.4 KB per
/// snapshot instead of 4 KB, and the snapshot stays readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spectrum(pub Vec<u8>);

impl Spectrum {
    /// A spectrum reporting no signal in any bin.
    pub fn silent() -> Self {
        Self(vec![0; SPECTRUM_BINS])
    }

    /// The bin at `index`, or 0 when the index is past the end.
    ///
    /// Not an error: the client's bin table is fixed while the length here is a wire value,
    /// so a short frame reads as silence rather than panicking mid-draw.
    pub fn bin(&self, index: usize) -> u8 {
        self.0.get(index).copied().unwrap_or(0)
    }
}

impl Serialize for Spectrum {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use base64::Engine as _;
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Spectrum {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use base64::Engine as _;
        let encoded = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// The complete player state.
///
/// Every event carries one of these in full; the protocol defines no deltas, so a client
/// that discards everything it has seen can still render from the next snapshot alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StateSnapshot {
    pub playback: PlaybackState,
    pub episode: Option<EpisodeRef>,
    /// The position of the audio actually being produced. Never reset or left stale by a
    /// seek, and never advanced past what is audible.
    pub position_secs: f64,
    /// The target of the outstanding seek, and `null` when no seek is outstanding. At
    /// most one seek is outstanding at a time; a later seek supersedes an earlier one.
    pub seek_target_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    /// True while `duration_secs` comes from the feed rather than from decoding.
    pub duration_approximate: bool,
    pub seekable: bool,
    pub source: Option<Source>,
    pub downloads: Vec<DownloadProgress>,
    /// Every episode the listener has marked a favourite, by episode identifier.
    #[serde(default)]
    pub favourites: Vec<String>,
    /// The spectrum of the audio being played.
    ///
    /// Absent when nothing is loaded, when playback is paused or stopped, and while a seek
    /// is in flight, so a client can tell "no audio to analyse" from "silence in the
    /// audio". A client that ignores it is unaffected by its presence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spectrum: Option<Spectrum>,
    pub error: Option<ErrorObject>,
}

impl StateSnapshot {
    /// A snapshot with nothing loaded.
    pub fn stopped() -> Self {
        Self {
            playback: PlaybackState::Stopped,
            episode: None,
            position_secs: 0.0,
            seek_target_secs: None,
            duration_secs: None,
            duration_approximate: true,
            seekable: false,
            source: None,
            downloads: Vec::new(),
            favourites: Vec::new(),
            spectrum: None,
            error: None,
        }
    }
}

/// A request line that could not be turned into a [`Request`], together with the response
/// the daemon owes for it.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestError {
    /// The request's id where it could be recovered from the line, and `None` otherwise.
    pub id: Option<i64>,
    pub error: ErrorObject,
}

impl RequestError {
    /// The response line to write back. The connection stays open either way.
    pub fn response(&self) -> Response {
        Response {
            id: self.id,
            result: None,
            error: Some(self.error.clone()),
        }
    }
}

/// Parses one request line, telling a malformed line from an unrecognised command from bad
/// arguments, each of which the spec answers with a different code.
///
/// Not JSON, not an object, or missing `id` or `cmd` is [`ErrorCode::InvalidRequest`] with
/// `id` `null`; an unrecognised `cmd.type` is [`ErrorCode::UnknownCommand`]; anything else
/// is [`ErrorCode::InvalidParams`].
pub fn parse_request(line: &str) -> std::result::Result<Request, RequestError> {
    fn reject(id: Option<i64>, error: Error) -> RequestError {
        RequestError {
            id,
            error: ErrorObject::from(&error),
        }
    }

    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|error| reject(None, Error::InvalidRequest(error.to_string())))?;

    let object = value.as_object().ok_or_else(|| {
        reject(
            None,
            Error::InvalidRequest("a request must be a JSON object".into()),
        )
    })?;

    let id = object
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| {
            reject(
                None,
                Error::InvalidRequest("a request must carry a numeric id".into()),
            )
        })?;

    let cmd = object.get("cmd").ok_or_else(|| {
        reject(
            Some(id),
            Error::InvalidRequest("a request must carry a cmd object".into()),
        )
    })?;

    let type_name = cmd
        .as_object()
        .and_then(|cmd| cmd.get("type"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            reject(
                Some(id),
                Error::InvalidRequest("cmd must be an object with a type string".into()),
            )
        })?;

    if !COMMAND_TYPES.contains(&type_name) {
        return Err(reject(
            Some(id),
            Error::UnknownCommand(type_name.to_owned()),
        ));
    }

    let command: Command = serde_json::from_value(cmd.clone())
        .map_err(|error| reject(Some(id), Error::InvalidParams(error.to_string())))?;
    command
        .validate()
        .map_err(|error| reject(Some(id), error))?;

    Ok(Request { id, cmd: command })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> StateSnapshot {
        StateSnapshot {
            favourites: Vec::new(),
            spectrum: None,
            playback: PlaybackState::Seeking,
            episode: Some(EpisodeRef {
                slug: "seventynine".into(),
                title: "Episode 79".into(),
                duration_secs: 14_400.0,
            }),
            position_secs: 1800.0,
            seek_target_secs: Some(5400.0),
            duration_secs: Some(14_400.0),
            duration_approximate: true,
            seekable: true,
            source: Some(Source::Stream),
            downloads: vec![DownloadProgress {
                slug: "seventyeight".into(),
                downloaded_bytes: 1024,
                total_bytes: 4096,
                state: DownloadState::Running,
                error: None,
            }],
            error: None,
        }
    }

    #[test]
    fn a_request_serialises_to_the_shape_the_spec_names() {
        let request = Request {
            id: 1,
            cmd: Command::Play {
                slug: Some("seventynine".into()),
            },
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"id":1,"cmd":{"type":"play","slug":"seventynine"}}"#
        );
    }

    /// What a snapshot costs to copy and to put on the wire.
    ///
    /// The measured baseline, so a regression shows up as a number rather than a hunch:
    /// 231 ns to clone and 1 us to serialise, for a line of 1732 bytes. The daemon pushes
    /// twenty a second, so the serialise is 0.002% of a core.
    #[test]
    #[ignore = "benchmark, run explicitly"]
    fn bench_snapshot_wire_cost() {
        use std::time::Instant;
        let mut snapshot = StateSnapshot::stopped();
        snapshot.playback = PlaybackState::Playing;
        snapshot.spectrum = Some(Spectrum((0..=255u8).cycle().take(SPECTRUM_BINS).collect()));
        snapshot.favourites = (0..12).map(|n| format!("episode-{n}")).collect();

        const N: u32 = 20_000;
        let start = Instant::now();
        for _ in 0..N {
            let _ = snapshot.clone();
        }
        println!("clone:     {:?} per snapshot", start.elapsed() / N);

        let start = Instant::now();
        for _ in 0..N {
            let _ = serde_json::to_string(&snapshot).unwrap();
        }
        println!("serialize: {:?} per snapshot", start.elapsed() / N);

        let line = serde_json::to_string(&snapshot).unwrap();
        println!("line size: {} bytes", line.len());

        let start = Instant::now();
        for _ in 0..N {
            let _: StateSnapshot = serde_json::from_str(&line).unwrap();
        }
        println!("parse:     {:?} per snapshot", start.elapsed() / N);
    }

    #[test]
    fn a_spectrum_round_trips_as_base64() {
        let spectrum = Spectrum((0..=255u8).cycle().take(SPECTRUM_BINS).collect());
        let json = serde_json::to_string(&spectrum).unwrap();
        assert!(
            json.starts_with('"') && json.ends_with('"'),
            "{json} is not a JSON string"
        );
        assert_eq!(serde_json::from_str::<Spectrum>(&json).unwrap(), spectrum);
    }

    #[test]
    fn a_spectrum_is_carried_far_more_compactly_than_an_array_of_numbers() {
        let spectrum = Spectrum(vec![200; SPECTRUM_BINS]);
        let encoded = serde_json::to_string(&spectrum).unwrap().len();
        let as_numbers = serde_json::to_string(&vec![200u8; SPECTRUM_BINS])
            .unwrap()
            .len();
        assert!(
            encoded < as_numbers,
            "base64 form ({encoded}) is not smaller than the array form ({as_numbers})"
        );
    }

    #[test]
    fn a_spectrum_reads_past_its_end_as_silence() {
        let spectrum = Spectrum(vec![9; 4]);
        assert_eq!(spectrum.bin(3), 9);
        assert_eq!(spectrum.bin(4), 0);
        assert_eq!(spectrum.bin(SPECTRUM_BINS), 0);
    }

    #[test]
    fn a_silent_spectrum_floors_every_bin() {
        let spectrum = Spectrum::silent();
        assert_eq!(spectrum.0.len(), SPECTRUM_BINS);
        assert!(spectrum.0.iter().all(|&bin| bin == 0));
    }

    #[test]
    fn a_snapshot_carrying_a_spectrum_and_favourites_round_trips() {
        let mut snapshot = StateSnapshot::stopped();
        snapshot.favourites = vec!["seventynine".into(), "sixtytwo".into()];
        snapshot.spectrum = Some(Spectrum(vec![7; SPECTRUM_BINS]));
        let json = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(
            serde_json::from_str::<StateSnapshot>(&json).unwrap(),
            snapshot
        );
    }

    #[test]
    fn a_stopped_snapshot_omits_the_spectrum_entirely() {
        let json = serde_json::to_string(&StateSnapshot::stopped()).unwrap();
        assert!(
            !json.contains("spectrum"),
            "{json} names an absent spectrum"
        );
    }

    #[test]
    fn a_snapshot_without_the_new_fields_still_deserialises() {
        let json = r#"{
            "playback": "stopped",
            "episode": null,
            "position_secs": 0.0,
            "seek_target_secs": null,
            "duration_secs": null,
            "duration_approximate": true,
            "volume": 0.8,
            "muted": false,
            "seekable": false,
            "source": null,
            "downloads": [],
            "error": null
        }"#;
        let snapshot: StateSnapshot = serde_json::from_str(json).unwrap();
        assert!(snapshot.favourites.is_empty());
        assert!(snapshot.spectrum.is_none());
    }

    #[test]
    fn the_favourites_result_body_round_trips() {
        let body = ResultBody::Favourites {
            favourites: vec!["seventynine".into()],
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(r#""type":"favourites""#), "{json}");
        assert_eq!(serde_json::from_str::<ResultBody>(&json).unwrap(), body);
    }

    #[test]
    fn the_preferences_result_body_round_trips() {
        let body = ResultBody::Preferences {
            preferences: Preferences {
                inverted_palette: true,
            },
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(r#""type":"preferences""#), "{json}");
        assert_eq!(serde_json::from_str::<ResultBody>(&json).unwrap(), body);
    }

    /// Reading is the command with no field, which must therefore be the wire default, not
    /// something a client has to spell out.
    #[test]
    fn preferences_with_no_field_is_a_read_and_omits_it_on_the_wire() {
        let request = parse_request(r#"{"id":1,"cmd":{"type":"preferences"}}"#).unwrap();
        assert_eq!(
            request.cmd,
            Command::Preferences {
                inverted_palette: None,
            }
        );

        let json = serde_json::to_string(&request.cmd).unwrap();
        assert!(!json.contains("inverted_palette"), "{json}");
    }

    /// The cookie notice is gone and so is the preference recording its dismissal. A client
    /// too old to know that must still be answered rather than refused, which is serde's
    /// default for an unknown field - verified here rather than assumed.
    #[test]
    fn a_client_still_sending_the_dismissed_notice_is_parsed_rather_than_refused() {
        let request = parse_request(
            r#"{"id":1,"cmd":{"type":"preferences","inverted_palette":true,"notice_dismissed":true}}"#,
        )
        .expect("the old field was refused");
        assert_eq!(
            request.cmd,
            Command::Preferences {
                inverted_palette: Some(true),
            }
        );

        let preferences: Preferences =
            serde_json::from_str(r#"{"inverted_palette":true,"notice_dismissed":true}"#)
                .expect("the old field was refused");
        assert!(preferences.inverted_palette);
    }

    /// Absent preferences are the defaults, which is what a daemon never told otherwise
    /// reports.
    #[test]
    fn absent_preferences_deserialise_to_the_defaults() {
        let preferences: Preferences = serde_json::from_str("{}").unwrap();
        assert_eq!(preferences, Preferences::default());
        assert!(!preferences.inverted_palette);
    }

    #[test]
    fn subscribe_defaults_to_no_spectrum_so_an_old_client_is_not_woken_for_it() {
        let request = parse_request(r#"{"id":1,"cmd":{"type":"subscribe"}}"#).unwrap();
        assert_eq!(request.cmd, Command::Subscribe { spectrum: false });
    }

    #[test]
    fn an_unparseable_line_is_invalid_request_with_a_null_id() {
        let rejected = parse_request("not json").unwrap_err();
        assert_eq!(rejected.id, None);
        assert_eq!(rejected.error.code, ErrorCode::InvalidRequest);
        assert_eq!(
            serde_json::to_string(&rejected.response()).unwrap(),
            format!(
                r#"{{"id":null,"error":{{"code":"invalid_request","message":{}}}}}"#,
                serde_json::to_string(&rejected.error.message).unwrap()
            )
        );
    }

    #[test]
    fn a_line_that_is_not_an_object_is_invalid_request() {
        assert_eq!(
            parse_request("[1,2,3]").unwrap_err().error.code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn a_missing_id_or_cmd_is_invalid_request() {
        assert_eq!(
            parse_request(r#"{"cmd":{"type":"pause"}}"#)
                .unwrap_err()
                .error
                .code,
            ErrorCode::InvalidRequest
        );
        let rejected = parse_request(r#"{"id":7}"#).unwrap_err();
        assert_eq!(rejected.id, Some(7));
        assert_eq!(rejected.error.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn an_unrecognised_command_keeps_its_id() {
        let rejected = parse_request(r#"{"id":7,"cmd":{"type":"teleport"}}"#).unwrap_err();
        assert_eq!(rejected.id, Some(7));
        assert_eq!(rejected.error.code, ErrorCode::UnknownCommand);
    }

    #[test]
    fn a_seek_with_neither_or_both_forms_is_invalid_params() {
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":{"type":"seek"}}"#)
                .unwrap_err()
                .error
                .code,
            ErrorCode::InvalidParams
        );
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":{"type":"seek","position_secs":1,"delta_secs":2}}"#)
                .unwrap_err()
                .error
                .code,
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn a_snapshot_from_an_older_daemon_still_deserialises() {
        // an older daemon's line carries volume and muted; a newer client must read the rest
        // of it rather than fail on two fields it no longer knows
        let json = r#"{
            "playback": "playing",
            "episode": null,
            "position_secs": 12.0,
            "seek_target_secs": null,
            "duration_secs": null,
            "duration_approximate": true,
            "volume": 0.8,
            "muted": true,
            "seekable": true,
            "source": "stream",
            "downloads": [],
            "error": null
        }"#;
        let snapshot: StateSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.position_secs, 12.0);
        assert_eq!(snapshot.playback, PlaybackState::Playing);
    }

    #[test]
    fn a_snapshot_carries_no_loudness_at_all() {
        let json = serde_json::to_string(&StateSnapshot::stopped()).unwrap();
        assert!(!json.contains("volume"), "{json}");
        assert!(!json.contains("muted"), "{json}");
    }

    #[test]
    fn volume_is_no_longer_a_command_the_daemon_knows() {
        // loudness belongs to the operating system's mixer, and a caller written against the
        // older surface must fail loudly rather than believe it changed something
        for line in [
            r#"{"id":1,"cmd":{"type":"volume"}}"#,
            r#"{"id":1,"cmd":{"type":"volume","level":0.5}}"#,
            r#"{"id":1,"cmd":{"type":"volume","delta":0.05}}"#,
            r#"{"id":1,"cmd":{"type":"volume","muted":true}}"#,
            r#"{"id":1,"cmd":{"type":"mute","state":"on"}}"#,
        ] {
            assert_eq!(
                parse_request(line).unwrap_err().error.code,
                ErrorCode::UnknownCommand,
                "{line} was not rejected as unknown"
            );
        }
        assert!(!COMMAND_TYPES.contains(&"volume"));
        assert!(!COMMAND_TYPES.contains(&"mute"));
    }

    #[test]
    fn a_seek_the_audio_engine_could_not_hold_is_invalid_params() {
        // 1e20 seconds is past what `Duration::from_secs_f64` accepts, and it reached the
        // engine unchecked before this was validated
        for line in [
            r#"{"id":1,"cmd":{"type":"seek","position_secs":1e20}}"#,
            r#"{"id":1,"cmd":{"type":"seek","position_secs":-1.0}}"#,
            r#"{"id":1,"cmd":{"type":"seek","delta_secs":1e20}}"#,
            r#"{"id":1,"cmd":{"type":"seek","delta_secs":-1e20}}"#,
        ] {
            let rejected = parse_request(line).unwrap_err();
            assert_eq!(
                rejected.error.code,
                ErrorCode::InvalidParams,
                "{line} was not refused"
            );
            assert_eq!(rejected.id, Some(1));
        }
    }

    /// JSON writes neither, so these reach `validate` only from a caller built in process.
    #[test]
    fn a_seek_to_a_non_finite_position_is_invalid_params() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let command = Command::Seek {
                position_secs: Some(value),
                delta_secs: None,
            };
            assert_eq!(
                command.validate().unwrap_err().code(),
                ErrorCode::InvalidParams,
                "{value} was not refused"
            );
        }
    }

    #[test]
    fn a_seek_inside_the_range_is_accepted() {
        let request = parse_request(r#"{"id":1,"cmd":{"type":"seek","position_secs":1800.5}}"#)
            .expect("an ordinary seek was refused");
        assert_eq!(
            request.cmd,
            Command::Seek {
                position_secs: Some(1800.5),
                delta_secs: None,
            }
        );
        assert!(
            parse_request(r#"{"id":1,"cmd":{"type":"seek","delta_secs":-30.0}}"#)
                .unwrap()
                .cmd
                .validate()
                .is_ok()
        );
    }

    /// `COMMAND_TYPES` is what `parse_request` gates on, and it is maintained by hand. A
    /// variant missing from it is answered `unknown_command` by a daemon that implements it,
    /// with nothing else to catch the drift.
    #[test]
    fn every_command_variant_is_named_in_command_types() {
        let every_variant = [
            Command::Play { slug: None },
            Command::Pause,
            Command::Toggle,
            Command::Stop,
            Command::Seek {
                position_secs: Some(0.0),
                delta_secs: None,
            },
            Command::Next,
            Command::Previous,
            Command::Download {
                slug: String::new(),
            },
            Command::CancelDownload {
                slug: String::new(),
            },
            Command::DeleteDownload {
                slug: String::new(),
            },
            Command::Favourite {
                slug: String::new(),
            },
            Command::Unfavourite {
                slug: String::new(),
            },
            Command::ListFavourites,
            Command::Preferences {
                inverted_palette: None,
            },
            Command::Status,
            Command::ListCatalog,
            Command::Subscribe { spectrum: false },
            Command::Shutdown,
        ];

        for command in &every_variant {
            assert!(
                COMMAND_TYPES.contains(&command.type_name()),
                "{} is not in COMMAND_TYPES",
                command.type_name()
            );
        }
        assert_eq!(
            COMMAND_TYPES.len(),
            every_variant.len(),
            "COMMAND_TYPES names a command no variant spells"
        );
    }

    /// The protocol carries no version, so an added spelling must cost a client that one
    /// value rather than the whole line.
    #[test]
    fn a_snapshot_spelling_this_release_does_not_know_reads_as_unknown() {
        let mut json = serde_json::to_value(snapshot()).unwrap();
        json["playback"] = serde_json::json!("buffering");
        json["source"] = serde_json::json!("satellite");
        json["downloads"][0]["state"] = serde_json::json!("verifying");

        let snapshot: StateSnapshot = serde_json::from_value(json).unwrap();

        assert_eq!(snapshot.playback, PlaybackState::Unknown);
        assert_eq!(snapshot.source, Some(Source::Unknown));
        assert_eq!(snapshot.downloads[0].state, DownloadState::Unknown);
        // everything beside the three unknown spellings still arrived
        assert_eq!(snapshot.position_secs, 1800.0);
        assert_eq!(snapshot.episode.unwrap().slug, "seventynine");
    }

    #[test]
    fn an_event_kind_this_release_does_not_know_reads_as_unknown() {
        let line = r#"{"event":{"type":"chapter","index":3}}"#;
        assert_eq!(
            serde_json::from_str::<Frame>(line).unwrap(),
            Frame::Event(EventFrame {
                event: Event::Unknown
            })
        );
    }

    #[test]
    fn a_download_missing_its_slug_is_invalid_params() {
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":{"type":"download"}}"#)
                .unwrap_err()
                .error
                .code,
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn a_delete_download_missing_its_slug_is_invalid_params() {
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":{"type":"delete_download"}}"#)
                .unwrap_err()
                .error
                .code,
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn the_eviction_and_catalog_request_lines_parse() {
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":{"type":"delete_download","slug":"seventynine"}}"#)
                .unwrap(),
            Request {
                id: 1,
                cmd: Command::DeleteDownload {
                    slug: "seventynine".into()
                }
            }
        );
        assert_eq!(
            parse_request(r#"{"id":2,"cmd":{"type":"list_catalog"}}"#).unwrap(),
            Request {
                id: 2,
                cmd: Command::ListCatalog
            }
        );
    }

    #[test]
    fn a_catalog_response_serialises_to_the_shape_the_spec_names_and_round_trips() {
        let catalog = Catalog {
            info: Vec::new(),
            episodes: vec![crate::model::Episode {
                bundle_title: None,
                special: false,
                title: "Episode 79".into(),
                link: "https://musicforprogramming.net/seventynine".into(),
                enclosure_url: "https://datashat.net/music_for_programming_79.mp3".into(),
                byte_len: 441_000_000,
                duration_secs: 14_400,
                published_at: 1_700_000_000,
                slug: Some("seventynine".into()),
                order: Some(79),
                tracklist: None,
                body: None,
                links: None,
            }],
            fetched_at: 1_700_000_000,
            enriched: true,
        };
        let response = Response::catalog(3, catalog);
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.starts_with(r#"{"id":3,"result":{"type":"catalog","catalog":{"#));
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
    }

    #[test]
    fn an_ok_response_serialises_to_the_shape_the_spec_names() {
        assert_eq!(
            serde_json::to_string(&Response::ok(1)).unwrap(),
            r#"{"id":1,"result":{"type":"ok"}}"#
        );
    }

    #[test]
    fn an_error_response_serialises_to_the_shape_the_spec_names() {
        assert_eq!(
            serde_json::to_string(&Response::failure(Some(2), &Error::NotPlaying)).unwrap(),
            r#"{"id":2,"error":{"code":"not_playing","message":"nothing is loaded"}}"#
        );
    }

    #[test]
    fn a_snapshot_carries_every_field_the_spec_names() {
        let json = serde_json::to_value(snapshot()).unwrap();
        let object = json.as_object().unwrap();
        for field in [
            "playback",
            "episode",
            "position_secs",
            "seek_target_secs",
            "duration_secs",
            "duration_approximate",
            "seekable",
            "source",
            "downloads",
            "error",
        ] {
            assert!(object.contains_key(field), "snapshot is missing {field}");
        }
        assert_eq!(object["playback"], "seeking");
        assert_eq!(object["source"], "stream");
        assert_eq!(object["seek_target_secs"], 5400.0);

        let episode = object["episode"].as_object().unwrap();
        for field in ["slug", "title", "duration_secs"] {
            assert!(episode.contains_key(field), "episode is missing {field}");
        }

        let download = object["downloads"][0].as_object().unwrap();
        for field in ["slug", "downloaded_bytes", "total_bytes", "state"] {
            assert!(download.contains_key(field), "download is missing {field}");
        }
        assert_eq!(download["state"], "running");
    }

    #[test]
    fn absent_snapshot_values_serialise_as_null_rather_than_being_omitted() {
        let json = serde_json::to_value(StateSnapshot::stopped()).unwrap();
        for field in [
            "episode",
            "seek_target_secs",
            "duration_secs",
            "source",
            "error",
        ] {
            assert_eq!(json[field], serde_json::Value::Null, "{field} was omitted");
        }
        assert_eq!(json["playback"], "stopped");
    }

    #[test]
    fn every_playback_spelling_matches_the_spec() {
        let spellings = [
            (PlaybackState::Stopped, "stopped"),
            (PlaybackState::Loading, "loading"),
            (PlaybackState::Playing, "playing"),
            (PlaybackState::Paused, "paused"),
            (PlaybackState::Seeking, "seeking"),
            (PlaybackState::Error, "error"),
        ];
        for (state, spelling) in spellings {
            assert_eq!(serde_json::to_value(state).unwrap(), spelling);
        }
    }

    #[test]
    fn every_download_state_spelling_round_trips() {
        for (state, spelling) in [
            (DownloadState::Queued, "queued"),
            (DownloadState::Running, "running"),
            (DownloadState::Completed, "completed"),
            (DownloadState::Failed, "failed"),
            (DownloadState::Cancelled, "cancelled"),
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), spelling);
            assert_eq!(
                serde_json::from_value::<DownloadState>(spelling.into()).unwrap(),
                state
            );
        }
    }

    #[test]
    fn a_state_response_round_trips() {
        let response = Response::state(6, snapshot());
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.starts_with(r#"{"id":6,"result":{"type":"state","state":{"#));
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
    }

    #[test]
    fn an_event_serialises_to_the_shape_the_spec_names_and_carries_no_id() {
        let frame = EventFrame {
            event: Event::State { state: snapshot() },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.starts_with(r#"{"event":{"type":"state","state":{"#));
        assert!(!json.contains(r#""id":"#));
        assert_eq!(serde_json::from_str::<EventFrame>(&json).unwrap(), frame);
    }

    #[test]
    fn a_frame_tells_events_from_responses() {
        let event = EventFrame {
            event: Event::State { state: snapshot() },
        };
        let event_line = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<Frame>(&event_line).unwrap(),
            Frame::Event(event)
        );

        let response = Response::ok(1);
        let response_line = serde_json::to_string(&response).unwrap();
        assert_eq!(
            serde_json::from_str::<Frame>(&response_line).unwrap(),
            Frame::Response(response)
        );

        let failure = Response::failure(None, &Error::InvalidRequest("bad".into()));
        let failure_line = serde_json::to_string(&failure).unwrap();
        assert_eq!(
            serde_json::from_str::<Frame>(&failure_line).unwrap(),
            Frame::Response(failure)
        );
    }

    #[test]
    fn a_snapshot_survives_fields_a_client_does_not_recognise() {
        let mut json = serde_json::to_value(snapshot()).unwrap();
        json["a_field_from_a_later_release"] = serde_json::json!("ignored");
        assert_eq!(
            serde_json::from_value::<StateSnapshot>(json).unwrap(),
            snapshot()
        );
    }

    #[test]
    fn no_line_a_daemon_writes_contains_a_newline() {
        for line in [
            serde_json::to_string(&Response::state(1, snapshot())).unwrap(),
            serde_json::to_string(&EventFrame {
                event: Event::State { state: snapshot() },
            })
            .unwrap(),
        ] {
            assert!(!line.contains('\n'));
        }
    }
}
