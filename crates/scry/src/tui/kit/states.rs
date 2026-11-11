//! What a view shows instead of a list: empty, loading, unavailable or failed. Each is a
//! glyph and a sentence, so none depends on colour.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use scry_core::view::Tone;

use crate::tui::theme::{Theme, glyph};

pub enum Status<'a> {
    Empty { title: &'a str, hint: &'a str },
    Loading,
    Unavailable(&'a str),
    Failed(&'a str),
}

pub fn status(f: &mut Frame, area: Rect, theme: &Theme, view: &str, s: Status) {
    let (mark, tone, title, hint) = match s {
        Status::Empty { title, hint } => ("✓", Tone::Ok, title.to_string(), hint.to_string()),
        Status::Loading => (
            "◌",
            Tone::Quiet,
            format!("Reading {}…", view.to_lowercase()),
            String::new(),
        ),
        Status::Unavailable(why) => (
            glyph(Tone::Quiet),
            Tone::Quiet,
            why.to_string(),
            String::new(),
        ),
        Status::Failed(why) => (
            glyph(Tone::Danger),
            Tone::Danger,
            format!("Could not read {}.", view.to_lowercase()),
            format!("{why}   r retry"),
        ),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(mark, theme.tone(tone)),
        Span::raw("  "),
        Span::styled(title, theme.strong),
    ])];
    if !hint.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(hint, theme.muted));
    }
    let top = area.height.saturating_sub(lines.len() as u16) / 3;
    let at = Rect {
        y: area.y + top,
        height: area.height - top,
        ..area
    };
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), at);
}
