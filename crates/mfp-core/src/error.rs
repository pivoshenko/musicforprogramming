//! The crate error type and the stable machine-readable codes clients branch on.
//!
//! Every variant maps to exactly one [`ErrorCode`]. The codes are wire contract, defined
//! in `specs/player-ipc/spec.md` and stable across releases; the human-readable messages
//! are not, and must never be parsed.

use serde::{Deserialize, Serialize};

/// A stable machine-readable error code.
///
/// Any condition not covered by a more specific code uses [`ErrorCode::Internal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// The request line was not valid JSON, was not an object, or lacked `id` or `cmd`.
    InvalidRequest,
    /// The `cmd.type` field named a command the daemon does not implement.
    UnknownCommand,
    /// The arguments parsed but fall outside their permitted domain.
    InvalidParams,
    /// No catalog episode has the requested identifier.
    UnknownEpisode,
    /// The command needs a loaded episode and none is loaded.
    NotPlaying,
    /// The current audio source cannot seek.
    SeekUnsupported,
    /// Audio output, decoding, or the audio transfer failed.
    PlaybackFailed,
    /// The catalog could not be served from cache or network.
    CatalogUnavailable,
    /// A download transfer failed or its verification did not pass.
    DownloadFailed,
    /// No download is running for the named episode.
    DownloadNotActive,
    /// The cache filesystem has less free space than the download requires.
    InsufficientSpace,
    /// Anything not covered by a more specific code.
    Internal,
    /// A spelling this release does not know, which a newer daemon sent.
    ///
    /// The protocol names no version, so an added code costs a client that one code rather
    /// than the whole response line.
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    /// The code's wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::UnknownCommand => "unknown_command",
            Self::InvalidParams => "invalid_params",
            Self::UnknownEpisode => "unknown_episode",
            Self::NotPlaying => "not_playing",
            Self::SeekUnsupported => "seek_unsupported",
            Self::PlaybackFailed => "playback_failed",
            Self::CatalogUnavailable => "catalog_unavailable",
            Self::DownloadFailed => "download_failed",
            Self::DownloadNotActive => "download_not_active",
            Self::InsufficientSpace => "insufficient_space",
            Self::Internal => "internal",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Any failure the player can report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("unknown command: {0}")]
    UnknownCommand(String),

    #[error("invalid parameters: {0}")]
    InvalidParams(String),

    #[error("no episode with identifier {0}")]
    UnknownEpisode(String),

    #[error("nothing is loaded")]
    NotPlaying,

    #[error("the current source does not support seeking")]
    SeekUnsupported,

    #[error("playback failed: {0}")]
    PlaybackFailed(String),

    #[error("catalog unavailable: {0}")]
    CatalogUnavailable(String),

    #[error("download failed: {0}")]
    DownloadFailed(String),

    #[error("no download in progress for {0}")]
    DownloadNotActive(String),

    #[error("{required_bytes} bytes are required but only {available_bytes} are free")]
    InsufficientSpace {
        required_bytes: u64,
        available_bytes: u64,
    },

    #[error("{0}")]
    Internal(String),

    /// An I/O failure, kept whole so a caller can still read its [`std::io::ErrorKind`].
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// The stable code for this failure.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::UnknownCommand(_) => ErrorCode::UnknownCommand,
            Self::InvalidParams(_) => ErrorCode::InvalidParams,
            Self::UnknownEpisode(_) => ErrorCode::UnknownEpisode,
            Self::NotPlaying => ErrorCode::NotPlaying,
            Self::SeekUnsupported => ErrorCode::SeekUnsupported,
            Self::PlaybackFailed(_) => ErrorCode::PlaybackFailed,
            Self::CatalogUnavailable(_) => ErrorCode::CatalogUnavailable,
            Self::DownloadFailed(_) => ErrorCode::DownloadFailed,
            Self::DownloadNotActive(_) => ErrorCode::DownloadNotActive,
            Self::InsufficientSpace { .. } => ErrorCode::InsufficientSpace,
            Self::Internal(_) | Self::Io(_) => ErrorCode::Internal,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_maps_to_its_spec_code() {
        let cases: Vec<(Error, &str)> = vec![
            (Error::InvalidRequest("x".into()), "invalid_request"),
            (Error::UnknownCommand("x".into()), "unknown_command"),
            (Error::InvalidParams("x".into()), "invalid_params"),
            (Error::UnknownEpisode("x".into()), "unknown_episode"),
            (Error::NotPlaying, "not_playing"),
            (Error::SeekUnsupported, "seek_unsupported"),
            (Error::PlaybackFailed("x".into()), "playback_failed"),
            (Error::CatalogUnavailable("x".into()), "catalog_unavailable"),
            (Error::DownloadFailed("x".into()), "download_failed"),
            (Error::DownloadNotActive("x".into()), "download_not_active"),
            (
                Error::InsufficientSpace {
                    required_bytes: 2,
                    available_bytes: 1,
                },
                "insufficient_space",
            ),
            (Error::Internal("x".into()), "internal"),
            (
                Error::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                "internal",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code().as_str(), code);
        }
    }

    #[test]
    fn codes_serialise_to_their_wire_spelling() {
        for code in [
            ErrorCode::InvalidRequest,
            ErrorCode::UnknownCommand,
            ErrorCode::InvalidParams,
            ErrorCode::UnknownEpisode,
            ErrorCode::NotPlaying,
            ErrorCode::SeekUnsupported,
            ErrorCode::PlaybackFailed,
            ErrorCode::CatalogUnavailable,
            ErrorCode::DownloadFailed,
            ErrorCode::DownloadNotActive,
            ErrorCode::InsufficientSpace,
            ErrorCode::Internal,
            ErrorCode::Unknown,
        ] {
            let json = serde_json::to_string(&code).unwrap();
            assert_eq!(json, format!("\"{}\"", code.as_str()));
            assert_eq!(serde_json::from_str::<ErrorCode>(&json).unwrap(), code);
        }
    }

    /// A newer daemon may name a code this release has never heard of, and the protocol
    /// carries no version to negotiate it away.
    #[test]
    fn a_code_this_release_does_not_know_reads_as_unknown() {
        assert_eq!(
            serde_json::from_str::<ErrorCode>(r#""a_code_from_a_later_release""#).unwrap(),
            ErrorCode::Unknown
        );
    }

    #[test]
    fn an_io_failure_keeps_its_kind_and_its_source() {
        let error = Error::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied));

        let Error::Io(source) = &error else {
            panic!("{error:?} is not an I/O failure");
        };
        assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(std::error::Error::source(&error).is_some());
    }
}
