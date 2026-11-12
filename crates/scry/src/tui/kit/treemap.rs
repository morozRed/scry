//! Disk as pictures: the volume as a bar, and the catalog's items as a squarified treemap,
//! each tile sized by its disk cost, coloured by kind and hatched where Scry can clean it
//! up. Arrows move between tiles by where they are on screen.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use scry_core::disk::catalog::Kind;
use scry_core::disk::space::Space;
use scry_core::view::disk_bytes;
use unicode_width::UnicodeWidthStr;

use super::fit;
use crate::tui::theme::{Theme, glyph};
use crate::tui::views::Ink;

/// Rows the volume takes: the bar, two rows tall, and the two lines under it.
pub const VOLUME_H: u16 = 4;

pub struct Item<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub size: u64,
    pub kind: Kind,
    pub cleanable: bool,
}

/// The volume as one bar: used, purgeable, free. Then the space and the outlook lines.
pub fn volume(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    space: Option<&Space>,
    lines: &[Vec<(String, Ink)>; 2],
) {
    let buf = f.buffer_mut();
    if let Some(s) = space.filter(|s| s.total > 0) {
        let w = area.width as f64;
        let total = s.total as f64;
        let cells = |b: u64| ((b as f64 / total) * w).round() as u16;
        let purge = cells(s.purgeable).max(u16::from(s.purgeable > 0));
        let used = cells(s.total - s.available).min(area.width - purge);
        let free = area.width - used - purge;
        for row in 0..2.min(area.height) {
            let y = area.y + row;
            buf.set_string(area.x, y, "█".repeat(used as usize), theme.faint);
            buf.set_string(area.x + used, y, "▒".repeat(purge as usize), theme.info);
            buf.set_string(
                area.x + used + purge,
                y,
                "█".repeat(free as usize),
                theme.ok,
            );
        }
    }
    for (i, segs) in lines.iter().enumerate() {
        let y = area.y + 2 + i as u16;
        if y >= area.bottom() {
            break;
        }
        let line = fit(inked(theme, segs), area.width as usize);
        buf.set_line(area.x, y, &line, area.width);
    }
}

pub fn inked<'a>(theme: &Theme, segs: &[(String, Ink)]) -> Vec<Span<'a>> {
    segs.iter()
        .map(|(text, ink)| match ink {
            Ink::Text => Span::styled(text.clone(), theme.text),
            Ink::Strong => Span::styled(text.clone(), theme.strong),
            Ink::Muted => Span::styled(text.clone(), theme.muted),
            Ink::Faint => Span::styled(text.clone(), theme.faint),
            Ink::Tone(t) => Span::styled(format!("{} {text}", glyph(*t)), theme.tone(*t)),
        })
        .collect()
}

/// The kinds on screen as swatches, wrapped between entries. Returns the rows it took.
pub fn legend(buf: &mut Buffer, area: Rect, theme: &Theme, kinds: &[Kind]) -> u16 {
    let mut entries = vec![vec![
        Span::styled("╱╱", theme.muted),
        Span::styled(" cleanable", theme.muted),
    ]];
    for k in kinds {
        entries.push(vec![
            Span::styled("■ ", theme.kind(*k)),
            Span::styled(k.word(), theme.muted),
        ]);
    }
    let (mut x, mut y) = (area.x, area.y);
    for e in entries {
        let w: u16 = e.iter().map(|s| s.content.width() as u16).sum();
        if x > area.x && x + w > area.right() {
            x = area.x;
            y += 1;
        }
        if y >= area.bottom() {
            break;
        }
        buf.set_line(x, y, &Line::from(e), w);
        x += w + 3;
    }
    y + 1 - area.y
}

type FRect = (f64, f64, f64, f64);

fn worst(row: &[f64], short: f64) -> f64 {
    let s: f64 = row.iter().sum();
    let mx = row.iter().copied().fold(f64::MIN, f64::max);
    let mn = row.iter().copied().fold(f64::MAX, f64::min);
    (short * short * mx / (s * s)).max(s * s / (short * short * mn))
}

/// Squarified treemap (Bruls, Huizing, van Wijk) of `vals`, largest first, in `r`.
fn squarify(vals: &[f64], mut r: FRect) -> Vec<FRect> {
    let total: f64 = vals.iter().sum();
    if total <= 0.0 {
        return Vec::new();
    }
    let scale = r.2 * r.3 / total;
    let areas: Vec<f64> = vals.iter().map(|v| v * scale).collect();
    let mut out = Vec::with_capacity(vals.len());
    let mut i = 0;
    while i < areas.len() {
        let short = r.2.min(r.3).max(f64::EPSILON);
        let mut j = i + 1;
        while j < areas.len() && worst(&areas[i..=j], short) <= worst(&areas[i..j], short) {
            j += 1;
        }
        let row = &areas[i..j];
        let sum: f64 = row.iter().sum();
        if r.2 >= r.3 {
            let cw = sum / r.3;
            let mut y = r.1;
            for a in row {
                out.push((r.0, y, cw, a / cw));
                y += a / cw;
            }
            r = (r.0 + cw, r.1, r.2 - cw, r.3);
        } else {
            let rh = sum / r.2;
            let mut x = r.0;
            for a in row {
                out.push((x, r.1, a / rh, rh));
                x += a / rh;
            }
            r = (r.0, r.1 + rh, r.2, r.3 - rh);
        }
        i = j;
    }
    out
}

/// Lays out `items` (any order) in `area` and draws them. Cells are about twice as tall as
/// they are wide, so the layout runs on doubled rows to keep tiles square on screen.
/// Returns each tile's whole cell area, by id, for the mouse and for moving between tiles.
pub fn treemap(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    items: &[Item],
    selected: Option<&str>,
) -> Vec<(String, Rect)> {
    let mut order: Vec<&Item> = items.iter().filter(|i| i.size > 0).collect();
    order.sort_by_key(|i| std::cmp::Reverse(i.size));
    let vals: Vec<f64> = order.iter().map(|i| i.size as f64).collect();
    let rects = squarify(
        &vals,
        (0.0, 0.0, area.width as f64, area.height as f64 * 2.0),
    );
    let buf = f.buffer_mut();
    let mut out = Vec::new();
    for (item, r) in order.iter().zip(rects) {
        let x0 = area.x + r.0.round() as u16;
        let x1 = area.x + (r.0 + r.2).round() as u16;
        let y0 = area.y + (r.1 / 2.0).round() as u16;
        let y1 = area.y + ((r.1 + r.3) / 2.0).round() as u16;
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let cell = Rect::new(x0, y0, x1 - x0, y1 - y0).intersection(area);
        out.push((item.id.to_string(), cell));
        draw_tile(buf, cell, theme, item, selected == Some(item.id));
    }
    out
}

fn draw_tile(buf: &mut Buffer, cell: Rect, theme: &Theme, item: &Item, selected: bool) {
    // A cell of plain background on the right and below keeps neighbours apart.
    let w = cell.width - u16::from(cell.width > 2);
    let h = cell.height - u16::from(cell.height > 2);
    let style = theme.tile(item.kind, selected);
    let body = if item.cleanable { "╱" } else { " " };
    buf.set_string(cell.x, cell.y, " ".repeat(w as usize), style.bar);
    for y in cell.y + 1..cell.y + h {
        buf.set_string(cell.x, y, body.repeat(w as usize), style.fill);
    }
    let room = w.saturating_sub(1) as usize;
    if room < 2 {
        return;
    }
    let size = disk_bytes(item.size);
    let name = fit(vec![Span::styled(item.name.to_string(), style.name)], room);
    let name_w = name.width();
    buf.set_line(cell.x + 1, cell.y, &name, room as u16);
    if name_w + size.width() + 2 <= room {
        let x = cell.x + w - 1 - size.width() as u16;
        buf.set_string(x, cell.y, &size, style.size);
    } else if h >= 2 && size.width() < room {
        buf.set_string(cell.x + 1, cell.y + 1, &size, style.note);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// The tile next to `from` in `dir`, as a browser moves focus: among the tiles that way,
/// the ones lined up with `from` first, then the nearest edge, then the least offset, and
/// last the topmost or leftmost.
pub fn neighbour(tiles: &[(String, Rect)], from: &str, dir: Dir) -> Option<String> {
    let (_, at) = tiles.iter().find(|(id, _)| id == from)?;
    // How far apart two ranges are; zero when they overlap.
    let apart =
        |a0: u16, a1: u16, b0: u16, b1: u16| b0.saturating_sub(a1).max(a0.saturating_sub(b1));
    tiles
        .iter()
        .filter(|(id, _)| id != from)
        .filter_map(|(id, r)| {
            let (gap, across, cross) = match dir {
                Dir::Right if r.x >= at.right() => (
                    r.x - at.right(),
                    apart(at.y, at.bottom(), r.y, r.bottom()),
                    r.y,
                ),
                Dir::Left if r.right() <= at.x => (
                    at.x - r.right(),
                    apart(at.y, at.bottom(), r.y, r.bottom()),
                    r.y,
                ),
                Dir::Down if r.y >= at.bottom() => (
                    r.y - at.bottom(),
                    apart(at.x, at.right(), r.x, r.right()),
                    r.x,
                ),
                Dir::Up if r.bottom() <= at.y => (
                    at.y - r.bottom(),
                    apart(at.x, at.right(), r.x, r.right()),
                    r.x,
                ),
                _ => return None,
            };
            Some(((across > 0, gap, across, cross), id))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, id)| id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarify_fills_the_rectangle_without_overlap() {
        let vals = [64.0, 21.0, 14.0, 6.4, 4.1, 3.7, 3.1, 2.2, 1.9];
        let rects = squarify(&vals, (0.0, 0.0, 80.0, 30.0));
        assert_eq!(rects.len(), vals.len());
        let area: f64 = rects.iter().map(|r| r.2 * r.3).sum();
        assert!((area - 2400.0).abs() < 1e-6);
        let total: f64 = vals.iter().sum();
        for (r, v) in rects.iter().zip(vals) {
            assert!((r.2 * r.3 / 2400.0 - v / total).abs() < 1e-9);
        }
    }

    #[test]
    fn neighbours_follow_the_screen() {
        //  a a b
        //  a a c
        //  d d d
        let t = |id: &str, x, y, w, h| (id.to_string(), Rect::new(x, y, w, h));
        let tiles = [
            t("a", 0, 0, 20, 10),
            t("b", 20, 0, 10, 5),
            t("c", 20, 5, 10, 5),
            t("d", 0, 10, 30, 5),
        ];
        assert_eq!(neighbour(&tiles, "a", Dir::Right).as_deref(), Some("b"));
        assert_eq!(neighbour(&tiles, "b", Dir::Down).as_deref(), Some("c"));
        assert_eq!(neighbour(&tiles, "c", Dir::Left).as_deref(), Some("a"));
        assert_eq!(neighbour(&tiles, "c", Dir::Down).as_deref(), Some("d"));
        assert_eq!(neighbour(&tiles, "d", Dir::Up).as_deref(), Some("a"));
        assert_eq!(neighbour(&tiles, "a", Dir::Left), None);
    }
}
