//! The header line with the views and the pulse, the footer, and rules.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use scry_core::view::{Board, Pulse, Tab, Tone, disk_bytes, human_bytes};

use super::width;
use crate::tui::theme::{Theme, glyph};
use unicode_width::UnicodeWidthStr;

/// The cursor eye from `assets/scry.svg`: an eye whose pupil is a block cursor.
const PUPIL: &str = "▮";

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

/// A vertical rule.
pub fn bar(f: &mut Frame, area: Rect, theme: &Theme) {
    let buf = f.buffer_mut();
    for y in area.y..area.bottom() {
        buf.set_string(area.x, y, "│", theme.border);
    }
}

/// A section's title in capitals, its note after it, and on the right what orders it.
pub fn section(f: &mut Frame, area: Rect, theme: &Theme, title: &str, note: &str, right: &str) {
    let mut spans = vec![Span::styled(title.to_string(), theme.title)];
    if !note.is_empty() {
        spans.push(Span::styled(format!("   {note}"), theme.muted));
    }
    let used = width(&spans);
    let room = area.width as usize;
    if !right.is_empty() && used + right.width() + 2 <= room {
        spans.push(Span::raw(" ".repeat(room - used - right.width())));
        spans.push(Span::styled(right.to_string(), theme.muted));
    }
    f.render_widget(Paragraph::new(super::fit(spans, room)), area);
}

/// One line: the mark and `SCRY`, then the views by name with their counts, the active one
/// in the accent; on the right the pulse, when `pulse` is set, and the activity. Segments
/// that do not fit are dropped from the right, the activity last. Returns where each tab
/// landed.
pub fn header(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    board: &Board,
    tabs: (Tab, [usize; 5]),
    pulse: bool,
    activity: &str,
) -> Vec<(Tab, Rect)> {
    let (active, badges) = tabs;
    let mut spans = vec![
        Span::raw(" "),
        Span::styled("(", theme.strong),
        Span::styled(PUPIL, theme.accent),
        Span::styled(") SCRY", theme.strong),
        Span::raw("   "),
    ];
    let mut at = Vec::new();
    for (i, tab) in Tab::ALL.iter().enumerate() {
        let on = *tab == active;
        let x = area.x + width(&spans) as u16;
        let mut label = vec![Span::styled(
            tab.title(),
            if on {
                theme.accent.add_modifier(Modifier::BOLD)
            } else {
                theme.muted
            },
        )];
        if badges[i] > 0 {
            label.push(Span::styled(
                format!(" {}", badges[i]),
                if on { theme.warn } else { theme.faint },
            ));
        }
        at.push((*tab, Rect::new(x, area.y, width(&label) as u16, 1)));
        spans.extend(label);
        spans.push(Span::raw("   "));
    }
    let room = area.width as usize;
    let mut right: Vec<Span> = Vec::new();
    if pulse {
        let segments = match &board.pulse {
            Some(p) => self::pulse(theme, p),
            None => vec![vec![Span::styled(
                board.overview.counts_line(),
                theme.muted,
            )]],
        };
        for seg in segments {
            if width(&spans) + width(&right) + width(&seg) + 3 > room {
                break;
            }
            right.extend(seg);
            right.push(Span::raw("   "));
        }
    }
    let activity = Span::styled(format!("{activity} "), theme.muted);
    if !activity.content.trim().is_empty()
        && width(&spans) + width(&right) + activity.width() <= room
    {
        right.push(activity);
    } else {
        right.pop();
        right.push(Span::raw(" "));
    }
    let used = width(&spans) + width(&right);
    if used <= room {
        spans.push(Span::raw(" ".repeat(room - used)));
        spans.extend(right);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    at
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
            Span::styled(disk_bytes(disk.free), theme.text),
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

/// The bottom line: a toast or the filter on the left, key hints after it.
pub fn footer(f: &mut Frame, area: Rect, theme: &Theme, left: Vec<Span>, hints: &[(&str, &str)]) {
    let mut spans = vec![Span::raw(" ")];
    if !left.is_empty() {
        spans.extend(left);
        spans.push(Span::raw("    "));
    }
    let room = area.width as usize;
    let need = |hints: &[(&str, &str)], gap: usize| {
        hints
            .iter()
            .map(|(key, what)| key.width() + 1 + what.width() + gap)
            .sum::<usize>()
            .saturating_sub(gap)
    };
    // When even two columns apart they do not fit, the hints just before the last four
    // (refresh, commands, help and quit, which every view ends with) give way first: each
    // is in the palette and on the help card too.
    let mut hints: Vec<(&str, &str)> = hints.to_vec();
    while width(&spans) + need(&hints, 2) > room && hints.len() > 5 {
        hints.remove(hints.len() - 5);
    }
    let all = need(&hints, 3) + 3;
    // Hints sit three columns apart, or two when that keeps the last of them on screen.
    let gap = if width(&spans) + all <= room + 3 {
        "   "
    } else {
        "  "
    };
    for (key, what) in hints {
        let hint = [
            Span::styled(key.to_string(), theme.key),
            Span::styled(format!(" {what}"), theme.muted),
            Span::raw(gap),
        ];
        if width(&spans) + width(&hint[..2]) > room {
            break;
        }
        spans.extend(hint);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
