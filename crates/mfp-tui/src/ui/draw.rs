//! The frame: how the panes are arranged, and the header, footer, and help overlay that
//! surround them.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::anim;
use super::app::{App, Mode};
use super::panes;
use super::theme::Theme;

/// Below this the two-column layout is dropped for a single column.
const NARROW: u16 = 76;

/// Below either of these there is nothing worth drawing, and the interface says so rather
/// than drawing a mangled frame.
const MIN_WIDTH: u16 = 36;
const MIN_HEIGHT: u16 = 12;

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();

    let ground = {
        let theme = app.theme();
        Style::default().bg(theme.bg).fg(theme.fg)
    };
    frame.render_widget(ratatui::widgets::Block::default().style(ground), area);

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        too_small(frame, area, app.theme());
        return;
    }

    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    header(frame, head, app, app.theme());
    footer(frame, foot, app, app.theme());

    if area.width < NARROW {
        // One column: the player and the analyser above the catalog. The track listing is
        // the pane that goes, because it is the one that needs width to be readable
        let [player, catalog] = panes::split_right(body);
        panes::player(frame, player, app);
        panes::catalog(frame, catalog, app);
    } else {
        // A column of gap between the two, so the panes' borders never run as a doubled
        // vertical rule
        // Widened to `u32` first: `body.width` is whatever the terminal reports, and past
        // 1638 columns the multiply alone overflows `u16`
        let catalog_width = (u32::from(body.width) * 40 / 100).clamp(30, 52) as u16;
        let [left, _, right] = Layout::horizontal([
            Constraint::Length(catalog_width),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(body);

        panes::catalog(frame, left, app);
        let [player, tracks] = panes::split_right(right);
        panes::player(frame, player, app);
        panes::tracks(frame, tracks, app);
    }

    if app.mode == Mode::Help {
        help(frame, area, app.theme());
    }
}

fn too_small(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                "mfp",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!("Needs {MIN_WIDTH}x{MIN_HEIGHT}"),
                Style::default().fg(theme.faint),
            ),
        ])
        .style(Style::default().bg(theme.bg))
        .alignment(Alignment::Center),
        area,
    );
}

fn header(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    let mut left = vec![Span::styled(
        " musicforprogramming.net",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )];

    if app.disconnected {
        left.push(Span::styled(
            "  daemon unreachable",
            Style::default().fg(theme.err).add_modifier(Modifier::BOLD),
        ));
    }

    // Beside the unreachable marker rather than on the footer, so a refusal is still seen
    // without the key legend going away to show it
    if let Some(status) = &app.status {
        left.push(Span::styled(
            format!("  {}", status.text),
            Style::default().fg(theme.err).add_modifier(Modifier::BOLD),
        ));
    }

    // After the two markers worth interrupting for, and unbolded: an update is worth
    // mentioning for as long as it is available, not worth pulling the eye off a playing
    // episode every frame
    if let Some(latest) = &app.update {
        left.push(Span::styled(
            format!("  update {latest} available"),
            Style::default().fg(theme.warn),
        ));
    }

    // Appended after the markers so they keep their room on a narrow terminal and the
    // masthead is what gives way
    let used: usize = left.iter().map(|span| span.content.chars().count()).sum();
    left.extend(tagline(theme, (area.width as usize).saturating_sub(used)));

    frame.render_widget(
        Paragraph::new(Line::from(left)).style(Style::default().bg(theme.bg)),
        area,
    );
}

/// The site's tagline, beside the site name, dropped whole when the header is too narrow
/// to hold it.
///
/// The header is one row, so rather than clip mid-word the tagline goes entirely.
fn tagline(theme: &Theme, room: usize) -> Vec<Span<'static>> {
    const SENTENCE: &str = "A series of mixes intended for listening while programming to focus the brain and \
         inspire the mind.";

    // Three cells of gap, so the tagline is never flush against the site name
    const GAP: &str = "   ";
    if GAP.len() + SENTENCE.chars().count() > room {
        return Vec::new();
    }

    vec![
        Span::raw(GAP),
        Span::styled(SENTENCE, Style::default().fg(theme.dim)),
    ]
}

/// The key legend, replaced by the search line while a search is running.
///
/// Nothing else ever takes this row: it is the one place the bindings are always
/// readable, and a message that borrows it takes them away to say something the screen
/// has usually already shown.
fn footer(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &Theme) {
    if app.mode == Mode::Search {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" /", Style::default().fg(theme.accent)),
                Span::styled(
                    app.query.clone(),
                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    anim::block(if app.tick % 16 < 8 { 8 } else { 0 }).to_string(),
                    Style::default().fg(theme.accent),
                ),
                Span::styled(
                    format!("   {} match", app.matches.len()),
                    Style::default().fg(theme.faint),
                ),
                Span::styled("   enter keep  esc clear", Style::default().fg(theme.faint)),
            ]))
            .style(Style::default().bg(theme.bg)),
            area,
        );
        return;
    }

    let hints: &[(&str, &str)] = &[
        ("j/k", "Move"),
        ("\u{21b5}", "Play"),
        ("spc", "Pause"),
        ("u", "Stop"),
        ("n/p", "Track"),
        ("h/l", "Seek"),
        ("r", "Rand"),
        ("f", "Fav"),
        ("d", "Dl"),
        ("x", "Drop"),
        ("/", "Find"),
        ("?", "Help"),
        ("q", "Quit"),
    ];

    let mut spans = vec![Span::raw(" ")];
    for (key, what) in hints {
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default().fg(theme.accent),
        ));
        spans.push(Span::styled(
            format!(" {what} "),
            Style::default().fg(theme.faint),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg)),
        area,
    );
}

// == Help ==

fn help(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let bindings: &[(&str, &str)] = &[
        ("j / k / arrows", "Move the cursor"),
        ("g / G", "First / last episode"),
        ("ctrl-d / ctrl-u", "Half a page"),
        ("tab", "Focus the other pane"),
        ("enter", "Play the selected episode"),
        ("space", "Play / pause"),
        ("u", "Unload"),
        ("n / p", "Next / previous episode"),
        ("h / l", "Seek 30 seconds"),
        ("H / L", "Seek 5 minutes"),
        ("esc", "Cancel a seek in flight"),
        ("r", "A random episode"),
        ("f", "Favourite the selected episode"),
        ("d", "Download it for offline play"),
        ("x", "Cancel the transfer, or delete the copy"),
        ("/", "Find by title or by track"),
        ("?", "This list"),
        ("q", "Quit"),
    ];

    let width = 64u16.min(area.width.saturating_sub(4));
    // Two rows of trailer plus two of border
    let height = (bindings.len() as u16 + 4).min(area.height.saturating_sub(2));
    let popup = centre(area, width, height);

    frame.render_widget(Clear, popup);
    let block = panes::pane(" // Keys ", true, theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line<'_>> = bindings
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {key:<17}"), Style::default().fg(theme.accent)),
                Span::styled((*what).to_string(), Style::default().fg(theme.dim)),
            ])
        })
        .collect();

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        " Downloading an episode makes seeking within it immediate.",
        Style::default().fg(theme.faint),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(theme.bg))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

/// A rectangle of this size in the middle of `area`, clamped to fit.
fn centre(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_popup_is_centred_inside_its_area() {
        let popup = centre(Rect::new(0, 0, 100, 40), 60, 20);
        assert_eq!(popup, Rect::new(20, 10, 60, 20));
    }

    #[test]
    fn a_popup_larger_than_its_area_is_clamped_rather_than_overflowing() {
        let popup = centre(Rect::new(0, 0, 40, 10), 60, 20);
        assert_eq!(popup, Rect::new(0, 0, 40, 10));
    }
}
