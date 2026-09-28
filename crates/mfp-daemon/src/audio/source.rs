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

const PREFETCH_BYTES: u64 = 64 * 1024;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

pub trait SeekableRead: Read + Seek + Send + Sync {}

impl<T: Read + Seek + Send + Sync> SeekableRead for T {}

pub struct OpenSource {
    pub reader: Box<dyn SeekableRead>,
    pub kind: Source,

    pub seekable: bool,
}

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

const TAP_CAPACITY: usize = FFT_SIZE * 8;

#[derive(Clone)]
pub struct SampleTap(Arc<Mutex<Ring>>);

struct Ring {
    samples: Box<[f32]>,

    write: usize,

    wrapped: bool,

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

    pub fn clear(&self) {
        let mut ring = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        ring.write = 0;
        ring.wrapped = false;
    }

    fn try_ring(&self) -> Option<MutexGuard<'_, Ring>> {
        match self.0.try_lock() {
            Ok(ring) => Some(ring),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

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
