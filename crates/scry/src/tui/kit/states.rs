//! What a view shows instead of a list: empty, loading, unavailable or failed. Each is a
//! glyph and a sentence, so none depends on colour. A hint under it says what would fill
//! the view, one paragraph per line; for an unavailable view, every line of its reason
//! after the first is that hint.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use scry_core::view::Tone;

use super::wrapped_lines;
use crate::tui::theme::{Theme, glyph};

/// Wider lines are hard to read centred, so the text wraps within this many columns.
const MEASURE: u16 = 64;

pub enum Status<'a> {
    /// Nothing there, and the view knows it: the only state with a ✓.
    Clear {
        title: &'a str,
        hint: &'a str,
    },
    /// Nothing to show, which says nothing about what is running.
    Empty {
        title: &'a str,
        hint: &'a str,
    },
    Loading,
    /// The first line is the title, the rest the hint.
    Unavailable(&'a str),
    Failed(&'a str),
}

pub fn status(f: &mut Frame, area: Rect, theme: &Theme, view: &str, s: Status) {
    let (mark, tone, title, hint) = match s {
        Status::Clear { title, hint } => ("✓", Tone::Ok, title.to_string(), hint.to_string()),
        Status::Empty { title, hint } => (
            glyph(Tone::Quiet),
            Tone::Quiet,
            title.to_string(),
            hint.to_string(),
        ),
        Status::Loading => (
            "◌",
            Tone::Quiet,
            format!("Reading {}…", view.to_lowercase()),
            String::new(),
        ),
        Status::Unavailable(why) => {
            let (title, hint) = why.split_once('\n').unwrap_or((why, ""));
            (
                glyph(Tone::Quiet),
                Tone::Quiet,
                title.to_string(),
                hint.trim().to_string(),
            )
        }
        Status::Failed(why) => (
            glyph(Tone::Danger),
            Tone::Danger,
            format!("Could not read {}.", view.to_lowercase()),
            format!("{why}   r retry"),
        ),
    };
    let width = area.width.min(MEASURE);
    let mut height = wrapped_lines(&format!("{mark}  {title}"), width);
    let mut lines = vec![Line::from(vec![
        Span::styled(mark, theme.tone(tone)),
        Span::raw("  "),
        Span::styled(title, theme.strong),
    ])];
    if !hint.is_empty() {
        lines.push(Line::raw(""));
        height += 1;
        for part in hint.lines() {
            height += wrapped_lines(part, width);
            lines.push(Line::styled(part.to_string(), theme.muted));
        }
    }
    let top = area.height.saturating_sub(height) / 3;
    let at = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + top,
        width,
        height: area.height - top,
    };
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        at,
    );
}
