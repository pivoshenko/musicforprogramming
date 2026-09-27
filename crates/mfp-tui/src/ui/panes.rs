//! The four things drawn inside the frame: the catalog, the player, the analyser, and the
//! track listing.
//!
//! Every function here is a pure draw from [`App`] and a rectangle, mutating state only to
//! keep a scroll offset in step with a selection: a pane that decided anything would be a
//! second place the interface's behaviour lived.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use mfp_core::protocol::{PlaybackState, Source};

use super::anim;
use super::app::{App, Focus, Local, TrackLine, UNKNOWN_DURATION, hms, name_of, number_of};
use super::theme::{SPECTRUM_STEPS, Theme};

/// The mark on the row the cursor is on. The only thing the mark column ever holds, so
/// the cursor sits hard against the episode number whatever else the row is.
const CURSOR: &str = ">";

const FAVOURITE: char = '*';

/// A complete local copy.
const CACHED: char = 'v';

/// A transfer that failed.
const FAILED: char = '!';

/// The resting analyser's baseline. A hyphen rather than a block: the gaps between the
/// glyphs read as a dashed rule, where a solid bar reads as a level the analyser is
/// holding.
const FLATLINE: char = '-';

/// The body immediately under the analyser's trace, and the body below that. ASCII in grey
/// rather than blocks: the gaps keep the fill reading as texture behind the trace, where a
/// solid column reads as a bar chart.
const FILL_NEAR: char = ':';
const FILL_FAR: char = '.';

/// How many rows under the mark take [`FILL_NEAR`]. Small, so the dense band tracks the
/// trace closely enough to move with it.
const FILL_HALO: usize = 2;

/// A bordered pane, brightened on the border when it holds focus.
///
/// The title does not take the border's colour: a border is chrome and a title is a label,
/// and the two on one accent made the focused pane read as a solid bracket.
pub fn pane<'a>(title: impl Into<Line<'a>>, focused: bool, theme: &Theme) -> Block<'a> {
    let edge = if focused { theme.accent } else { theme.line };
    let label = theme.title;
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(edge))
        .title(title)
        .title_style(Style::default().fg(label).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(theme.bg))
}

// == Catalog ==

/// The episode list, with the number, the name, the markers, and the duration.
pub fn catalog(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let focused = app.focus == Focus::Catalog;

    let title = match app.query.is_empty() {
        true => format!(
            " // Episodes {}/{} ",
            app.selected + 1,
            app.matches.len().max(1)
        ),
        false => format!(
            " // Episodes /{} \u{2192} {} ",
            app.query,
            app.matches.len()
        ),
    };
    let block = pane(title, focused, app.theme());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.matches.is_empty() {
        let theme = app.theme();
        let message = match app.query.is_empty() {
            true => "no episodes yet",
            false => "nothing matches",
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(theme.faint).bg(theme.bg))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }

    let height = inner.height as usize;
    app.catalog_height = height;
    app.catalog_offset = keep_visible(app.catalog_offset, app.selected, height, app.matches.len());

    // Nothing below this writes to `app`, so the theme is borrowed rather than cloned out
    let app: &App = app;
    let theme = app.theme();
    let loaded = app.loaded_id().map(str::to_owned);
    let rows: Vec<Line<'_>> = app
        .matches
        .iter()
        .skip(app.catalog_offset)
        .take(height)
        .enumerate()
        .filter_map(|(row, index)| {
            // `matches` indexes `catalog.episodes` and `refilter` rebuilds it whenever the
            // catalog is replaced, so this always finds one - but a draw is the wrong place
            // to discover otherwise
            let episode = app.catalog.episodes.get(*index)?;
            let id = episode.id();
            let on_cursor = app.catalog_offset + row == app.selected;
            let is_loaded = loaded.as_deref() == Some(id.as_ref());

            Some(catalog_row(
                theme,
                inner.width as usize,
                number_of(episode),
                name_of(episode),
                episode.duration_secs as f64,
                app.is_favourite(&id),
                app.local(&id),
                on_cursor,
                is_loaded,
                focused,
            ))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(rows).style(Style::default().bg(theme.bg)),
        inner,
    );
}

/// One episode row, laid out so the name column is the only one that flexes and titles
/// therefore never shift horizontally as the list scrolls.
#[allow(clippy::too_many_arguments)]
fn catalog_row<'a>(
    theme: &Theme,
    width: usize,
    number: &str,
    name: &'a str,
    duration: f64,
    favourite: bool,
    local: Local,
    on_cursor: bool,
    loaded: bool,
    focused: bool,
) -> Line<'a> {
    // mark(2) number(3) gap(1) name(flex) gap(1) fav(1) local(1) gap(1) duration(8)
    const FIXED: usize = 2 + 3 + 1 + 1 + 1 + 1 + 1 + 8;
    let name_width = width.saturating_sub(FIXED).max(1);

    // The cursor alone, never a second glyph beside it: the loaded episode says so in the
    // colour and weight of its own name, which costs no column and so cannot shift the
    // rows around it
    let (mark, mark_colour) = match on_cursor {
        true if loaded => (CURSOR, theme.playing),
        true => (CURSOR, theme.accent),
        false => (" ", theme.faint),
    };

    let text = if loaded { theme.playing } else { theme.fg };

    let mut style = Style::default().fg(text);
    if on_cursor || loaded {
        style = style.add_modifier(Modifier::BOLD);
    }

    // Carried by the whole line rather than the name alone, so the highlight runs the
    // width of the row: a band that stops before the duration reads as the name being
    // selected rather than the episode
    let row = match on_cursor && focused {
        true => Style::default().bg(theme.line),
        false => Style::default(),
    };

    let (local_mark, local_colour) = match local {
        Local::None => (' ', theme.faint),
        Local::Cached => (CACHED, theme.ok),
        Local::Downloading(_) => (anim::block(4), theme.warn),
        Local::Failed => (FAILED, theme.err),
    };

    let tail = match local {
        Local::Downloading(fraction) => format!("{:>7}%", (fraction * 100.0) as u32),
        _ => format!("{:>8}", hms(duration)),
    };

    Line::from(vec![
        Span::styled(format!("{mark:<2}"), Style::default().fg(mark_colour)),
        Span::styled(
            format!("{number:>3} "),
            Style::default().fg(if on_cursor { theme.accent } else { theme.faint }),
        ),
        Span::styled(
            format!(
                "{:<width$}",
                anim::elide(name, name_width),
                width = name_width
            ),
            style,
        ),
        Span::raw(" "),
        Span::styled(
            String::from(if favourite { FAVOURITE } else { ' ' }),
            Style::default().fg(theme.warn),
        ),
        Span::styled(String::from(local_mark), Style::default().fg(local_colour)),
        Span::raw(" "),
        Span::styled(
            tail,
            Style::default().fg(match local {
                Local::Downloading(_) => theme.warn,
                _ => theme.dim,
            }),
        ),
    ])
    .style(row)
}

/// Slides a window so that `selected` is inside it, moving by the least it can.
///
/// Returned rather than clamped in place so the same rule serves both scrollable panes.
pub fn keep_visible(offset: usize, selected: usize, height: usize, total: usize) -> usize {
    if height == 0 || total == 0 {
        return 0;
    }
    let max_offset = total.saturating_sub(height);
    let offset = offset.min(max_offset);
    if selected < offset {
        selected
    } else if selected >= offset + height {
        selected + 1 - height
    } else {
        offset
    }
}

// == Player ==

/// The loaded episode, what it is doing, where in it playback is, and the spectrum of it.
///
/// One pane rather than two: the analyser is a picture of this episode and nothing else, and a
/// border between them made the interface read as four regions where it has three.
pub fn player(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.theme();
    let block = pane(" // Player ", false, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let Some(episode) = app.snapshot.episode.as_ref() else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::styled(
                    "Nothing loaded, please select an episode",
                    Style::default().fg(theme.faint),
                ),
            ])
            .style(Style::default().bg(theme.bg))
            .alignment(Alignment::Center),
            inner,
        );
        return;
    };

    let width = inner.width as usize;
    let title = mfp_core::model::site_title(&episode.title);
    let mut lines = vec![Line::from(header_spans(app, theme, title, width))];

    lines.push(Line::from(status_spans(app, theme)));

    let rows = lines.len() as u16;
    let [text, spectrum] =
        Layout::vertical([Constraint::Length(rows), Constraint::Min(0)]).areas(inner);

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        text,
    );
    analyser(frame, spectrum, app);
}

/// The episode on the left, marquee'd when it does not fit, then the clock and what playback
/// is doing, hard against the right edge.
///
/// One row rather than three: the title, the position, and the state are what a glance at the
/// player asks for together, and on separate rows each of them read as a line saying one word.
fn header_spans<'a>(app: &App, theme: &'a Theme, title: &str, width: usize) -> Vec<Span<'a>> {
    /// The least space between the title and the clock, so a title filling the row cannot
    /// run into it.
    const GAP: usize = 2;

    let (word, colour) = state_word(app, theme);
    let mut right = time_spans(app, theme);
    right.push(separator(theme));
    right.push(Span::styled(
        word.to_string(),
        Style::default().fg(colour).add_modifier(Modifier::BOLD),
    ));

    let used: usize = right
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>()
        + GAP;
    let room = width.saturating_sub(used);
    let title = anim::marquee(title, room, app.tick);
    let gap = room.saturating_sub(title.chars().count()) + GAP;

    let mut spans = vec![
        Span::styled(
            title,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(gap)),
    ];
    spans.extend(right);
    spans
}

/// The rule between two parts of a line.
fn separator(theme: &Theme) -> Span<'static> {
    Span::styled(" \u{b7} ".to_string(), Style::default().fg(theme.line))
}

fn state_word(app: &App, theme: &Theme) -> (&'static str, Color) {
    match app.snapshot.playback {
        PlaybackState::Playing => ("Playing", theme.playing),
        PlaybackState::Paused => ("Paused", theme.dim),
        PlaybackState::Loading => ("Loading", theme.warn),
        PlaybackState::Seeking => ("Seeking", theme.warn),
        PlaybackState::Stopped => ("Stopped", theme.faint),
        PlaybackState::Error => ("Error", theme.err),
        // a state only a newer daemon knows, named rather than guessed at
        _ => ("Unknown", theme.faint),
    }
}

/// `v Downloaded . 400 MB . * Favourite`, each part omitted when it says nothing.
fn status_spans<'a>(app: &App, theme: &'a Theme) -> Vec<Span<'a>> {
    let mut spans: Vec<Span<'a>> = Vec::new();

    let separate = |spans: &mut Vec<Span<'a>>| {
        if !spans.is_empty() {
            spans.push(separator(theme));
        }
    };

    // Only a local copy is worth a word: streaming is the default every episode starts in,
    // and saying so on every row of the status line said nothing. Marked with the list's
    // own glyph so it means a complete local copy wherever it appears
    if app.snapshot.source == Some(Source::Local) {
        separate(&mut spans);
        spans.push(Span::styled(
            format!("{CACHED} Downloaded"),
            Style::default().fg(theme.ok),
        ));
    }

    if let Some(id) = app.loaded_id()
        && app.is_favourite(id)
    {
        separate(&mut spans);
        spans.push(Span::styled(
            format!("{FAVOURITE} Favourite"),
            Style::default().fg(theme.warn),
        ));
    }

    if let Some(error) = app.snapshot.error.as_ref() {
        separate(&mut spans);
        spans.push(Span::styled(
            error.message.clone(),
            Style::default().fg(theme.err),
        ));
    }

    spans
}

/// `38:19 / ~4:00:00`, the seek target standing in for the position while one is in flight.
///
/// A `~` marks a total the feed only estimates, rather than the word `approx`: it sits on the
/// number it qualifies, which is what a clock sharing its row with the title has room for.
fn time_spans<'a>(app: &App, theme: &'a Theme) -> Vec<Span<'a>> {
    let total = app
        .snapshot
        .duration_secs
        .map_or(UNKNOWN_DURATION.to_owned(), hms);
    let total = match app.snapshot.duration_approximate {
        true => format!("~{total}"),
        false => total,
    };

    let (elapsed, elapsed_colour) = match app.snapshot.seek_target_secs {
        Some(target) => (format!("\u{2192} {}", hms(target)), theme.warn),
        None => (
            app.display_position()
                .map_or(UNKNOWN_DURATION.to_owned(), hms),
            theme.fg,
        ),
    };

    vec![
        Span::styled(
            elapsed,
            Style::default()
                .fg(elapsed_colour)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" / ".to_string(), Style::default().fg(theme.line)),
        Span::styled(total, Style::default().fg(theme.dim)),
    ]
}

/// The spectrum, as one mark per column over a filled body, drawn into `inner` inside the
/// player's pane.
///
/// The mark carries the column's height, as the site draws it, so the eye follows a line of
/// marks as a moving waveform. The rows beneath it are filled with grey ASCII that thins
/// with depth: the trace alone left most of the pane empty, and a body densest just under
/// the mark moves with it instead of sitting there as a wall.
///
/// Draws a flat baseline rather than nothing when there is no spectrum: a blank region
/// reads as a broken component, and a floor reads as silence.
fn analyser(frame: &mut Frame<'_>, inner: Rect, app: &App) {
    let theme = app.theme();

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let height = inner.height as usize;
    let columns = match app.snapshot.spectrum.as_ref() {
        Some(spectrum) => anim::spectrum_columns(spectrum, inner.width as usize),
        None => {
            let flat = Line::styled(
                FLATLINE.to_string().repeat(inner.width as usize),
                Style::default().fg(theme.spectrum[0]),
            );
            let mut rows = vec![Line::raw(""); height.saturating_sub(1)];
            rows.push(flat);
            frame.render_widget(
                Paragraph::new(rows).style(Style::default().bg(theme.bg)),
                inner,
            );
            return;
        }
    };

    let buffer = frame.buffer_mut();
    for (column, magnitude) in columns.iter().enumerate() {
        let steps = anim::MARK_STEPS;
        let reached = (f64::from(*magnitude) * (height * steps) as f64).round() as usize;

        // A column reading nothing still draws its baseline mark, so the trace runs the
        // full width of the pane. The top of the band is quiet in this catalog, and a
        // blank right-hand third reads as a pane that stops early rather than as silence
        let (from_bottom, within) = match reached {
            0 => (0, 0),
            _ => (
                ((reached - 1) / steps).min(height - 1),
                (reached - 1) % steps,
            ),
        };

        let x = inner.x + column as u16;

        // The rows under the mark, so a quiet frame reads as a low band rather than as a
        // pane with a thread across it. Thinning with depth rather than filled flat: the
        // dense rows hug the mark and move with it, where one glyph all the way down held
        // still under a trace that did not
        for row in 0..from_bottom {
            let y = inner.y + (height - 1 - row) as u16;
            if let Some(cell) = buffer.cell_mut((x, y)) {
                let (glyph, colour) = match from_bottom - row {
                    1..=FILL_HALO => (FILL_NEAR, theme.faint),
                    _ => (FILL_FAR, theme.line),
                };
                cell.set_char(glyph).set_fg(colour).set_bg(theme.bg);
            }
        }

        let y = inner.y + (height - 1 - from_bottom) as u16;
        if let Some(cell) = buffer.cell_mut((x, y)) {
            let step = anim::ramp_step(from_bottom, height, SPECTRUM_STEPS);
            cell.set_char(anim::mark(within))
                .set_fg(theme.spectrum[step])
                .set_bg(theme.bg);
        }
    }
}

// == Tracks ==

/// The selected episode's track listing, or its description when it has no listing.
pub fn tracks(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let focused = app.focus == Focus::Tracks;

    if app.selection().is_none() {
        frame.render_widget(pane(" // Tracks ", focused, app.theme()), area);
        return;
    }
    let (heading, body) = app.track_lines();

    let title = format!(" // {heading} ");
    let block = pane(title, focused, app.theme());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let height = inner.height as usize;
    app.tracks_height = height;
    let max_offset = body.len().saturating_sub(height);
    app.tracks_offset = app.tracks_offset.min(max_offset);

    // Nothing below this writes to `app`, so the theme is borrowed rather than cloned out
    let app: &App = app;
    let theme = app.theme();
    let width = inner.width as usize;
    let lines: Vec<Line<'_>> = body
        .iter()
        .enumerate()
        .skip(app.tracks_offset)
        .take(height)
        // The number is the row's index because every numbered line comes before every other
        // kind, which is what `track_lines` builds
        .map(|(index, line)| match line {
            TrackLine::Numbered(text) => Line::from(vec![
                Span::styled(
                    format!("{:>3} ", index + 1),
                    Style::default().fg(theme.faint),
                ),
                Span::styled(
                    anim::elide(text, width.saturating_sub(4)),
                    Style::default().fg(theme.fg),
                ),
            ]),
            TrackLine::Blank => Line::raw(""),
            TrackLine::Link(url) => Line::styled(
                format!("    {}", anim::elide(url, width.saturating_sub(4))),
                Style::default().fg(theme.title),
            ),
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        inner,
    );
}

/// Splits the right-hand column into the player, the analyser, and the tracks.
///
/// The analyser takes what is left after the other two have what they need, up to nine
/// rows: beyond that it crowds out the track listing, which is the pane one actually
/// reads. On a column too short for all three it takes whatever remains, down to nothing.
pub fn split_right(area: Rect) -> [Rect; 2] {
    /// Two rows of text, two of border, and the analyser's own minimum.
    const PLAYER_MIN: u16 = 5;
    const TRACKS_MIN: u16 = 5;
    const ANALYSER_MAX: u16 = 9;

    let spare = area.height.saturating_sub(PLAYER_MIN + TRACKS_MIN);
    let player = PLAYER_MIN + spare.min(ANALYSER_MAX);

    Layout::vertical([Constraint::Length(player), Constraint::Min(0)]).areas(area)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selection_already_on_screen_does_not_scroll_the_list() {
        assert_eq!(keep_visible(10, 12, 20, 79), 10);
    }

    #[test]
    fn a_selection_above_the_window_scrolls_up_to_it() {
        assert_eq!(keep_visible(10, 3, 20, 79), 3);
    }

    #[test]
    fn a_selection_below_the_window_scrolls_down_by_the_least_it_can() {
        assert_eq!(keep_visible(0, 25, 20, 79), 6);
    }

    #[test]
    fn the_window_never_scrolls_past_the_end_of_the_list() {
        assert_eq!(keep_visible(70, 78, 20, 79), 59);
    }

    #[test]
    fn a_list_shorter_than_the_window_never_scrolls() {
        assert_eq!(keep_visible(5, 2, 20, 3), 0);
    }

    #[test]
    fn an_empty_list_or_a_zero_height_pane_is_not_a_division_by_zero() {
        assert_eq!(keep_visible(4, 0, 0, 10), 0);
        assert_eq!(keep_visible(4, 0, 10, 0), 0);
    }

    #[test]
    fn the_player_takes_what_is_spare_for_its_analyser_within_bounds() {
        let tall = split_right(Rect::new(0, 0, 40, 40));
        assert_eq!(
            tall[0].height, 14,
            "five rows of player plus the analyser's cap"
        );
        assert_eq!(tall[1].height, 26);

        let short = split_right(Rect::new(0, 0, 40, 16));
        assert_eq!(short[0].height, 11);
        assert_eq!(short[1].height, 5);
    }

    #[test]
    fn a_column_too_short_for_everything_still_divides_without_overflowing() {
        for height in 0..16u16 {
            let areas = split_right(Rect::new(0, 0, 40, height));
            let total: u16 = areas.iter().map(|area| area.height).sum();
            assert_eq!(total, height, "at height {height}");
        }
    }
}
