//! A flattened tree as a list without a header row: the state as a glyph in a gutter, the
//! first column indented by depth, the other columns aligned across rows and numbers to the
//! right. The section title above the list names it, so the columns need no titles.

use std::cell::Cell as Shared;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use super::fit;
use crate::tui::theme::{Theme, glyph};
use crate::tui::views::{self, CellKind, Column, Hits, Shown, Width, is_numeric};
use scry_core::view::Tone;

const GAP: u16 = 2;
/// The state glyph and the space after it.
pub const GUTTER: u16 = 2;
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
}

/// Draws the list and returns where each visible row landed, by id. `offset` is the first
/// row on screen; it moves to keep the cursor in view.
pub fn table(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    list: &ListView,
    offset: &Shared<usize>,
) -> Vec<(String, Rect)> {
    let inner = area.width.saturating_sub(GUTTER);
    let visible = views::visible(list.columns, area.width);
    let widths = widths(list, &visible, inner);
    let numeric: Vec<bool> = visible.iter().map(|&i| is_numeric(list.rows, i)).collect();

    let room = area.height as usize;
    let len = list.shown.len();
    // A list that grew shorter, or a terminal that grew taller, must not leave rows
    // scrolled away above empty space.
    let mut top = offset.get().min(len.saturating_sub(room));
    if list.cursor < top {
        top = list.cursor;
    } else if room > 0 && list.cursor >= top + room {
        top = list.cursor + 1 - room;
    }
    offset.set(top);

    let none = Hits::default();
    let buf = f.buffer_mut();
    let mut painted = Vec::new();
    for (n, s) in list.shown.iter().enumerate().skip(top).take(room) {
        let y = area.y + (n - top) as u16;
        let row_area = Rect::new(area.x, y, area.width, 1);
        let selected = n == list.cursor;
        let base = if selected {
            buf.set_style(row_area, theme.selection);
            theme.selection
        } else {
            Style::new()
        };
        let (mark, mark_style) = match s.row.state() {
            Some(state) => (glyph(state.tone()), theme.tone(state.tone())),
            None if s.row.depth > 0 => ("·", theme.faint),
            None => (" ", theme.faint),
        };
        buf.set_span(area.x, y, &Span::styled(mark, mark_style.patch(base)), 1);

        let mut x = area.x + GUTTER;
        for (k, &i) in visible.iter().enumerate() {
            let w = widths[k];
            let hits = s.hits.get(i).unwrap_or(&none);
            let spans = if k == 0 {
                first_cell(theme, s, i, hits)
            } else {
                cell_spans(theme, &s.row.cells[i], hits)
            };
            let line = patch(fit(spans, w as usize), base);
            let lw = line.width() as u16;
            let at = if numeric[k] {
                x + w.saturating_sub(lw)
            } else {
                x
            };
            buf.set_line(at, y, &line, w);
            x += w + GAP;
        }
        painted.push((s.row.id.clone(), row_area));
    }
    painted
}

fn patch(line: Line<'_>, base: Style) -> Line<'_> {
    if base == Style::new() {
        return line;
    }
    Line::from(
        line.spans
            .into_iter()
            .map(|s| {
                let style = s.style.patch(Style {
                    bg: base.bg,
                    add_modifier: base.add_modifier,
                    ..Style::new()
                });
                Span::styled(s.content, style)
            })
            .collect::<Vec<_>>(),
    )
}

/// Column widths: text columns as wide as their longest cell (at most `FILL_MAX`), with the
/// spare width left at the right. When that does not fit, text columns share what the
/// fixed ones leave.
fn widths(list: &ListView, visible: &[usize], inner: u16) -> Vec<u16> {
    let natural: Vec<u16> = visible
        .iter()
        .map(|&i| match list.columns[i].width {
            Width::Fixed(w) => w,
            Width::Fill(min) => longest(list.rows, i, visible[0]).min(FILL_MAX).max(min),
        })
        .collect();
    let gaps = GAP * visible.len().saturating_sub(1) as u16;
    if natural.iter().sum::<u16>() + gaps <= inner {
        return natural;
    }
    let constraints = visible
        .iter()
        .zip(&natural)
        .map(|(&i, &w)| match list.columns[i].width {
            Width::Fixed(_) => Constraint::Length(w),
            Width::Fill(min) => Constraint::Min(min),
        });
    Layout::horizontal(constraints)
        .spacing(GAP)
        .split(Rect::new(0, 0, inner, 1))
        .iter()
        .map(|r| r.width)
        .collect()
}

/// The widest cell of column `i` across `rows`, with the indent and fold count the first
/// column adds.
fn longest(rows: &[views::Row], i: usize, first: usize) -> u16 {
    use unicode_width::UnicodeWidthStr;
    rows.iter()
        .enumerate()
        .map(|(n, r)| {
            let Some(cell) = r.cells.get(i) else {
                return 0;
            };
            let context = cell.context.as_ref().map_or(0, |c| 2 + c.width());
            let mut w = cell.text.width() + context;
            if i == first {
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

/// The fold marker or indent, then the text. A parent row reads as a group's header: its
/// name in the accent, bold.
fn first_cell<'a>(theme: &Theme, s: &Shown<'a>, i: usize, hits: &Hits) -> Vec<Span<'a>> {
    let cell = &s.row.cells[i];
    let mut spans = vec![Span::styled(s.prefix.clone(), theme.faint)];
    if let CellKind::State(_) = cell.kind {
        spans.extend(cell_spans(theme, cell, hits));
        return spans;
    }
    let base = match cell.kind {
        CellKind::Muted => theme.muted,
        _ if s.row.depth == 0 && s.prefix.starts_with(['▾', '▸']) => {
            theme.accent.add_modifier(Modifier::BOLD)
        }
        _ => theme.text,
    };
    spans.extend(marked(&cell.text, &hits.text, base, theme.matched));
    // Before the context, so a narrow column cuts the context, not the count.
    if s.folded > 0 {
        spans.push(Span::styled(format!(" +{}", s.folded), theme.muted));
    }
    if let Some(ctx) = &cell.context {
        spans.push(Span::styled("  ", theme.muted));
        spans.extend(marked(ctx, &hits.context, theme.muted, theme.matched));
    }
    spans
}

/// A state column shows the word only, in its tone: the gutter has the glyph.
fn cell_spans<'a>(theme: &Theme, cell: &views::Cell, hits: &Hits) -> Vec<Span<'a>> {
    match cell.kind {
        CellKind::State(state) => {
            let tone = match state.tone() {
                Tone::Quiet => theme.muted,
                t => theme.tone(t),
            };
            marked(state.word(), &hits.text, tone, theme.matched)
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
