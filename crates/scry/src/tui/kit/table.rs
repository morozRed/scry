//! A flattened tree as a table: columns align across depths, the first column carries the
//! tree glyphs, and numbers align right.

use std::cell::Cell as Shared;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, HighlightSpacing, Row, Table, TableState};

use super::fit;
use crate::tui::theme::{Chrome, Theme, glyph};
use crate::tui::views::{CellKind, Column, Shown, Sort, Width, is_numeric};

const GAP: u16 = 2;

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
    let marker = if theme.chrome == Chrome::Panels {
        "▌"
    } else {
        " "
    };
    let inner = area.width.saturating_sub(1);
    let visible: Vec<usize> = (0..list.columns.len())
        .filter(|&i| list.columns[i].min_list_width <= area.width)
        .collect();
    let constraints: Vec<Constraint> = visible
        .iter()
        .map(|&i| match list.columns[i].width {
            Width::Fixed(w) => Constraint::Length(w),
            Width::Fill(min) => Constraint::Min(min),
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

    let body = list.shown.iter().map(|s| {
        Row::new(visible.iter().enumerate().map(|(n, &i)| {
            let w = widths[n] as usize;
            let line = if n == 0 {
                fit(first_cell(theme, s), w)
            } else {
                fit(cell_spans(theme, &s.row.cells[i]), w)
            };
            Cell::from(if numeric[n] {
                line.alignment(Alignment::Right)
            } else {
                line
            })
        }))
    });

    let table = Table::new(body, widths.iter().map(|&w| Constraint::Length(w)))
        .header(header)
        .column_spacing(GAP)
        .row_highlight_style(theme.selection)
        .highlight_symbol(Text::from(Span::styled(marker, theme.accent)))
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = TableState::default()
        .with_offset(offset.get())
        .with_selected(Some(list.cursor));
    f.render_stateful_widget(table, area, &mut state);
    offset.set(state.offset());

    list.shown
        .iter()
        .enumerate()
        .skip(state.offset())
        .take(area.height.saturating_sub(1) as usize)
        .map(|(i, s)| {
            let y = area.y + 1 + (i - state.offset()) as u16;
            (s.row.id.clone(), Rect::new(area.x, y, area.width, 1))
        })
        .collect()
}

fn first_cell<'a>(theme: &Theme, s: &Shown<'a>) -> Vec<Span<'a>> {
    let cell = &s.row.cells[0];
    let mut spans = vec![Span::styled(s.prefix.clone(), theme.faint)];
    if let CellKind::State(_) = cell.kind {
        spans.extend(cell_spans(theme, cell));
        return spans;
    }
    let base = match cell.kind {
        CellKind::Muted => theme.muted,
        _ if s.row.depth == 0 && s.prefix.starts_with(['▾', '▸']) => theme.strong,
        _ => theme.text,
    };
    if s.hits.is_empty() {
        spans.push(Span::styled(cell.text.clone(), base));
    } else {
        for (i, ch) in cell.text.chars().enumerate() {
            let style = if s.hits.contains(&(i as u32)) {
                theme.matched
            } else {
                base
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
    }
    if let Some(ctx) = &cell.context {
        spans.push(Span::styled(format!("  {ctx}"), theme.muted));
    }
    if s.folded > 0 {
        spans.push(Span::styled(format!("  +{}", s.folded), theme.faint));
    }
    spans
}

fn cell_spans<'a>(theme: &Theme, cell: &crate::tui::views::Cell) -> Vec<Span<'a>> {
    match cell.kind {
        CellKind::State(state) if theme.pills => {
            vec![Span::styled(
                format!(" {} ", state.word()),
                theme.pill(state.tone()),
            )]
        }
        CellKind::State(state) => {
            let tone = theme.tone(state.tone());
            vec![
                Span::styled(glyph(state.tone()), tone),
                Span::styled(format!(" {}", state.word()), tone),
            ]
        }
        CellKind::Muted => vec![Span::styled(cell.text.clone(), theme.muted)],
        CellKind::Text | CellKind::Num => {
            let mut v = vec![Span::styled(cell.text.clone(), theme.text)];
            if let Some(ctx) = &cell.context {
                v.push(Span::styled(format!("  {ctx}"), theme.muted));
            }
            v
        }
    }
}
