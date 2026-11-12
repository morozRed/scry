//! Header with the pulse slot, tab bar, footer and rules.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use scry_core::view::{Board, Pulse, Tab, Tone, human_bytes};

use super::{fill, width};
use crate::tui::theme::{Chrome, Theme, glyph};

/// Up to five eighths of the cell, in the muted ink: a trend beside the number, not the
/// heaviest thing in the header.
const SPARK: [char; 5] = ['▁', '▂', '▃', '▄', '▅'];
const SPARK_LEN: usize = 8;
/// The sparkline scales to its own peak, but never makes a quiet 3% look like a spike.
const SPARK_FLOOR: u8 = 20;
const FORECAST_WARN_DAYS: u32 = 30;

pub fn rule(f: &mut Frame, area: Rect, theme: &Theme) {
    let line = "─".repeat(area.width as usize);
    f.render_widget(Paragraph::new(line).style(theme.border), area);
}

/// ` scry` and the pulse strip on the left, the data source on the right. Segments that do
/// not fit are dropped from the right, the source first.
pub fn header(f: &mut Frame, area: Rect, theme: &Theme, board: &Board, source: &str) {
    if theme.chrome == Chrome::Bands {
        fill(f, area, theme.surface);
    }
    let mut spans = vec![
        Span::styled(" scry", theme.accent.add_modifier(Modifier::BOLD)),
        Span::raw("   "),
    ];
    let segments = match &board.pulse {
        Some(p) => pulse(theme, p),
        None => vec![vec![Span::styled(
            board.overview.counts_line(),
            theme.muted,
        )]],
    };
    let source = Span::styled(format!("{source} "), theme.muted);
    let room = area.width as usize;
    for seg in segments {
        if width(&spans) + width(&seg) + 3 > room {
            break;
        }
        spans.extend(seg);
        spans.push(Span::raw("   "));
    }
    let used = width(&spans);
    if used + source.width() <= room {
        spans.push(Span::raw(" ".repeat(room - used - source.width())));
        spans.push(source);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn pulse<'a>(theme: &Theme, p: &Pulse) -> Vec<Vec<Span<'a>>> {
    let label = |s: &'static str| Span::styled(s, theme.muted);
    let warming = || Span::styled("warming up", theme.muted);
    let mut out = Vec::new();
    out.push(match &p.cpu {
        Some(cpu) => {
            let recent = &cpu.recent[cpu.recent.len().saturating_sub(SPARK_LEN)..];
            let top = recent.iter().copied().max().unwrap_or(0).max(SPARK_FLOOR) as usize;
            let spark: String = recent
                .iter()
                .map(|&v| SPARK[(v as usize).min(top) * (SPARK.len() - 1) / top])
                .collect();
            vec![
                label("cpu "),
                Span::styled(spark, theme.muted),
                Span::styled(format!(" {}%", cpu.now), theme.text),
            ]
        }
        None => vec![label("cpu "), warming()],
    });
    out.push(match p.memory {
        Some(m) => vec![
            label("mem "),
            Span::styled(glyph(m.tone()), theme.tone(m.tone())),
            Span::styled(format!(" {}", m.word()), theme.text),
        ],
        None => vec![label("mem "), warming()],
    });
    if let Some(swap) = p.swap_used {
        out.push(vec![
            label("swap "),
            Span::styled(human_bytes(swap), theme.text),
        ]);
    }
    if let Some(disk) = &p.disk {
        let mut seg = vec![
            label("disk "),
            Span::styled(human_bytes(disk.free), theme.text),
        ];
        if let Some(days) = disk.full_in_days {
            let tone = if days < FORECAST_WARN_DAYS {
                Tone::Warn
            } else {
                Tone::Quiet
            };
            seg.push(Span::styled(
                format!(" · full in {days}d"),
                theme.tone(tone),
            ));
        }
        out.push(seg);
    }
    out
}

pub fn tabs(f: &mut Frame, area: Rect, theme: &Theme, active: Tab, badges: [usize; 5]) {
    let mut spans = vec![Span::raw(" ")];
    for (i, tab) in Tab::ALL.iter().enumerate() {
        let on = *tab == active;
        let style = if on { theme.tab_active } else { theme.muted };
        spans.push(Span::styled(format!(" {} {}", i + 1, tab.title()), style));
        if badges[i] > 0 {
            spans.push(Span::styled(
                format!(" ·{}", badges[i]),
                if on {
                    theme.tab_active.patch(theme.warn)
                } else {
                    theme.warn
                },
            ));
        }
        spans.push(Span::styled(" ", style));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The bottom line: a toast or the filter on the left, key hints after it. It sits on the
/// plain background, so it never reads as one more line of the detail band above it.
pub fn footer(f: &mut Frame, area: Rect, theme: &Theme, left: Vec<Span>, hints: &[(&str, &str)]) {
    let mut spans = vec![Span::raw(" ")];
    if !left.is_empty() {
        spans.extend(left);
        spans.push(Span::raw("    "));
    }
    let room = area.width as usize;
    for (key, what) in hints {
        let hint = [
            Span::styled(key.to_string(), theme.key),
            Span::styled(format!(" {what}"), theme.muted),
            Span::raw("   "),
        ];
        if width(&spans) + width(&hint[..2]) > room {
            break;
        }
        spans.extend(hint);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
