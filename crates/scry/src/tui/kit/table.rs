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
/// A text column grows to its longest cell up to this, and no further while the numbers
/// beside it have room: past it, long cells end in `…` rather than push the numbers away.
const FILL_MAX: u16 = 48;

pub struct ListView<'a> {
    pub columns: &'a [Column],
    /// Every row of the view, filtered or not, so widths and alignment hold still while a
    /// filter or a fold changes what is shown.
    pub rows: &'a [views::Row],
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
    let widths = widths(list, &visible, inner);
    let numeric: Vec<bool> = visible.iter().map(|&i| is_numeric(list.rows, i)).collect();

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

/// Column widths: text columns as wide as their longest cell (at most `FILL_MAX`), with the
/// spare width left at the right. When that does not fit, text columns share what the
/// fixed ones leave.
fn widths(list: &ListView, visible: &[usize], inner: u16) -> Vec<u16> {
    // A sortable column keeps room for its arrow, so the title is never cut.
    let title = |c: &Column| {
        if c.sortable {
            c.title.width() as u16 + ARROW
        } else {
            0
        }
    };
    let natural: Vec<u16> = visible
        .iter()
        .map(|&i| {
            let c = &list.columns[i];
            match c.width {
                Width::Fixed(w) => w.max(title(c)),
                Width::Fill(min) => longest(list.rows, i).min(FILL_MAX).max(min).max(title(c)),
            }
        })
        .collect();
    let gaps = GAP * visible.len().saturating_sub(1) as u16;
    if natural.iter().sum::<u16>() + gaps <= inner {
        return natural;
    }
    let constraints = visible.iter().zip(&natural).map(|(&i, &w)| {
        let c = &list.columns[i];
        match c.width {
            Width::Fixed(_) => Constraint::Length(w),
            Width::Fill(min) => Constraint::Min(min.max(title(c))),
        }
    });
    Layout::horizontal(constraints)
        .spacing(GAP)
        .split(Rect::new(0, 0, inner, 1))
        .iter()
        .map(|r| r.width)
        .collect()
}

/// The widest cell of column `i` across `rows`, with the tree guides and fold count the
/// first column adds.
fn longest(rows: &[views::Row], i: usize) -> u16 {
    rows.iter()
        .enumerate()
        .map(|(n, r)| {
            let Some(cell) = r.cells.get(i) else {
                return 0;
            };
            let context = cell.context.as_ref().map_or(0, |c| 2 + c.width());
            let mut w = cell.text.width() + context;
            if i == 0 {
                w += 2 + 2 * r.depth as usize;
                let below = rows[n + 1..]
                    .iter()
                    .take_while(|c| c.depth > r.depth)
                    .count();
                if below > 0 {
                    w += 3 + below.to_string().len();
                }
            }
            w as u16
        })
        .max()
        .unwrap_or(0)
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
