//! The detail pane: the selected row's title, its state and facts as filled chips, then
//! sections set apart by rules: why it is there, its details, the evidence, and for some
//! views a ranking. Under a narrow list it keeps to the title, chips and sentence.

use std::cell::Cell;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use scry_core::view::Detail;
use unicode_width::UnicodeWidthStr;

use super::{pad, wrap, wrapped_lines};
use crate::tui::theme::Theme;
use crate::tui::views::Bars;

/// Text sits this far in from the pane's edges; rules run edge to edge.
const INSET: u16 = 2;
const BAR_W: usize = 12;
const SIZE_W: usize = 7;

pub struct Pane<'a> {
    pub detail: &'a Detail,
    pub chips: Vec<(String, Style)>,
    /// The heading over the sentence.
    pub about: &'a str,
    pub bars: Option<Bars>,
}

/// Rows the pane needs under a list `width` columns wide: title, chips, sentence.
pub fn stacked_height(p: &Pane, width: u16) -> u16 {
    let room = width.saturating_sub(2 * INSET);
    2 + wrapped_lines(&p.detail.sentence, room)
}

/// Under the list: the title, the chips and the sentence, cut at the bottom.
pub fn stacked(f: &mut Frame, area: Rect, theme: &Theme, p: &Pane) {
    let room = area.width.saturating_sub(2 * INSET);
    let mut lines = vec![Line::styled(p.detail.title.clone(), theme.strong), chips(p)];
    lines.extend(
        wrap(&p.detail.sentence, room)
            .into_iter()
            .map(|l| Line::styled(l, theme.text)),
    );
    let buf = f.buffer_mut();
    for (i, line) in lines.iter().take(area.height as usize).enumerate() {
        buf.set_line(area.x + INSET, area.y + i as u16, line, room);
    }
}

enum Item<'a> {
    Rule,
    Text(Line<'a>),
}

/// Beside the list, or over the whole body when expanded: every section, scrolled by
/// `scroll`, which is clamped to what there is.
pub fn beside(f: &mut Frame, area: Rect, theme: &Theme, p: &Pane, scroll: &Cell<usize>) {
    let room = area.width.saturating_sub(2 * INSET);
    let items = items(theme, p, room);
    let max = items.len().saturating_sub(area.height as usize);
    let top = scroll.get().min(max);
    scroll.set(top);
    let buf = f.buffer_mut();
    for (i, item) in items
        .iter()
        .skip(top)
        .take(area.height as usize)
        .enumerate()
    {
        let y = area.y + i as u16;
        match item {
            Item::Rule => {
                let line = "─".repeat(area.width as usize);
                buf.set_string(area.x, y, line, theme.border);
            }
            Item::Text(line) => {
                buf.set_line(area.x + INSET, y, line, room);
            }
        }
    }
    if top > 0 {
        buf.set_string(area.right().saturating_sub(INSET), area.y, "↑", theme.muted);
    }
    if top < max {
        buf.set_string(
            area.right().saturating_sub(INSET),
            area.bottom() - 1,
            "↓",
            theme.muted,
        );
    }
}

fn chips<'a>(p: &Pane) -> Line<'a> {
    let mut spans = Vec::new();
    for (text, style) in &p.chips {
        spans.push(Span::styled(format!(" {text} "), *style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn items<'a>(theme: &Theme, p: &'a Pane, room: u16) -> Vec<Item<'a>> {
    let d = p.detail;
    let mut out = vec![
        Item::Text(Line::raw("")),
        Item::Text(Line::styled(d.title.as_str(), theme.strong)),
    ];
    if !p.chips.is_empty() {
        out.push(Item::Text(chips(p)));
    }
    let section = |out: &mut Vec<Item<'a>>, title: &str, note: &str| {
        out.push(Item::Text(Line::raw("")));
        out.push(Item::Rule);
        out.push(Item::Text(Line::raw("")));
        let gap = (room as usize).saturating_sub(title.width() + note.width());
        out.push(Item::Text(Line::from(vec![
            Span::styled(title.to_string(), theme.title),
            Span::raw(" ".repeat(gap)),
            Span::styled(note.to_string(), theme.title),
        ])));
    };

    section(&mut out, p.about, "");
    for l in wrap(&d.sentence, room) {
        out.push(Item::Text(Line::styled(l, theme.text)));
    }

    if !d.facts.is_empty() {
        section(&mut out, "Details", "");
        let cols = d.facts.iter().map(|f| f.label.width()).max().unwrap_or(0) + 2;
        let value_room = room.saturating_sub(cols as u16);
        for fact in &d.facts {
            for (i, l) in wrap(&fact.value, value_room).into_iter().enumerate() {
                let label = if i == 0 {
                    pad(&fact.label, cols)
                } else {
                    pad("", cols)
                };
                out.push(Item::Text(Line::from(vec![
                    Span::styled(label, theme.muted),
                    Span::styled(l, theme.text),
                ])));
            }
        }
    }

    if !d.evidence.is_empty() {
        section(&mut out, "Evidence", "");
        for e in &d.evidence {
            for (i, l) in wrap(e, room.saturating_sub(2)).into_iter().enumerate() {
                let mark = if i == 0 { "✓ " } else { "  " };
                out.push(Item::Text(Line::from(vec![
                    Span::styled(mark, theme.ok),
                    Span::styled(l, theme.text),
                ])));
            }
        }
    }

    if let Some(bars) = &p.bars {
        section(&mut out, &bars.title, &bars.note);
        let top = bars.rows.iter().map(|r| r.1).max().unwrap_or(1).max(1);
        let name_w = (room as usize).saturating_sub(BAR_W + SIZE_W + 2);
        for (name, bytes, kind) in &bars.rows {
            let filled = ((*bytes as f64 / top as f64) * BAR_W as f64).ceil() as usize;
            let filled = filled.clamp(1, BAR_W);
            let name = super::fit(vec![Span::raw(name.clone())], name_w);
            let name = name
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>();
            out.push(Item::Text(Line::from(vec![
                Span::styled(pad(&name, name_w + 1), theme.text),
                Span::styled("━".repeat(filled), theme.kind(*kind)),
                Span::styled("─".repeat(BAR_W - filled), theme.faint),
                Span::styled(
                    format!("{:>SIZE_W$}", scry_core::view::disk_bytes(*bytes)),
                    theme.text,
                ),
            ])));
        }
    }
    out
}
