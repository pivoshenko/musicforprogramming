use std::sync::Arc;

use mfp_core::protocol::{
    SPECTRUM_BINS, SPECTRUM_MAX_DB, SPECTRUM_MIN_DB, SPECTRUM_SMOOTHING, Spectrum,
};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

pub const FFT_SIZE: usize = SPECTRUM_BINS * 2;

const MAGNITUDE_SCALE: f32 = 1.0 / FFT_SIZE as f32;

const DECIBEL_SPAN: f32 = SPECTRUM_MAX_DB - SPECTRUM_MIN_DB;

pub struct Analyser {
    fft: Arc<dyn Fft<f32>>,

    window: Vec<f32>,

    smoothed: Vec<f32>,

    buffer: Vec<Complex<f32>>,
}

impl Analyser {
    pub fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let window = (0..FFT_SIZE)
            .map(|index| {
                let phase = std::f32::consts::TAU * index as f32 / FFT_SIZE as f32;
                0.5 * (1.0 - phase.cos())
            })
            .collect();
        Self {
            fft,
            window,
            smoothed: vec![0.0; SPECTRUM_BINS],
            buffer: vec![Complex::default(); FFT_SIZE],
        }
    }

    pub fn analyse(&mut self, samples: &[f32], channels: usize) -> Spectrum {
        self.fill(samples, channels);
        self.fft.process(&mut self.buffer);

        let mut bytes = Vec::with_capacity(SPECTRUM_BINS);
        for bin in 0..SPECTRUM_BINS {
            let magnitude = self.buffer[bin].norm() * MAGNITUDE_SCALE;
            let smoothed =
                SPECTRUM_SMOOTHING * self.smoothed[bin] + (1.0 - SPECTRUM_SMOOTHING) * magnitude;
            self.smoothed[bin] = smoothed;
            let decibels = 20.0 * smoothed.log10();
            let scaled = 255.0 * (decibels - SPECTRUM_MIN_DB) / DECIBEL_SPAN;

            bytes.push(scaled.clamp(0.0, 255.0) as u8);
        }
        Spectrum(bytes)
    }

    fn fill(&mut self, samples: &[f32], channels: usize) {
        let channels = channels.max(1);
        let frames = samples.len() / channels;
        let taken = frames.min(FFT_SIZE);
        let skipped = (frames - taken) * channels;
        let padding = FFT_SIZE - taken;

        self.buffer[..padding].fill(Complex::default());
        for frame in 0..taken {
            let start = skipped + frame * channels;
            let sum: f32 = samples[start..start + channels]
                .iter()
                .map(|sample| if sample.is_finite() { *sample } else { 0.0 })
                .sum();
            let index = padding + frame;
            self.buffer[index] = Complex::new(sum / channels as f32 * self.window[index], 0.0);
        }
    }
}

impl Default for Analyser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {

    #[test]
    #[ignore = "benchmark, run explicitly"]
    fn bench_analyse() {
        use std::time::Instant;
        let samples: Vec<f32> = (0..FFT_SIZE * 2)
            .map(|n| ((n as f32) * 0.017).sin() * 0.5)
            .collect();
        let mut analyser = Analyser::new();

        const N: u32 = 5_000;
        let start = Instant::now();
        for _ in 0..N {
            let _ = analyser.analyse(&samples, 2);
        }
        let each = start.elapsed() / N;
        println!(
            "analyse: {each:?} per frame, {:.4}% of a 50ms budget",
            each.as_secs_f64() / 0.050 * 100.0
        );
    }

    use super::*;

    const SAMPLE_RATE: f32 = 44_100.0;

    fn sine(frequency: f32, amplitude: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|index| {
                let phase = std::f32::consts::TAU * frequency * index as f32 / SAMPLE_RATE;
                amplitude * phase.sin()
            })
            .collect()
    }

    fn noise(amplitude: f32, len: usize) -> Vec<f32> {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let unit = (state >> 40) as f32 / 8_388_608.0 - 1.0;
                amplitude * unit
            })
            .collect()
    }

    fn settled(analyser: &mut Analyser, samples: &[f32], channels: usize) -> Spectrum {
        let mut spectrum = Spectrum::silent();
        for _ in 0..40 {
            spectrum = analyser.analyse(samples, channels);
        }
        spectrum
    }

    #[test]
    fn a_spectrum_carries_exactly_the_published_number_of_bins() {
        let spectrum = Analyser::new().analyse(&sine(1_000.0, 0.5, FFT_SIZE), 1);
        assert_eq!(spectrum.0.len(), SPECTRUM_BINS);
    }

    #[test]
    fn silence_floors_every_bin() {
        let spectrum = settled(&mut Analyser::new(), &vec![0.0; FFT_SIZE], 1);
        assert!(
            spectrum.0.iter().all(|&bin| bin == 0),
            "silence produced a signal"
        );
    }

    #[test]
    fn no_samples_at_all_floor_every_bin() {
        let spectrum = Analyser::new().analyse(&[], 2);
        assert_eq!(spectrum.0.len(), SPECTRUM_BINS);
        assert!(spectrum.0.iter().all(|&bin| bin == 0));
    }

    #[test]
    fn a_full_scale_tone_peaks_in_the_bin_its_frequency_belongs_to() {
        const BIN: usize = 100;
        let frequency = SAMPLE_RATE * BIN as f32 / FFT_SIZE as f32;
        let spectrum = settled(&mut Analyser::new(), &sine(frequency, 1.0, FFT_SIZE), 1);

        assert_eq!(spectrum.0[BIN], 255, "the tone's own bin is not saturated");

        for (index, &bin) in spectrum.0.iter().enumerate() {
            if index.abs_diff(BIN) > 4 {
                assert_eq!(bin, 0, "bin {index} carries {bin} for a tone at bin {BIN}");
            }
        }
    }

    #[test]
    fn a_stereo_tone_is_downmixed_rather_than_read_as_twice_the_audio() {
        const BIN: usize = 200;
        let frequency = SAMPLE_RATE * BIN as f32 / FFT_SIZE as f32;
        let mono = sine(frequency, 0.2, FFT_SIZE);
        let stereo: Vec<f32> = mono.iter().flat_map(|&sample| [sample, sample]).collect();

        let from_mono = settled(&mut Analyser::new(), &mono, 1);
        let from_stereo = settled(&mut Analyser::new(), &stereo, 2);
        assert_eq!(from_mono, from_stereo);
    }

    #[test]
    fn a_different_signal_yields_a_different_spectrum() {
        let low = settled(&mut Analyser::new(), &sine(200.0, 0.2, FFT_SIZE), 1);
        let high = settled(&mut Analyser::new(), &sine(8_000.0, 0.2, FFT_SIZE), 1);
        assert_ne!(low, high);
    }

    #[test]
    fn every_byte_stays_in_range_for_arbitrary_input() {
        let mut analyser = Analyser::new();
        let hostile = [
            vec![f32::NAN; FFT_SIZE],
            vec![f32::INFINITY; FFT_SIZE],
            vec![f32::NEG_INFINITY; FFT_SIZE],
            vec![1e30; FFT_SIZE],
            vec![-1e-30; FFT_SIZE],
            noise(1e6, FFT_SIZE),
            vec![0.3; 7],
        ];
        for samples in &hostile {
            for channels in [1, 2, 3, 8] {
                let spectrum = analyser.analyse(samples, channels);
                assert_eq!(spectrum.0.len(), SPECTRUM_BINS);
            }
        }
    }

    #[test]
    fn a_non_finite_sample_does_not_leave_the_analyser_stuck() {
        let mut analyser = Analyser::new();
        for hostile in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            analyser.analyse(&vec![hostile; FFT_SIZE], 1);
        }
        assert!(
            settled(&mut analyser, &vec![0.0; FFT_SIZE], 1)
                .0
                .iter()
                .all(|&bin| bin == 0),
            "a non-finite frame left the analyser stuck"
        );
    }

    #[test]
    fn a_loud_signal_saturates_rather_than_wrapping_to_silence() {
        let spectrum = settled(&mut Analyser::new(), &vec![1e12; FFT_SIZE], 1);
        assert_eq!(spectrum.0[0], 255);
    }

    #[test]
    fn smoothing_climbs_towards_the_signal_rather_than_arriving_at_once() {
        let samples = noise(0.05, FFT_SIZE);
        let mut analyser = Analyser::new();

        let first = analyser.analyse(&samples, 1);
        let second = analyser.analyse(&samples, 1);
        let peak = |spectrum: &Spectrum| *spectrum.0.iter().max().expect("a bin");
        assert!(
            peak(&second) > peak(&first),
            "the second frame did not rise on the first: {} then {}",
            peak(&first),
            peak(&second)
        );
    }
}
