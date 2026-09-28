use mfp_core::protocol::Spectrum;

pub const TICK_MS: u64 = 50;

const MARQUEE_EVERY: u64 = 4;

const MARQUEE_GAP: &str = "   ///   ";

const BLOCKS: [char; 9] = [
    ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
    '\u{2588}',
];

pub fn block(eighths: usize) -> char {
    BLOCKS[eighths.min(8)]
}

const MARKS: [char; 10] = ['_', '.', ',', ':', '\u{2022}', 'o', '-', '*', '^', '\''];

pub const MARK_STEPS: usize = MARKS.len();

pub fn mark(step: usize) -> char {
    MARKS[step.min(MARK_STEPS - 1)]
}

pub fn marquee(text: &str, width: usize, tick: u64) -> String {
    let chars: Vec<char> = text.chars().collect();
    if width == 0 {
        return String::new();
    }
    if chars.len() <= width {
        return text.to_owned();
    }

    let looped: Vec<char> = chars
        .iter()
        .copied()
        .chain(MARQUEE_GAP.chars())
        .cycle()
        .take(chars.len() + MARQUEE_GAP.chars().count() + width)
        .collect();
    let period = chars.len() + MARQUEE_GAP.chars().count();
    let offset = ((tick / MARQUEE_EVERY) as usize) % period;

    looped[offset..offset + width].iter().collect()
}

pub fn elide(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return chars.iter().take(width).collect();
    }
    chars[..width - 1].iter().collect::<String>() + "\u{2026}"
}

const FIRST_BIN: usize = 1;

const LAST_BIN: usize = 512;

pub fn spectrum_columns(spectrum: &Spectrum, columns: usize) -> Vec<f32> {
    if columns == 0 {
        return Vec::new();
    }

    let span = (LAST_BIN as f32 / FIRST_BIN as f32).ln();
    let mut out = Vec::with_capacity(columns);

    let mut first = FIRST_BIN;
    for column in 0..columns {
        let high = FIRST_BIN as f32 * (span * (column + 1) as f32 / columns as f32).exp();

        let low = first.min(LAST_BIN - 1);
        let last = (high as usize).max(low + 1).min(LAST_BIN);

        let peak = (low..last).map(|bin| spectrum.bin(bin)).max().unwrap_or(0);
        out.push(f32::from(peak) / 255.0);
        first = last;
    }

    out
}

pub fn ramp_step(row: usize, height: usize, steps: usize) -> usize {
    if height == 0 || steps == 0 {
        return 0;
    }
    (row * steps / height).min(steps - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_that_fits_never_moves() {
        for tick in 0..40 {
            assert_eq!(marquee("79: Corticyte", 20, tick), "79: Corticyte");
        }
    }

    #[test]
    fn a_title_that_does_not_fit_scrolls_and_returns_to_its_start() {
        let text = "79: Corticyte by Datassette";
        let width = 10;
        let first = marquee(text, width, 0);
        assert_eq!(first.chars().count(), width);

        let period = (text.chars().count() + MARQUEE_GAP.chars().count()) as u64 * MARQUEE_EVERY;
        assert_eq!(marquee(text, width, period), first);
        assert_ne!(marquee(text, width, MARQUEE_EVERY), first);
    }

    #[test]
    fn every_marquee_window_is_exactly_the_requested_width() {
        let text = "a rather long episode title that will not fit";
        for tick in 0..200 {
            assert_eq!(marquee(text, 12, tick).chars().count(), 12);
        }
    }

    #[test]
    fn a_zero_width_marquee_is_empty_rather_than_a_panic() {
        assert_eq!(marquee("anything", 0, 7), "");
    }

    #[test]
    fn eliding_marks_the_cut_and_never_exceeds_the_width() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("truncate me", 8), "truncat\u{2026}");
        assert_eq!(elide("truncate me", 1).chars().count(), 1);
        assert_eq!(elide("truncate me", 0), "");
    }

    #[test]
    fn spectrum_columns_are_normalised_and_counted() {
        let spectrum = Spectrum(vec![255; 1024]);
        let columns = spectrum_columns(&spectrum, 32);
        assert_eq!(columns.len(), 32);
        assert!(
            columns
                .iter()
                .all(|value| (*value - 1.0).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn silence_reads_as_zero_in_every_column() {
        let columns = spectrum_columns(&Spectrum::silent(), 24);
        assert!(columns.iter().all(|value| *value == 0.0));
    }

    #[test]
    fn a_short_spectrum_frame_reads_as_silence_rather_than_panicking() {
        let columns = spectrum_columns(&Spectrum(vec![128; 4]), 16);
        assert_eq!(columns.len(), 16);
    }

    #[test]
    fn columns_are_spaced_logarithmically_so_the_bass_is_not_one_column() {
        let mut bins = vec![0u8; 1024];
        bins[2] = 255;
        let columns = spectrum_columns(&Spectrum(bins), 32);

        let lit: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter(|(_, value)| **value > 0.9)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(lit.len(), 1, "one bin must light one column, not {lit:?}");
        assert!(lit[0] < 8, "a 90 Hz tone belongs in the left quarter");
    }

    #[test]
    fn the_top_of_the_analyser_is_a_higher_frequency_than_the_bottom() {
        let mut bins = vec![0u8; 1024];
        bins[400] = 255;
        let columns = spectrum_columns(&Spectrum(bins), 32);
        let lit = columns.iter().position(|value| *value > 0.9).unwrap();
        assert!(lit > 24, "an 8 kHz tone belongs on the right, not at {lit}");
    }

    #[test]
    fn an_analyser_wider_than_the_band_it_reads_has_no_dead_columns() {
        let columns = spectrum_columns(&Spectrum(vec![255; 1024]), 900);

        assert_eq!(columns.len(), 900);
        assert!(
            columns.iter().all(|value| *value > 0.9),
            "{} of 900 columns read nothing",
            columns.iter().filter(|value| **value <= 0.9).count()
        );
    }

    #[test]
    fn no_column_is_requested_beyond_the_ramp() {
        for height in 1..8 {
            for row in 0..height {
                assert!(ramp_step(row, height, 4) < 4);
            }
        }
    }

    #[test]
    fn glyph_ramps_saturate_rather_than_index_out_of_range() {
        assert_eq!(block(0), ' ');
        assert_eq!(block(8), '\u{2588}');
        assert_eq!(block(99), '\u{2588}');
        assert_eq!(mark(0), '_');
        assert_eq!(mark(MARK_STEPS - 1), '\'');
        assert_eq!(mark(99), '\'');
    }

    #[test]
    fn the_mark_ramp_is_used_end_to_end() {
        let drawn: Vec<char> = (0..MARK_STEPS).map(mark).collect();
        assert_eq!(drawn, MARKS, "a glyph the ramp holds is never drawn");
        assert!(
            drawn.windows(2).all(|pair| pair[0] != pair[1]),
            "two neighbouring steps draw the same glyph: {drawn:?}"
        );
    }
}
