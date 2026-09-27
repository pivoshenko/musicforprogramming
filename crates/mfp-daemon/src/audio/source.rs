//! Building a decodable byte source for an episode.
//!
//! Streaming and local playback differ only in where the bytes come from, so both are built
//! behind one interface and the decoder pipeline downstream is identical. A complete
//! verified local file is always preferred; a `.part` file is never a playback source.

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::time::Duration;

use mfp_core::Episode;
use mfp_core::error::{Error, Result};
use mfp_core::protocol::Source;
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};

use super::spectrum::FFT_SIZE;

/// How much of the enclosure to buffer before the decoder is handed the reader: enough for
/// it to recognise the format and start producing audio, small enough that a 441 MB episode
/// is still audible in about half a second.
const PREFETCH_BYTES: u64 = 64 * 1024;

/// How long a transfer may receive nothing before it is reconnected from the last
/// received offset. `stream-download` performs the resume itself.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A byte source the decoder can consume: the range-backed reader from `stream-download`,
/// or a plain `File` for a complete local copy.
pub trait SeekableRead: Read + Seek + Send + Sync {}

impl<T: Read + Seek + Send + Sync> SeekableRead for T {}

/// An opened source together with what a state snapshot must report about it.
pub struct OpenSource {
    pub reader: Box<dyn SeekableRead>,
    pub kind: Source,
    /// False when the upstream host answered a range request with the full resource
    /// instead of partial content. The source then degrades to sequential streaming and
    /// seeking is reported unsupported rather than blocking or silently doing nothing.
    pub seekable: bool,
}

/// Opens the episode's audio, using `local_path` when it names a complete verified copy
/// and streaming from the enclosure URL otherwise.
///
/// Called from the audio thread, which is not async, so it drives the streamed reader
/// through `runtime` rather than awaiting.
///
/// `range_support` is what an earlier open of the same enclosure learned; passing it skips
/// the probe, a whole request whose answer cannot change under a session.
pub fn open(
    runtime: &tokio::runtime::Handle,
    episode: &Episode,
    local_path: Option<&Path>,
    range_support: Option<bool>,
) -> Result<OpenSource> {
    if let Some(path) = local_path {
        match open_local(episode, path) {
            Ok(source) => return Ok(source),
            Err(error) => tracing::debug!(
                path = %path.display(),
                %error,
                "local copy unusable, falling back to streaming"
            ),
        }
    }
    open_streamed(runtime, episode, range_support)
}

/// Opens a local copy, rejecting anything whose length disagrees with the feed: a truncated
/// or half-written file must not be played as though it were the whole episode.
fn open_local(episode: &Episode, path: &Path) -> Result<OpenSource> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if episode.byte_len != 0 && len != episode.byte_len {
        return Err(Error::PlaybackFailed(format!(
            "{} holds {len} bytes but the feed declares {}",
            path.display(),
            episode.byte_len
        )));
    }
    Ok(OpenSource {
        reader: Box::new(BufReader::new(file)),
        kind: Source::Local,
        // A file on disk always seeks, and does so without touching the network
        seekable: true,
    })
}

fn open_streamed(
    runtime: &tokio::runtime::Handle,
    episode: &Episode,
    range_support: Option<bool>,
) -> Result<OpenSource> {
    let url: reqwest::Url = episode.enclosure_url.parse().map_err(|error| {
        Error::PlaybackFailed(format!(
            "{} is not a usable audio URL: {error}",
            episode.enclosure_url
        ))
    })?;
    let seekable = range_support.unwrap_or_else(|| honours_range_requests(runtime, &url));
    let reader = runtime
        .block_on(StreamDownload::new_http(
            url,
            TempStorageProvider::new(),
            Settings::default()
                .prefetch_bytes(PREFETCH_BYTES)
                .retry_timeout(READ_TIMEOUT),
        ))
        .map_err(|error| Error::PlaybackFailed(error.to_string()))?;
    Ok(OpenSource {
        reader: Box::new(reader),
        kind: Source::Stream,
        seekable,
    })
}

/// Asks the host whether it serves ranges, and reports what it said.
///
/// `stream-download` reads `Accept-Ranges` but keeps the answer to itself, and a source
/// that cannot seek has to be reported as such rather than discovered by a seek that
/// stalls waiting for the transfer to catch up.
///
/// `HEAD` rather than `GET`, because a host that ignores the range header answers a `GET`
/// by beginning to send the whole 441 MB enclosure - and that is the very case this
/// exists to detect, so it is the case that would pay most. A `HEAD` carries no body to
/// abandon. Both answers count: a host may reply `206` to the range, or `200` with
/// `Accept-Ranges: bytes`, and either means a later range request will be honoured.
fn honours_range_requests(runtime: &tokio::runtime::Handle, url: &reqwest::Url) -> bool {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(mfp_core::USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    });
    runtime.block_on(async {
        match client
            .head(url.clone())
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()
            .await
        {
            Ok(response) => {
                response.status() == reqwest::StatusCode::PARTIAL_CONTENT
                    || response
                        .headers()
                        .get(reqwest::header::ACCEPT_RANGES)
                        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"bytes"))
            }
            Err(error) => {
                tracing::debug!(%error, "range probe failed, treating the source as unseekable");
                false
            }
        }
    })
}

// == Sample Tap ==

/// How many interleaved samples the tap keeps: one full transform at up to eight channels,
/// so the analyser has a complete window whatever the decoder's channel layout turns out
/// to be.
const TAP_CAPACITY: usize = FFT_SIZE * 8;

/// The most recent samples pulled through a [`Tap`].
///
/// Neither side ever waits on the other. The audio path drops a sample rather than block
/// on the analyser, and the analyser skips a frame rather than block on the audio path:
/// losing analysis work is what the spec asks for, delaying a sample is not.
#[derive(Clone)]
pub struct SampleTap(Arc<Mutex<Ring>>);

struct Ring {
    samples: Box<[f32]>,
    /// Where the next sample goes
    write: usize,
    /// False until `write` has wrapped once, so a barely started stream is not analysed
    /// with the silence it was allocated with
    wrapped: bool,
    /// What the samples held are interleaved at
    channels: usize,
}

impl SampleTap {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Ring {
            samples: vec![0.0; TAP_CAPACITY].into_boxed_slice(),
            write: 0,
            wrapped: false,
            channels: 1,
        })))
    }

    /// The samples held, oldest first, with the channel count they are interleaved at.
    ///
    /// `None` when the buffer is momentarily taken by the audio path, a frame the caller
    /// skips rather than waits for.
    pub fn take(&self) -> Option<(Vec<f32>, usize)> {
        let ring = self.try_ring()?;
        let samples = if ring.wrapped {
            let (newest, oldest) = ring.samples.split_at(ring.write);
            oldest.iter().chain(newest.iter()).copied().collect()
        } else {
            ring.samples[..ring.write].to_vec()
        };
        Some((samples, ring.channels))
    }

    /// Forgets everything held. Called when the decoder chain is torn down, so audio from
    /// before a seek is never analysed together with the audio that replaced it.
    pub fn clear(&self) {
        let mut ring = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        ring.write = 0;
        ring.wrapped = false;
    }

    /// The buffer, unless it is momentarily taken.
    ///
    /// Poisoning is recovered: the ring is plain data with no invariant a panic elsewhere
    /// could break, and refusing it would silence the analyser for the life of the daemon,
    /// indistinguishable from the outside from one that has simply gone quiet.
    fn try_ring(&self) -> Option<MutexGuard<'_, Ring>> {
        match self.0.try_lock() {
            Ok(ring) => Some(ring),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// Records one sample, or drops it if the buffer is taken.
    fn push(&self, sample: f32, channels: usize) {
        let Some(mut ring) = self.try_ring() else {
            return;
        };
        let write = ring.write;
        ring.samples[write] = sample;
        ring.write = write + 1;
        if ring.write == TAP_CAPACITY {
            ring.write = 0;
            ring.wrapped = true;
        }
        ring.channels = channels;
    }

    /// Holds the buffer the way a slow analyser would.
    #[cfg(test)]
    fn hold(&self) -> MutexGuard<'_, Ring> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for SampleTap {
    fn default() -> Self {
        Self::new()
    }
}

/// A `Source` adapter that copies what is pulled through it into a [`SampleTap`], and is
/// otherwise transparent: every sample is returned exactly as the inner source produced it,
/// and everything the player asks about the stream, seeks included, is forwarded untouched.
pub struct Tap<S> {
    inner: S,
    tap: SampleTap,
}

impl<S> Tap<S> {
    pub fn new(inner: S, tap: SampleTap) -> Self {
        Self { inner, tap }
    }
}

impl<S: rodio::Source> Iterator for Tap<S> {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.inner.next()?;
        self.tap
            .push(sample, usize::from(self.inner.channels().get()));
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: rodio::Source> rodio::Source for Tap<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> std::result::Result<(), rodio::source::SeekError> {
        self.inner.try_seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use rodio::Source as _;

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

    fn buffer(samples: &[f32], channels: u16) -> rodio::buffer::SamplesBuffer {
        rodio::buffer::SamplesBuffer::new(
            std::num::NonZero::new(channels).unwrap(),
            std::num::NonZero::new(44_100).unwrap(),
            samples.to_vec(),
        )
    }

    /// The tap is on the audio path, so the one thing it must never do is change what
    /// comes out of it.
    #[test]
    fn the_tap_yields_exactly_the_samples_it_was_given() {
        let samples: Vec<f32> = (0..4_096).map(|n| (n % 97) as f32 / 97.0 - 0.5).collect();
        let tapped = Tap::new(buffer(&samples, 2), SampleTap::new());

        assert_eq!(tapped.channels().get(), 2);
        assert_eq!(tapped.sample_rate().get(), 44_100);
        assert_eq!(tapped.collect::<Vec<_>>(), samples);
    }

    #[test]
    fn the_tap_holds_the_samples_that_passed_through_it() {
        let samples: Vec<f32> = (0..1_000).map(|n| n as f32).collect();
        let tap = SampleTap::new();
        Tap::new(buffer(&samples, 2), tap.clone()).for_each(drop);

        let (held, channels) = tap.take().expect("the buffer is free");
        assert_eq!(channels, 2);
        assert_eq!(held, samples);
    }

    #[test]
    fn the_tap_keeps_the_most_recent_samples_once_it_has_wrapped() {
        let samples: Vec<f32> = (0..TAP_CAPACITY + 500).map(|n| n as f32).collect();
        let tap = SampleTap::new();
        Tap::new(buffer(&samples, 1), tap.clone()).for_each(drop);

        let (held, _) = tap.take().expect("the buffer is free");
        assert_eq!(held.len(), TAP_CAPACITY);
        assert_eq!(held, samples[500..]);
    }

    #[test]
    fn clearing_the_tap_forgets_what_it_was_holding() {
        let tap = SampleTap::new();
        Tap::new(buffer(&[0.5; 64], 1), tap.clone()).for_each(drop);
        tap.clear();

        let (held, _) = tap.take().expect("the buffer is free");
        assert!(held.is_empty(), "the tap kept {} samples", held.len());
    }

    /// What "analysis never stalls audio" means in practice: with the buffer held for
    /// far longer than a frame, the samples still come through at once and unchanged.
    /// The analysis work is what gets dropped.
    #[test]
    fn a_held_buffer_never_delays_the_samples_passing_through_the_tap() {
        let tap = SampleTap::new();
        let holder_tap = tap.clone();
        let (holding, held) = std::sync::mpsc::channel();
        let (release, releasing) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let guard = holder_tap.hold();
            holding.send(()).expect("the test is waiting");
            releasing.recv().expect("the test releases the buffer");
            drop(guard);
        });
        held.recv().expect("the holder took the buffer");

        let samples: Vec<f32> = (0..200_000).map(|n| (n % 89) as f32 / 89.0).collect();
        let started = std::time::Instant::now();
        let pulled: Vec<f32> = Tap::new(buffer(&samples, 2), tap).collect();
        let elapsed = started.elapsed();

        release.send(()).expect("the holder is waiting");
        holder.join().expect("the holder thread");

        assert_eq!(
            pulled, samples,
            "a dropped analysis frame changed the audio"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "pulling waited {elapsed:?} on the analyser"
        );
    }

    /// A panic elsewhere must not cost the spectrum for the life of the daemon. The ring
    /// holds no invariant a panic could break, so the buffer is recovered rather than
    /// refused, and the tap goes on recording and reporting.
    #[test]
    fn a_poisoned_buffer_does_not_silence_the_tap_forever() {
        let tap = SampleTap::new();
        let poisoner = tap.clone();
        std::thread::spawn(move || {
            let _guard = poisoner.hold();
            panic!("poisoning the tap");
        })
        .join()
        .expect_err("the thread panics on purpose");

        tap.clear();
        let samples: Vec<f32> = (0..256).map(|n| n as f32).collect();
        Tap::new(buffer(&samples, 1), tap.clone()).for_each(drop);

        let (held, channels) = tap.take().expect("the buffer is recovered, not refused");
        assert_eq!(channels, 1);
        assert_eq!(held, samples);
    }

    #[test]
    fn a_complete_local_copy_opens_as_a_local_seekable_source() {
        let fixture = testing::Fixture::new();
        let runtime = runtime();
        let opened = open(
            runtime.handle(),
            &fixture.episode,
            Some(&fixture.path),
            None,
        )
        .unwrap();
        assert_eq!(opened.kind, Source::Local);
        assert!(opened.seekable);
    }

    #[test]
    fn a_local_copy_builds_a_working_decoder() {
        let fixture = testing::Fixture::new();
        let runtime = runtime();
        let opened = open(
            runtime.handle(),
            &fixture.episode,
            Some(&fixture.path),
            None,
        )
        .unwrap();
        let mut decoder = rodio::Decoder::new(opened.reader).unwrap();
        assert_eq!(testing::plateau(&mut decoder), 0);
    }

    #[test]
    fn a_local_copy_whose_length_disagrees_with_the_feed_is_refused() {
        let fixture = testing::Fixture::new();
        let mut episode = fixture.episode;
        episode.byte_len += 1;
        assert!(open_local(&episode, &fixture.path).is_err());
    }

    #[test]
    fn a_local_copy_that_cannot_be_opened_is_refused() {
        let fixture = testing::Fixture::new();
        assert!(
            open_local(
                &fixture.episode,
                &fixture.directory.path().join("absent.wav")
            )
            .is_err()
        );
    }

    #[test]
    fn an_unreadable_local_copy_falls_back_to_streaming_rather_than_failing_the_load() {
        // The enclosure URL is unroutable, so the fallback is observable as a streaming
        // failure rather than as the local error the truncated file would have produced
        let fixture = testing::Fixture::new();
        let truncated = fixture.directory.path().join("truncated.wav");
        File::create(&truncated)
            .unwrap()
            .write_all(b"nope")
            .unwrap();
        let mut episode = fixture.episode;
        episode.enclosure_url = "not a url".into();

        let runtime = runtime();
        let Err(error) = open(runtime.handle(), &episode, Some(&truncated), None) else {
            panic!("the unroutable enclosure URL must not open");
        };
        assert!(
            error.to_string().contains("not a usable audio URL"),
            "expected the streaming path to be reached, got {error}"
        );
    }
}
