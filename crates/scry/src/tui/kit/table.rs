//! A flattened tree as a table: columns align across depths, the first column carries the
//! tree glyphs, and numbers align right.

use std::cell::Cell as Shared;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, HighlightSpacing, Row, Table, TableState};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::fit;
use crate::tui::theme::{Theme, glyph};
use crate::tui::views::{self, CellKind, Column, Hits, Shown, Sort, Width, is_numeric};

const GAP: u16 = 2;
/// ` ↓` after a sorted column's title.
const ARROW: u16 = 2;

pub struct ListView<'a> {
    pub columns: &'a [Column],
    pub shown: &'a [Shown<'a>],
    pub cursor: usize,
    pub sort: Option<Sort>,
}

/// Draws the list and returns where each visible row landed, by id.
pub fn table(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    list: &ListView,
    offset: &Shared<usize>,
) -> Vec<(String, Rect)> {
    let cursor_w = theme.cursor.0.width() as u16;
    let inner = area.width.saturating_sub(cursor_w);
    let visible = views::visible(list.columns, area.width);
    let constraints: Vec<Constraint> = visible
        .iter()
        .map(|&i| {
            let c = &list.columns[i];
            // A sortable column keeps room for its arrow, so the title is never cut.
            let title = if c.sortable {
                c.title.width() as u16 + ARROW
            } else {
                0
            };
            match c.width {
                Width::Fixed(w) => Constraint::Length(w.max(title)),
                Width::Fill(min) => Constraint::Min(min.max(title)),
            }
        })
        .collect();
    let widths: Vec<u16> = Layout::horizontal(constraints)
        .spacing(GAP)
        .split(Rect::new(0, 0, inner, 1))
        .iter()
        .map(|r| r.width)
        .collect();
    let numeric: Vec<bool> = visible
        .iter()
        .map(|&i| is_numeric(list.shown.iter().map(|s| s.row), i))
        .collect();

    let header = Row::new(visible.iter().enumerate().map(|(n, &i)| {
        let mut title = list.columns[i].title.to_string();
        let mut style = theme.heading;
        if let Some(s) = list.sort.filter(|s| s.column == i) {
            title.push_str(if s.descending { " ↓" } else { " ↑" });
            style = style.patch(theme.accent);
        }
        let line = Line::styled(title, style);
        Cell::from(if numeric[n] {
            line.alignment(Alignment::Right)
        } else {
            line
        })
    }));

    let none = Hits::default();
    let body = list.shown.iter().map(|s| {
        Row::new(visible.iter().enumerate().map(|(n, &i)| {
            let w = widths[n] as usize;
            let hits = s.hits.get(i).unwrap_or(&none);
            let line = if n == 0 {
                fit(first_cell(theme, s, hits), w)
            } else {
                fit(cell_spans(theme, &s.row.cells[i], hits), w)
            };
            Cell::from(if numeric[n] {
                line.alignment(Alignment::Right)
            } else {
                line
            })
        }))
    });

    // Rows that fit under the header. A list that grew shorter, or a terminal that grew
    // taller, must not leave rows scrolled away above empty space.
    let room = area.height.saturating_sub(1) as usize;
    let top = offset.get().min(list.shown.len().saturating_sub(room));
    let (symbol, symbol_style) = theme.cursor;
    let table = Table::new(body, widths.iter().map(|&w| Constraint::Length(w)))
        .header(header)
        .column_spacing(GAP)
        .row_highlight_style(theme.selection)
        .highlight_symbol(Text::from(Span::styled(symbol, symbol_style)))
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = TableState::default()
        .with_offset(top)
        .with_selected(Some(list.cursor));
    f.render_stateful_widget(table, area, &mut state);
    offset.set(state.offset());

    list.shown
        .iter()
        .enumerate()
        .skip(state.offset())
        .take(room)
        .map(|(i, s)| {
            let y = area.y + 1 + (i - state.offset()) as u16;
            (s.row.id.clone(), Rect::new(area.x, y, area.width, 1))
        })
        .collect()
}

fn first_cell<'a>(theme: &Theme, s: &Shown<'a>, hits: &Hits) -> Vec<Span<'a>> {
    let cell = &s.row.cells[0];
    let mut spans = vec![Span::styled(s.prefix.clone(), theme.faint)];
    if let CellKind::State(_) = cell.kind {
        spans.extend(cell_spans(theme, cell, hits));
        return spans;
    }
    let base = match cell.kind {
        CellKind::Muted => theme.muted,
        _ if s.row.depth == 0 && s.prefix.starts_with(['▾', '▸']) => theme.strong,
        _ => theme.text,
    };
    spans.extend(marked(&cell.text, &hits.text, base, theme.matched));
    if let Some(ctx) = &cell.context {
        spans.push(Span::styled("  ", theme.muted));
        spans.extend(marked(ctx, &hits.context, theme.muted, theme.matched));
    }
    if s.folded > 0 {
        spans.push(Span::styled(format!("  +{}", s.folded), theme.muted));
    }
    spans
}

fn cell_spans<'a>(theme: &Theme, cell: &views::Cell, hits: &Hits) -> Vec<Span<'a>> {
    match cell.kind {
        CellKind::State(state) => {
            let tone = theme.tone(state.tone());
            let mut v = vec![
                Span::styled(glyph(state.tone()), tone),
                Span::styled(" ", tone),
            ];
            v.extend(marked(state.word(), &hits.text, tone, theme.matched));
            v
        }
        CellKind::Muted => marked(&cell.text, &hits.text, theme.muted, theme.matched),
        CellKind::Text | CellKind::Num => {
            let mut v = marked(&cell.text, &hits.text, theme.text, theme.matched);
            if let Some(ctx) = &cell.context {
                v.push(Span::styled("  ", theme.muted));
                v.extend(marked(ctx, &hits.context, theme.muted, theme.matched));
            }
            v
        }
    }
}

/// `text` in `base`, with the graphemes at `hits` in `matched`, as few spans as possible.
pub fn marked<'a>(text: &str, hits: &[usize], base: Style, matched: Style) -> Vec<Span<'a>> {
    if hits.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut out: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_hit = false;
    for (i, g) in text.graphemes(true).enumerate() {
        let hit = hits.binary_search(&i).is_ok();
        if hit != run_hit && !run.is_empty() {
            let style = if run_hit { base.patch(matched) } else { base };
            out.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_hit = hit;
        run.push_str(g);
    }
    if !run.is_empty() {
        let style = if run_hit { base.patch(matched) } else { base };
        out.push(Span::styled(run, style));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(spans: &[Span]) -> Vec<(String, bool)> {
        spans
            .iter()
            .map(|s| (s.content.to_string(), s.style == Style::new().bold()))
            .collect()
    }

    #[test]
    fn marks_whole_graphemes() {
        let hit = Style::new().bold();
        let got = marked("cafe\u{301} crème", &[3, 5, 6], Style::new(), hit);
        assert_eq!(
            parts(&got),
            [
                ("caf".into(), false),
                ("e\u{301}".into(), true),
                (" ".into(), false),
                ("cr".into(), true),
                ("ème".into(), false),
            ]
        );
        let got = marked("👩‍💻 dev", &[0], Style::new(), hit);
        assert_eq!(parts(&got)[0], ("👩‍💻".into(), true));
        let got = marked("東京 dev", &[1, 3], Style::new(), hit);
        assert_eq!(
            parts(&got),
            [
                ("東".into(), false),
                ("京".into(), true),
                (" ".into(), false),
                ("d".into(), true),
                ("ev".into(), false),
            ]
        );
    }
}
