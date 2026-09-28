use ratatui::style::Color;

pub const SPECTRUM_STEPS: usize = 4;

pub struct Theme {
    pub bg: Color,

    pub fg: Color,

    pub dim: Color,

    pub faint: Color,

    pub line: Color,

    pub accent: Color,

    pub playing: Color,

    pub ok: Color,

    pub warn: Color,

    pub err: Color,

    pub spectrum: [Color; SPECTRUM_STEPS],

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
