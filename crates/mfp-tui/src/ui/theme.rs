//! The interface's colours, as a set of named roles rather than a set of hues.
//!
//! Every widget asks for a role, so no pane names a hue of its own.
//!
//! Four hues, each with one job. Neutrals carry the words: white is the subject of a row,
//! grey is whatever measures it, and the darkest grey is chrome. Green is the only colour
//! that means "here", violet the one that means "playing", blue titles the panes, and gold
//! and red are kept for the two states worth interrupting a reader for. Nothing else gets a
//! hue, because a colour used for a second purpose stops being a signal.

use ratatui::style::Color;

/// How many steps the analyser's colour ramp has.
pub const SPECTRUM_STEPS: usize = 4;

/// The palette, in roles.
pub struct Theme {
    /// The pane background. Painted explicitly rather than left to the terminal so the
    /// interface looks the same inside a terminal configured for something else.
    pub bg: Color,
    /// A row's subject: an episode name, a track name, the time elapsed.
    pub fg: Color,
    /// Whatever measures the subject: durations, counts, the transport's own words.
    pub dim: Color,
    /// Chrome: row marks, index numbers, placeholders, the key legend.
    pub faint: Color,
    /// Pane borders and rules.
    pub line: Color,
    /// The subject: the selection, the focused pane's border, the search caret.
    pub accent: Color,
    /// The episode that is loaded, wherever it appears, and the word for what playback is
    /// doing. Its own hue rather than the accent's: the state word shares a row with the
    /// episode title, and two greens side by side read as one span.
    pub playing: Color,
    /// A completed download. The accent again: a local copy is a good state, not a third
    /// kind of green.
    pub ok: Color,
    /// A favourite, and a download in flight.
    pub warn: Color,
    /// A failure.
    pub err: Color,
    /// The analyser's ramp, by how high up the pane a mark sits rather than by how loud
    /// its column is. The site's own four: green along the floor, then yellow, orange, and
    /// pink at the top, so a loud frame is visibly hotter than a quiet one.
    pub spectrum: [Color; SPECTRUM_STEPS],
    /// Every pane's title. A border is chrome and a title is a label, so the two are
    /// never the same colour even on the focused pane.
    pub title: Color,
}

const fn rgb(value: u32) -> Color {
    Color::Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

pub const PALETTE: Theme = Theme {
    bg: rgb(0x1f1f1e),
    fg: rgb(0xe4e2de),
    dim: rgb(0x9a958c),
    faint: rgb(0x6b655f),
    line: rgb(0x373634),
    accent: rgb(0x5ca88a),
    playing: rgb(0xa88ccc),
    ok: rgb(0x5ca88a),
    warn: rgb(0xd4a85a),
    err: rgb(0xc87a72),
    spectrum: [rgb(0x3fcf94), rgb(0xd8d84a), rgb(0xe8963c), rgb(0xe86ec7)],
    title: rgb(0x6a9fd4),
};
