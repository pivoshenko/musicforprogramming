//! Seeking, which is rebuild-based because in-place seeking is not safe.
//!
//! Measured against the real upstream file: a forward seek on a live decoder succeeds, but
//! a backward seek fails *and leaves the decoder permanently wedged*. After a wedge every
//! later seek fails in about 8 ms while playback keeps going and `get_pos` keeps advancing,
//! so the position readout silently lies.
//!
//! Every seek therefore tears the chain down and rebuilds it at the target. Two
//! consequences for the caller: the rebuilt decoder's clock restarts at zero, so position
//! is `base_offset + decoder position`; and a rebuild costs up to ~11 s on a distant
//! streamed target, so seeking is an observable state rather than a synchronous call.

use std::path::Path;
use std::time::Duration;

use mfp_core::Episode;
use mfp_core::error::{Error, Result};
use mfp_core::protocol::Source;
use rodio::Source as _;

use super::source::{self, OpenSource, SeekableRead};

/// A decoder chain positioned at a target, with everything the caller has to learn from it.
///
/// It carries the decoder rather than the [`OpenSource`] it was built from because a chain
/// only counts as positioned once the decoder has been moved to the target; handing back an
/// unpositioned reader would put the one step that can fail outside the module that exists
/// to get it right.
pub struct Chain {
    pub decoder: rodio::Decoder<Box<dyn SeekableRead>>,
    /// Where this decoder's zero sits in the episode. A rebuilt decoder's clock restarts at
    /// zero, so every reported position is this plus the player's own clock; the raw player
    /// position after a seek would show the wrong time.
    pub base_offset_secs: f64,
    pub kind: Source,
    pub seekable: bool,
}

/// The number of seconds a [`Duration`] can hold. Exclusive: `u64::MAX as f64` has no exact
/// `f64` and rounds up to 2^64, which `Duration::from_secs_f64` refuses along with
/// everything above it.
const SECS_LIMIT: f64 = u64::MAX as f64;

/// Converts seconds into a [`Duration`] without the panic `Duration::from_secs_f64`
/// carries for a negative, non-finite, or out-of-range input.
///
/// `mfp_core::protocol::Command::validate` already refuses such a target, so an error here
/// means one arrived by another route - still a refusal rather than a panic, because a
/// panic on the audio thread ends playback for the life of the daemon.
pub fn duration_from_secs(secs: f64) -> Result<Duration> {
    if secs.is_finite() && (0.0..SECS_LIMIT).contains(&secs) {
        Ok(Duration::from_secs_f64(secs))
    } else {
        Err(Error::InvalidParams(format!(
            "{secs} is not a position this player can hold"
        )))
    }
}

/// How a seek to a given target is to be carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekPlan {
    /// The target is ahead of the decoder's current position, the one direction measured
    /// to be safe in place. Any error from the attempt still forces a rebuild.
    InPlaceThenRebuild,
    /// The target is at or behind the current position. Seeking backward in place wedges
    /// the decoder permanently, so it is never attempted.
    Rebuild,
}

/// Chooses between the in-place fast path and a rebuild.
///
/// Both positions are absolute episode positions. The comparison is deliberately strict: a
/// target equal to the current one rebuilds, because a zero-distance in-place seek buys
/// nothing and the rebuild is the path known to work.
pub fn plan(current_secs: f64, target_secs: f64) -> SeekPlan {
    if target_secs > current_secs {
        SeekPlan::InPlaceThenRebuild
    } else {
        SeekPlan::Rebuild
    }
}

/// Resolves a requested seek into an absolute target.
///
/// A relative offset applies to `current_secs` and is then treated exactly as an absolute
/// target. Below zero clamps to the start. At or past the end is returned unchanged, for
/// the caller to treat as end-of-episode rather than as an error.
pub fn resolve_target(
    current_secs: f64,
    position_secs: Option<f64>,
    delta_secs: Option<f64>,
) -> f64 {
    let target = match (position_secs, delta_secs) {
        (Some(position), _) => position,
        (None, Some(delta)) => current_secs + delta,
        (None, None) => current_secs,
    };
    // `max` yields the operand that is not NaN, so this clamps a nonsensical target to
    // the start rather than propagating it into a Duration that would panic
    target.max(0.0)
}

/// Rebuilds the decoder chain positioned at `target_secs`.
///
/// The new decoder starts at zero and moves forward to the target, the direction measured
/// to be safe; nothing is ever seeked backward. The reader is reopened from scratch, so a
/// decoder wedged by an earlier failure cannot survive into it and no seek is affected by
/// an earlier one.
///
/// `range_support` is what an earlier open of the same enclosure learned about the host, so
/// a session's second and later rebuilds do not probe it again.
pub fn rebuild_at(
    runtime: &tokio::runtime::Handle,
    episode: &Episode,
    local_path: Option<&Path>,
    target_secs: f64,
    range_support: Option<bool>,
) -> Result<Chain> {
    position(
        source::open(runtime, episode, local_path, range_support)?,
        target_secs,
    )
}

/// Builds a decoder over an opened source and moves it forward to the target.
fn position(opened: OpenSource, target_secs: f64) -> Result<Chain> {
    // Before the decoder is built, so a target no decoder could be moved to costs nothing
    let target = duration_from_secs(target_secs)?;
    let OpenSource {
        reader,
        kind,
        seekable,
    } = opened;
    let mut decoder = rodio::Decoder::new(reader)
        .map_err(|error| Error::PlaybackFailed(format!("could not decode the audio: {error}")))?;

    if target_secs > 0.0 {
        if !seekable {
            return Err(Error::SeekUnsupported);
        }
        decoder.try_seek(target).map_err(|error| {
            Error::PlaybackFailed(format!(
                "could not position the decoder at {target_secs:.0}s: {error}"
            ))
        })?;
    }

    Ok(Chain {
        decoder,
        base_offset_secs: target_secs,
        kind,
        seekable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::testing;

    fn runtime() -> tokio::runtime::Runtime {
        // Multi-threaded on purpose: the audio thread reaches the network through
        // `Handle::block_on`, which cannot drive a current-thread runtime from off it
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn an_absolute_target_is_taken_as_given() {
        assert_eq!(resolve_target(300.0, Some(5400.0), None), 5400.0);
    }

    #[test]
    fn a_relative_offset_applies_to_the_current_position() {
        assert_eq!(resolve_target(300.0, None, Some(30.0)), 330.0);
        assert_eq!(resolve_target(300.0, None, Some(-30.0)), 270.0);
    }

    #[test]
    fn a_target_below_zero_clamps_to_the_start() {
        assert_eq!(resolve_target(0.0, Some(-100.0), None), 0.0);
        assert_eq!(resolve_target(10.0, None, Some(-30.0)), 0.0);
        assert_eq!(resolve_target(10.0, Some(f64::NAN), None), 0.0);
    }

    #[test]
    fn a_target_past_the_end_is_left_for_the_caller_to_treat_as_the_end() {
        assert_eq!(resolve_target(100.0, Some(99_999.0), None), 99_999.0);
    }

    #[test]
    fn a_backward_target_is_never_seeked_in_place() {
        assert_eq!(plan(1800.0, 300.0), SeekPlan::Rebuild);
        assert_eq!(plan(300.0, 299.999), SeekPlan::Rebuild);
        assert_eq!(plan(300.0, 300.0), SeekPlan::Rebuild);
    }

    #[test]
    fn a_forward_target_may_take_the_in_place_fast_path() {
        assert_eq!(plan(300.0, 330.0), SeekPlan::InPlaceThenRebuild);
        assert_eq!(plan(0.0, 1800.0), SeekPlan::InPlaceThenRebuild);
    }

    /// The regression test this whole module exists for.
    ///
    /// A backward seek on a live decoder wedges it permanently, so every seek rebuilds. This
    /// walks forward, back, and forward again, checking not that each call returns `Ok` but
    /// that the decoder really is producing the audio at the target - the wedge's signature
    /// was audio that kept coming under a position that had nothing to do with it.
    #[test]
    fn a_forward_then_backward_then_forward_seek_all_succeed_and_land_on_their_targets() {
        let fixture = testing::Fixture::new();
        let runtime = runtime();

        for (target, expected_plateau) in [(4.5, 4), (1.5, 1), (5.5, 5)] {
            let mut chain = rebuild_at(
                runtime.handle(),
                &fixture.episode,
                Some(&fixture.path),
                target,
                None,
            )
            .unwrap_or_else(|error| panic!("the rebuild at {target}s failed: {error}"));

            assert_eq!(chain.base_offset_secs, target);
            assert_eq!(
                testing::plateau(&mut chain.decoder),
                expected_plateau,
                "the chain rebuilt at {target}s is not producing the audio there"
            );
        }
    }

    #[test]
    fn ten_alternating_seeks_all_succeed() {
        let fixture = testing::Fixture::new();
        let runtime = runtime();

        let targets = [0.5, 5.5, 1.5, 4.5, 2.5, 3.5, 0.5, 5.5, 1.5, 4.5];
        for target in targets {
            let mut chain = rebuild_at(
                runtime.handle(),
                &fixture.episode,
                Some(&fixture.path),
                target,
                None,
            )
            .unwrap_or_else(|error| panic!("the rebuild at {target}s failed: {error}"));
            assert_eq!(
                testing::plateau(&mut chain.decoder),
                target as i64,
                "the chain rebuilt at {target}s is not producing the audio there"
            );
        }
    }

    #[test]
    fn a_rebuild_at_the_start_needs_no_seek_at_all() {
        let fixture = testing::Fixture::new();
        let runtime = runtime();
        let mut chain = rebuild_at(
            runtime.handle(),
            &fixture.episode,
            Some(&fixture.path),
            0.0,
            None,
        )
        .unwrap();
        assert_eq!(chain.base_offset_secs, 0.0);
        assert_eq!(testing::plateau(&mut chain.decoder), 0);
    }

    #[test]
    fn a_source_that_cannot_seek_refuses_promptly_rather_than_blocking() {
        let fixture = testing::Fixture::new();
        let reader = std::fs::File::open(&fixture.path).unwrap();
        let opened = OpenSource {
            reader: Box::new(std::io::BufReader::new(reader)),
            kind: Source::Stream,
            seekable: false,
        };
        let Err(error) = position(opened, 300.0) else {
            panic!("a source that cannot seek must refuse a non-zero target");
        };
        assert_eq!(error.code(), mfp_core::ErrorCode::SeekUnsupported);
    }

    #[test]
    fn a_target_inside_the_range_converts_to_the_duration_it_names() {
        assert_eq!(duration_from_secs(0.0).unwrap(), Duration::ZERO);
        assert_eq!(
            duration_from_secs(1_800.5).unwrap(),
            Duration::from_millis(1_800_500)
        );
    }

    /// The conversion that used to be `Duration::from_secs_f64` outright, which panics
    /// on every one of these and would take the audio thread with it.
    #[test]
    fn a_target_no_duration_could_hold_is_refused_rather_than_panicking() {
        for hostile in [
            1e20,
            SECS_LIMIT,
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            -1.0,
        ] {
            let Err(error) = duration_from_secs(hostile) else {
                panic!("{hostile} converted to a Duration");
            };
            assert_eq!(error.code(), mfp_core::ErrorCode::InvalidParams);
        }
    }

    /// The same target reaching the rebuild is an error the caller can report, not a
    /// panic, and it costs no source open to find out.
    #[test]
    fn a_rebuild_at_a_target_no_duration_could_hold_is_refused() {
        let opened = OpenSource {
            reader: Box::new(std::io::Cursor::new(b"not audio".to_vec())),
            kind: Source::Stream,
            seekable: true,
        };
        let Err(error) = position(opened, 1e20) else {
            panic!("a target beyond the Duration range must not yield a chain");
        };
        assert_eq!(error.code(), mfp_core::ErrorCode::InvalidParams);
    }

    #[test]
    fn an_undecodable_source_fails_rather_than_producing_a_chain() {
        let opened = OpenSource {
            reader: Box::new(std::io::Cursor::new(b"not audio".to_vec())),
            kind: Source::Stream,
            seekable: true,
        };
        let Err(error) = position(opened, 0.0) else {
            panic!("bytes that are not audio must not yield a chain");
        };
        assert_eq!(error.code(), mfp_core::ErrorCode::PlaybackFailed);
    }
}
