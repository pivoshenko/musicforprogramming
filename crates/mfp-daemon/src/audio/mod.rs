pub mod engine;
pub mod seek;
pub mod source;
pub mod spectrum;

#[cfg(test)]
pub(crate) mod testing {

    use std::io::Write;
    use std::path::PathBuf;

    use mfp_core::Episode;

    pub const SAMPLE_RATE: u32 = 8_000;

    pub const DURATION_SECS: u32 = 6;

    const STEP: i16 = 4_000;

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

    pub fn plateau(samples: &mut impl Iterator<Item = rodio::Sample>) -> i64 {
        let sample = samples
            .find(|sample| sample.is_finite())
            .expect("the decoder produced no audio");
        (f64::from(sample) * 32_768.0 / f64::from(STEP)).round() as i64
    }
}
