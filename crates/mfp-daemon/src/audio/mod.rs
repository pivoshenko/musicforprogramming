//! Audio output and transport control.
//!
//! One dedicated OS thread owns the player and consumes a command channel; it never
//! awaits, and the async side never blocks on it.

pub mod engine;
pub mod seek;
pub mod source;
pub mod spectrum;

#[cfg(test)]
pub(crate) mod testing {
    //! A synthetic episode the decode and seek tests can position themselves in.
    //!
    //! Nothing here touches the network or an audio device, so the tests proving a rebuilt
    //! chain really produces the audio at its target run everywhere. The fixture is
    //! generated rather than checked in so the value-to-timestamp relation is stated in code.

    use std::io::Write;
    use std::path::PathBuf;

    use mfp_core::Episode;

    /// Deliberately low: the fixture only has to be decodable and seekable, and a small
    /// one keeps the test suite fast.
    pub const SAMPLE_RATE: u32 = 8_000;

    /// How long the fixture runs. Whole seconds, so every second is one plateau.
    pub const DURATION_SECS: u32 = 6;

    /// The amplitude step between one second and the next. Six plateaus at this step stay
    /// inside `i16`, so the second a sample belongs to is recoverable from its value exactly.
    const STEP: i16 = 4_000;

    /// A generated WAV episode on disk, together with the catalog entry naming it.
    pub struct Fixture {
        pub directory: tempfile::TempDir,
        pub path: PathBuf,
        pub episode: Episode,
    }

    impl Fixture {
        pub fn new() -> Self {
            let directory = tempfile::tempdir().expect("a temporary directory");
            let path = directory.path().join("fixture.wav");
            let bytes = wav();
            std::fs::File::create(&path)
                .and_then(|mut file| file.write_all(&bytes))
                .expect("the fixture is writable");

            let episode = Episode {
                bundle_title: None,
                special: false,
                title: "Fixture".into(),
                link: "https://musicforprogramming.net/fixture".into(),
                // Unroutable on purpose: a test that reaches the network fails in CI
                enclosure_url: "http://127.0.0.1:1/fixture.wav".into(),
                byte_len: bytes.len() as u64,
                duration_secs: u64::from(DURATION_SECS),
                published_at: 1_700_000_000,
                slug: Some("fixture".into()),
                order: Some(1),
                tracklist: None,
                body: None,
                links: None,
            };

            Self {
                directory,
                path,
                episode,
            }
        }
    }

    /// A mono 16-bit PCM WAV whose every sample states which second it belongs to.
    fn wav() -> Vec<u8> {
        let sample_count = SAMPLE_RATE * DURATION_SECS;
        let mut data = Vec::with_capacity(sample_count as usize * 2);
        for index in 0..sample_count {
            let second = (index / SAMPLE_RATE) as i16;
            data.extend_from_slice(&(second * STEP).to_le_bytes());
        }

        let mut wav = Vec::with_capacity(data.len() + 44);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        wav
    }

    /// The second of the fixture the next samples belong to.
    ///
    /// Recovered from the audio the decoder is actually producing, rather than from a
    /// position readout a wedged decoder would happily keep advancing.
    pub fn plateau(samples: &mut impl Iterator<Item = rodio::Sample>) -> i64 {
        let sample = samples
            .find(|sample| sample.is_finite())
            .expect("the decoder produced no audio");
        (f64::from(sample) * 32_768.0 / f64::from(STEP)).round() as i64
    }
}
