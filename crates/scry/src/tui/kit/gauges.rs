//! The Overview's gauges: CPU, memory, disk and, when there is room, what agents started.
//! Each is a label, a figure three rows tall, a graph two rows tall and a caption, side by
//! side with thin rules between them.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use scry_core::disk::forecast::Forecast;
use scry_core::view::{Board, Tone, disk_bytes, human_bytes};

use crate::tui::theme::{Theme, glyph};

/// Rows a gauge takes.
pub const HEIGHT: u16 = 7;
/// Columns between gauges; the rule sits in the middle.
const GAP: u16 = 5;
/// From this width a fourth gauge shows what agent sessions started.
const FOURTH_FROM: u16 = 110;
/// The graph scales to its own peak, but never makes a quiet 3% look like a spike.
const SPARK_FLOOR: u8 = 20;
const FORECAST_WARN_DAYS: f64 = 30.0;

/// Digits three rows tall, from half blocks.
const BIG: [[&str; 3]; 10] = [
    ["█▀█", "█ █", "▀▀▀"],
    ["▀█ ", " █ ", "▀▀▀"],
    ["▀▀█", "█▀▀", "▀▀▀"],
    ["▀▀█", " ▀█", "▀▀▀"],
    ["█ █", "▀▀█", "  ▀"],
    ["█▀▀", "▀▀█", "▀▀▀"],
    ["█▀▀", "█▀█", "▀▀▀"],
    ["▀▀█", "  █", "  ▀"],
    ["█▀█", "█▀█", "▀▀▀"],
    ["█▀█", "▀▀█", "▀▀▀"],
];
const EIGHTHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

enum Graph {
    /// Recent percentages, oldest first, as columns of eighth blocks.
    Spark(Vec<u8>, Style),
    /// One bar of parts: (share, glyph, style). The last part takes what is left.
    Bar(Vec<(f64, &'static str, Style)>),
    /// Three steps, the first `at + 1` filled.
    Steps(usize, Style),
    None,
}

struct Gauge<'a> {
    label: &'static str,
    /// Digits and dots; empty for nothing to show yet.
    figure: String,
    figure_style: Style,
    unit: String,
    graph: Graph,
    caption: Vec<Span<'a>>,
}

pub fn gauges(f: &mut Frame, area: Rect, theme: &Theme, board: &Board) {
    let list = gauge_list(theme, board, area.width >= FOURTH_FROM);
    if list.is_empty() {
        return;
    }
    let n = list.len() as u16;
    let w = area.width.saturating_sub(GAP * (n - 1)) / n;
    let buf = f.buffer_mut();
    for (i, g) in list.iter().enumerate() {
        let x = area.x + i as u16 * (w + GAP);
        draw(
            buf,
            Rect::new(x, area.y, w, HEIGHT.min(area.height)),
            theme,
            g,
        );
        if i + 1 < list.len() {
            for y in area.y..area.y + HEIGHT.min(area.height) {
                buf.set_string(x + w + GAP / 2, y, "│", theme.border);
            }
        }
    }
}

fn gauge_list<'a>(theme: &Theme, board: &Board, fourth: bool) -> Vec<Gauge<'a>> {
    let warming = || vec![Span::styled("warming up", theme.muted)];
    let mut out = Vec::new();
    let pulse = board.pulse.as_ref();
    out.push(match pulse.and_then(|p| p.cpu.as_ref()) {
        Some(c) => {
            let peak = c.recent.iter().copied().max().unwrap_or(c.now);
            Gauge {
                label: "CPU",
                figure: c.now.to_string(),
                figure_style: theme.strong,
                unit: "% of all cores".into(),
                graph: Graph::Spark(c.recent.clone(), theme.info),
                caption: vec![
                    Span::styled("peak ", theme.muted),
                    Span::styled(format!("{peak}%"), theme.text),
                ],
            }
        }
        None => blank("CPU", warming()),
    });
    out.push(match pulse.and_then(|p| p.memory) {
        Some(m) => {
            let tone = m.tone();
            let at = match tone {
                Tone::Danger => 2,
                Tone::Warn => 1,
                _ => 0,
            };
            let (figure, unit) = split(&human_bytes(pulse.and_then(|p| p.swap_used).unwrap_or(0)));
            Gauge {
                label: "MEMORY",
                figure,
                figure_style: theme.strong,
                unit: format!("{unit} in swap"),
                graph: Graph::Steps(at, theme.tone(tone)),
                caption: vec![
                    Span::styled("pressure ", theme.muted),
                    Span::styled(format!("{} {}", glyph(tone), m.word()), theme.tone(tone)),
                ],
            }
        }
        None => blank("MEMORY", warming()),
    });
    if let Some(d) = pulse.and_then(|p| p.disk.as_ref()) {
        let space = board.disk.ready().and_then(|l| l.space.as_ref());
        let forecast = board.disk.ready().and_then(|l| l.forecast.as_ref());
        let (figure, unit) = split(&disk_bytes(d.free));
        let mut caption = match forecast {
            Some(Forecast::FullIn { days, .. }) => {
                let tone = if *days < FORECAST_WARN_DAYS {
                    Tone::Warn
                } else {
                    Tone::Quiet
                };
                vec![Span::styled(
                    format!("{} full in ~{:.0} days", glyph(tone), days.max(1.0)),
                    theme.tone(tone),
                )]
            }
            _ => match d.full_in_days {
                Some(days) => vec![Span::styled(
                    format!("{} full in ~{days} days", glyph(Tone::Warn)),
                    theme.warn,
                )],
                None => Vec::new(),
            },
        };
        let graph = match space {
            Some(s) if s.total > 0 => {
                let total = s.total as f64;
                Graph::Bar(vec![
                    ((s.total - s.available) as f64 / total, "█", theme.faint),
                    (s.purgeable as f64 / total, "▒", theme.info),
                    (0.0, "█", theme.ok),
                ])
            }
            _ => Graph::None,
        };
        if caption.is_empty()
            && let Some(s) = space
        {
            caption = vec![Span::styled(
                format!("of {} on {}", disk_bytes(s.total), s.volume),
                theme.muted,
            )];
        }
        let near = d
            .full_in_days
            .is_some_and(|d| (d as f64) < FORECAST_WARN_DAYS);
        out.push(Gauge {
            label: "DISK",
            figure,
            figure_style: if near {
                theme.warn.add_modifier(Modifier::BOLD)
            } else {
                theme.strong
            },
            unit: format!("{unit}B free"),
            graph,
            caption,
        });
    }
    let o = &board.overview;
    if fourth && o.process_count > 0 {
        out.push(Gauge {
            label: "FROM AGENTS",
            figure: o.own_count.to_string(),
            figure_style: theme.accent.add_modifier(Modifier::BOLD),
            unit: format!("of {}", o.process_count),
            graph: Graph::Bar(vec![
                (
                    o.own_count as f64 / o.process_count as f64,
                    "█",
                    theme.accent,
                ),
                (0.0, "░", theme.faint),
            ]),
            caption: vec![
                Span::styled("processes · ", theme.muted),
                Span::styled(human_bytes(o.own_footprint), theme.text),
                Span::styled(" of memory", theme.muted),
            ],
        });
    }
    out
}

fn blank<'a>(label: &'static str, caption: Vec<Span<'a>>) -> Gauge<'a> {
    Gauge {
        label,
        figure: String::new(),
        figure_style: Style::new(),
        unit: String::new(),
        graph: Graph::None,
        caption,
    }
}

/// `1.2G` as `("1.2", "G")`.
fn split(s: &str) -> (String, String) {
    let at = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    (s[..at].to_string(), s[at..].to_string())
}

fn draw(buf: &mut ratatui::buffer::Buffer, r: Rect, theme: &Theme, g: &Gauge) {
    buf.set_string(r.x, r.y, g.label, theme.heading);
    if r.height < HEIGHT {
        return;
    }
    let w = big(buf, r.x, r.y + 1, &g.figure, g.figure_style);
    if !g.unit.is_empty() {
        let x = r.x + w + u16::from(w > 0);
        let room = r.right().saturating_sub(x);
        buf.set_line(x, r.y + 3, &Line::styled(g.unit.clone(), theme.muted), room);
    }
    let gy = r.y + 4;
    match &g.graph {
        Graph::Spark(vals, style) if !vals.is_empty() && r.width > 0 => {
            let top = vals.iter().copied().max().unwrap_or(0).max(SPARK_FLOOR) as f64;
            let n = vals.len();
            for c in 0..r.width {
                let v = vals[(c as usize * n) / r.width as usize] as f64;
                let e = ((v / top) * 16.0).round().clamp(0.0, 16.0) as usize;
                buf.set_string(r.x + c, gy + 1, EIGHTHS[e.min(8)], *style);
                buf.set_string(r.x + c, gy, EIGHTHS[e.saturating_sub(8)], *style);
            }
        }
        Graph::Bar(parts) => {
            let mut x = r.x;
            let mut left = r.width;
            for (i, (share, glyph, style)) in parts.iter().enumerate() {
                let n = if i + 1 == parts.len() {
                    left
                } else {
                    // A part that is there at all gets a cell.
                    let n = (share * r.width as f64).round() as u16;
                    n.max(u16::from(*share > 0.0)).min(left)
                };
                for row in 0..2 {
                    buf.set_string(x, gy + row, glyph.repeat(n as usize), *style);
                }
                x += n;
                left -= n;
            }
        }
        Graph::Steps(at, style) => {
            let seg = r.width.saturating_sub(2) / 3;
            for i in 0..3u16 {
                let x = r.x + i * (seg + 1);
                let (glyph, s) = if (i as usize) <= *at {
                    ("█", *style)
                } else {
                    ("░", theme.faint)
                };
                for row in 0..2 {
                    buf.set_string(x, gy + row, glyph.repeat(seg as usize), s);
                }
            }
        }
        _ => {}
    }
    buf.set_line(r.x, r.y + 6, &Line::from(g.caption.clone()), r.width);
}

/// Draws `text` in the big digits and returns the columns it took.
fn big(buf: &mut ratatui::buffer::Buffer, x: u16, y: u16, text: &str, style: Style) -> u16 {
    let mut at = x;
    for c in text.chars() {
        match c.to_digit(10) {
            Some(d) => {
                for (row, s) in BIG[d as usize].iter().enumerate() {
                    buf.set_string(at, y + row as u16, s, style);
                }
                at += 4;
            }
            None if c == '.' => {
                buf.set_string(at, y + 2, "▀", style);
                at += 2;
            }
            None => {}
        }
    }
    at.saturating_sub(x).saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_size_into_figure_and_unit() {
        assert_eq!(split("1.2G"), ("1.2".into(), "G".into()));
        assert_eq!(split("84G"), ("84".into(), "G".into()));
        assert_eq!(split("512"), ("512".into(), "".into()));
    }
}
