//! The moving parts: the analyser's bin mapping, the marquee, and the glyph ramps
//! everything is drawn out of.
//!
//! Nothing here reads a clock: every function purely maps a tick count the event loop owns
//! to that tick's frame, so the loop can stop ticking when playback is not running and
//! still draw a correct resting frame.

use mfp_core::protocol::Spectrum;

/// Milliseconds between animation ticks while something is moving. Twenty frames a
/// second: fast enough that the analyser reads as audio, slow enough to cost nothing.
pub const TICK_MS: u64 = 50;

/// Ticks between marquee steps. Five characters a second is readable; twenty is a blur.
const MARQUEE_EVERY: u64 = 4;

/// What separates the end of a scrolling title from its own beginning.
const MARQUEE_GAP: &str = "   ///   ";

/// Vertical eighths, indexed by how many eighths of the cell are filled.
const BLOCKS: [char; 9] = [
    ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
    '\u{2588}',
];

/// The glyph for a cell filled `eighths` of the way up.
pub fn block(eighths: usize) -> char {
    BLOCKS[eighths.min(8)]
}

/// Marks that sit at successive heights inside one cell, lowest first.
///
/// The analyser draws one of these per column rather than a filled bar, as the site does:
/// a row of marks at varying heights reads as a waveform, where stacked blocks read as a
/// bar chart. Each glyph is chosen for where its ink sits in the cell, not for its weight,
/// and the set is wider than the eight sub-cell steps so neighbouring columns at similar
/// heights still differ from one another.
const MARKS: [char; 10] = ['_', '.', ',', ':', '\u{2022}', 'o', '-', '*', '^', '\''];

/// How many heights a mark can take inside one cell.
///
/// The ramp is the resolution rather than the eighths the block glyphs use: quantising to
/// eighths first would leave two of these glyphs unreachable, and an unreachable glyph is
/// one the analyser paid for and never draws.
pub const MARK_STEPS: usize = MARKS.len();

/// The mark for a column whose peak sits `step` of [`MARK_STEPS`] up its cell.
pub fn mark(step: usize) -> char {
    MARKS[step.min(MARK_STEPS - 1)]
}

/// The window of `text` visible at `tick`, scrolling when it does not fit.
///
/// Text that fits is returned unchanged, rather than animated into unreadability for
/// consistency with a long title.
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

/// Truncates to `width`, marking the cut with a single ellipsis character rather than
/// three cells of dots.
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

/// The lowest FFT bin any column reads.
///
/// Bin 0 is DC and carries no music; starting at 1 keeps a constant offset out of the
/// leftmost bar.
const FIRST_BIN: usize = 1;

/// The highest FFT bin any column reads.
///
/// A 2048-point FFT at 44.1 kHz puts bin 512 at about 11 kHz. This catalog sits near the
/// floor above it, so including it would spend a third of the analyser's width on nothing.
const LAST_BIN: usize = 512;

/// Collapses a spectrum's 1024 bins into `columns` magnitudes in `0.0..=1.0`.
///
/// Columns are spaced logarithmically, an octave per column: linear spacing would put six
/// of eight columns above 5 kHz, where this catalog has almost no energy, and cram the
/// bass - the part one actually sees moving - into the first column. Each column takes the
/// loudest bin in its range rather than the mean, which over a wide high-frequency range
/// washes out every transient.
pub fn spectrum_columns(spectrum: &Spectrum, columns: usize) -> Vec<f32> {
    if columns == 0 {
        return Vec::new();
    }

    let span = (LAST_BIN as f32 / FIRST_BIN as f32).ln();
    let mut out = Vec::with_capacity(columns);

    // Ranges are consecutive rather than each computed from the curve: at the low end the
    // curve is narrower than one bin, and `floor(low)..ceil(high)` would hand one bin to
    // several columns, so one tone would light a smear of bars
    let mut first = FIRST_BIN;
    for column in 0..columns {
        let high = FIRST_BIN as f32 * (span * (column + 1) as f32 / columns as f32).exp();
        // An analyser wider than the band it reads runs `first` off the end; past that a
        // column would take an inverted range, read nothing, and draw a floor that never
        // moves, so they share the topmost bin instead
        let low = first.min(LAST_BIN - 1);
        let last = (high as usize).max(low + 1).min(LAST_BIN);

        let peak = (low..last).map(|bin| spectrum.bin(bin)).max().unwrap_or(0);
        out.push(f32::from(peak) / 255.0);
        first = last;
    }

    out
}

/// The ramp step a bar of height `filled` rows reaches at row `row`, counted from the
/// bottom, given `steps` colours over `height` rows.
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
        // Bin 2 is around 90 Hz: linear spacing would share its column with everything up
        // to 350 Hz, log spacing gives it the low end alone and lights nothing above it
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

    /// Wider than the 511 bins the analyser reads, so the mapping runs out of band. Every
    /// column must still read a bin: one that reads none draws a floor that never moves,
    /// and a whole dead half of the pane looks like a broken component.
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

    /// Every sub-cell step must land on a glyph, and the ramp must run end to end: a step
    /// that never reaches the top glyph wastes the tallest mark the analyser has.
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
