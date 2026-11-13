//! Top's load bars: the CPU and memory as two bars across the list, each split by the
//! heaviest groups in their series hues, the rest of yours, and macOS and other users; then
//! a legend that names the hues.
//!
//! ```text
//! cpu  ███████████▓▓▓▓▓▓▓████▒▒▒▒▒░░░░░░░░░░░░░░░░░░░░░░░░░░   38%  of 10 cores
//! mem  █████████████████████▓▓▓▓▓▓▓▓▓████▒▒▒▒▒▒▒▒▒▒░░░░░░░░░░░   22G  of 32G
//!      ■ rustc   ■ Google Chrome   ■ node   ■ other   ▒ macOS
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use scry_core::view::usage::{REST_ID, percent};
use scry_core::view::{UsageView, human_bytes};
use unicode_width::UnicodeWidthStr;

use crate::tui::theme::Theme;

/// The two bars and the legend.
pub const HEIGHT: u16 = 3;
/// Groups named in their own hue.
const NAMED: usize = 3;
const LABEL_W: u16 = 5;
/// Room on the right for "100%  of 10 cores".
const FIGURE_W: u16 = 20;

/// One part of a bar: its share of the whole and how it is drawn.
struct Part {
    share: f64,
    glyph: &'static str,
    style: Style,
}

pub fn load(buf: &mut Buffer, area: Rect, theme: &Theme, u: &UsageView) {
    if area.height == 0 || area.width < LABEL_W + FIGURE_W + 10 {
        return;
    }
    let heads: Vec<_> = u.rows.iter().filter(|r| r.depth == 0).collect();
    let named: Vec<_> = heads
        .iter()
        .filter(|r| r.id != REST_ID)
        .take(NAMED)
        .copied()
        .collect();
    let rest = heads.iter().find(|r| r.id == REST_ID);
    let bar_w = area.width - LABEL_W - FIGURE_W;

    // CPU: shares of all cores, so the parts and the idle add up to 100.
    let cpu_parts = u.cpu.map(|total| {
        let mut parts: Vec<Part> = named
            .iter()
            .enumerate()
            .map(|(i, r)| Part {
                share: f64::from(r.cpu.unwrap_or(0.0)) / 100.0,
                glyph: "█",
                style: theme.series(i),
            })
            .collect();
        let named_sum: f32 = named.iter().filter_map(|r| r.cpu).sum();
        let rest_cpu = rest.and_then(|r| r.cpu).unwrap_or(0.0);
        let others = (total - named_sum - rest_cpu).max(0.0);
        parts.push(Part {
            share: f64::from(others) / 100.0,
            glyph: "█",
            style: theme.muted,
        });
        parts.push(Part {
            share: f64::from(rest_cpu) / 100.0,
            glyph: "▒",
            style: theme.muted,
        });
        parts
    });
    let cpu_figure = match u.cpu {
        Some(c) => format!("{:>4}  of {} cores", percent(c), u.cores),
        None => "  measuring…".into(),
    };
    bar(buf, area, 0, theme, "cpu", cpu_parts, bar_w, &cpu_figure);

    let ram_parts = u.ram_total.filter(|&t| t > 0).map(|total| {
        let total = total as f64;
        let mut parts: Vec<Part> = named
            .iter()
            .enumerate()
            .map(|(i, r)| Part {
                share: r.ram.unwrap_or(0) as f64 / total,
                glyph: "█",
                style: theme.series(i),
            })
            .collect();
        let named_sum: u64 = named.iter().filter_map(|r| r.ram).sum();
        let rest_ram = rest.and_then(|r| r.ram).unwrap_or(0);
        let used = u.ram_used.unwrap_or(0);
        parts.push(Part {
            share: used.saturating_sub(named_sum + rest_ram) as f64 / total,
            glyph: "█",
            style: theme.muted,
        });
        parts.push(Part {
            share: rest_ram as f64 / total,
            glyph: "▒",
            style: theme.muted,
        });
        parts
    });
    let ram_figure = match (u.ram_used, u.ram_total) {
        (Some(used), Some(total)) => {
            format!("{:>4}  of {}", human_bytes(used), human_bytes(total))
        }
        _ => "  not read".into(),
    };
    if area.height > 1 {
        bar(buf, area, 1, theme, "mem", ram_parts, bar_w, &ram_figure);
    }

    if area.height > 2 {
        let mut entries: Vec<Vec<Span>> = named
            .iter()
            .enumerate()
            .map(|(i, r)| {
                vec![
                    Span::styled("■ ", theme.series(i)),
                    Span::styled(short(&r.label), theme.muted),
                ]
            })
            .collect();
        entries.push(vec![
            Span::styled("■ ", theme.muted),
            Span::styled("other", theme.muted),
        ]);
        entries.push(vec![
            Span::styled("▒ ", theme.muted),
            Span::styled("macOS", theme.muted),
        ]);
        let y = area.y + 2;
        let mut x = area.x + LABEL_W;
        for e in entries {
            let w: u16 = e.iter().map(|s| s.content.width() as u16).sum();
            if x + w > area.right() {
                break;
            }
            buf.set_line(x, y, &Line::from(e), w);
            x += w + 3;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn bar(
    buf: &mut Buffer,
    area: Rect,
    row: u16,
    theme: &Theme,
    label: &str,
    parts: Option<Vec<Part>>,
    width: u16,
    figure: &str,
) {
    let y = area.y + row;
    buf.set_string(area.x, y, label, theme.heading);
    let x0 = area.x + LABEL_W;
    // The idle or free part is a faint track under the parts.
    buf.set_string(x0, y, "░".repeat(width as usize), theme.border);
    if let Some(parts) = parts {
        let mut x = 0u16;
        let mut acc = 0.0;
        for p in parts {
            // Ends are rounded from the running total, so parts never drift or overflow.
            acc += p.share.max(0.0);
            let end = ((acc * f64::from(width)).round() as u16).min(width);
            if end > x {
                buf.set_string(x0 + x, y, p.glyph.repeat((end - x) as usize), p.style);
                x = end;
            }
        }
    }
    buf.set_string(x0 + width + 1, y, figure, theme.text);
}

/// A legend name, cut so one long name leaves room for the rest.
fn short(name: &str) -> String {
    const MAX: usize = 16;
    if name.chars().count() <= MAX {
        return name.to_string();
    }
    let mut s: String = name.chars().take(MAX - 1).collect();
    s.push('…');
    s
}
