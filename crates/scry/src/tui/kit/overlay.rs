//! Things drawn over the body: the confirmation sheet and the help card.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Row, Table, Wrap};
use scry_core::view::{human_age, human_bytes};
use unicode_width::UnicodeWidthStr;

use crate::tui::app::{Sheet, SheetKind};
use crate::tui::theme::{Chrome, Theme};

fn card(theme: &Theme, borders: Borders) -> Block<'static> {
    let block = Block::new()
        .borders(borders)
        .border_style(theme.border)
        .style(theme.surface);
    match theme.chrome {
        Chrome::Rules => block,
        Chrome::Bands => block.border_type(BorderType::Rounded),
    }
}

/// The key that confirms a sheet, worded for what it sends.
pub fn confirm_label(kind: SheetKind) -> &'static str {
    match kind {
        SheetKind::Stop => "send SIGTERM",
        SheetKind::Reap => "reap all",
    }
}

/// Rises from the bottom of the body and lists every target before anything is sent.
/// Returns where it was drawn, for the fade.
pub fn sheet(f: &mut Frame, body: Rect, theme: &Theme, sheet: &Sheet) -> Rect {
    let n = sheet.targets.len();
    let (title, what) = match sheet.kind {
        SheetKind::Stop => (
            format!("Stop {}?", plural(n, "process", "processes")),
            "Sends SIGTERM. If it is still running after 3 s, Scry asks again before SIGKILL.",
        ),
        SheetKind::Reap => (
            format!("Reap {}?", plural(n, "leftover", "leftovers")),
            "Children first, each re-checked by pid and start time just before its signal. Processes tied only by a guess are never included.",
        ),
    };
    let frees: u64 = sheet.targets.iter().filter_map(|t| t.frees).sum();
    let mut text = vec![
        Line::styled(title, theme.strong),
        Line::styled(what, theme.muted),
    ];
    if frees > 0 {
        text.push(Line::from(vec![
            Span::styled("Frees about ", theme.muted),
            Span::styled(human_bytes(frees), theme.text),
            Span::styled(".", theme.muted),
        ]));
    }

    let block = card(theme, Borders::TOP).padding(Padding::new(2, 2, 1, 1));
    let probe = block.inner(body);
    let head = text.len() as u16 + wrapped_extra(what, probe.width) + 1;
    let height = ((body.height - probe.height) + head + 1 + n as u16).min(body.height);
    let area = Rect {
        y: body.bottom() - height,
        height,
        ..body
    };
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    let [head, list] =
        Layout::vertical([Constraint::Length(head), Constraint::Min(1)]).areas(inner);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), head);

    // Targets that fit under the column titles. When some do not, the last line says how
    // many and how to scroll to them, and the head above keeps the totals in view.
    let room = list.height.saturating_sub(1) as usize;
    let rows_shown = if n <= room { n } else { room.saturating_sub(1) };
    sheet.max_offset.set(n - rows_shown);
    let top = sheet.offset.get().min(n - rows_shown);
    if rows_shown < n {
        let below = n - top - rows_shown;
        let mut more = Vec::new();
        if top > 0 {
            more.push(Span::styled(format!("↑ {top} above   "), theme.muted));
        }
        if below > 0 {
            more.push(Span::styled(format!("↓ {below} more   "), theme.text));
        }
        more.push(Span::styled("j k", theme.key));
        more.push(Span::styled(" scroll", theme.muted));
        let at = Rect {
            y: list.bottom() - 1,
            height: 1,
            ..list
        };
        f.render_widget(Paragraph::new(Line::from(more)), at);
    }

    let wide = inner.width >= 90;
    let mut widths = vec![
        Constraint::Length(6),
        Constraint::Length(4),
        Constraint::Fill(2),
    ];
    let mut titles = vec![Line::from("PID"), Line::from("AGE"), Line::from("COMMAND")];
    if wide {
        widths.extend([Constraint::Fill(2), Constraint::Length(8)]);
        titles.extend([Line::from("SESSION"), Line::from("EVIDENCE")]);
    }
    widths.push(Constraint::Length(6));
    titles.push(Line::from("FREES").right_aligned());
    let rows = sheet.targets[top..top + rows_shown].iter().map(|t| {
        let mut cells = vec![
            Line::styled(t.pid.to_string(), theme.text),
            Line::styled(human_age(t.age_s), theme.text),
            Line::styled(t.command.clone(), theme.strong),
        ];
        if wide {
            cells.push(Line::styled(
                t.session.clone().unwrap_or_default(),
                theme.muted,
            ));
            cells.push(Line::styled(
                t.evidence.map_or("", |e| e.word()),
                theme.muted,
            ));
        }
        cells.push(
            Line::styled(t.frees.map(human_bytes).unwrap_or_default(), theme.text).right_aligned(),
        );
        Row::new(cells)
    });
    let table = Rect {
        height: list.height.min(1 + rows_shown as u16),
        ..list
    };
    f.render_widget(
        Table::new(rows, widths)
            .header(Row::new(titles).style(theme.heading))
            .column_spacing(2),
        table,
    );
    area
}

/// Lines a sentence gains when word-wrapped to `width`.
fn wrapped_extra(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    let mut lines = 1;
    let mut used = 0;
    for word in text.split_whitespace() {
        let w = word.width();
        if used > 0 && used + 1 + w > width {
            lines += 1;
            used = w;
        } else {
            used += w + usize::from(used > 0);
        }
    }
    lines - 1
}

const KEYS: [(&str, &str, &str); 14] = [
    ("Move", "j k ↑ ↓", "up and down, ctrl-d ctrl-u by page"),
    ("", "g G", "first and last row"),
    ("", "⏎", "fold a session, or go to the row"),
    ("", "h l", "fold and unfold; h goes to the parent"),
    ("Views", "1-5  tab", "switch view"),
    ("Find", "/", "filter, fuzzy, within a column"),
    ("", "esc", "clear the filter"),
    ("", "s  S", "sort by the next column, reverse"),
    ("Act", "x", "stop the selected process"),
    ("", "X", "reap leftovers"),
    ("", "r", "rescan"),
    ("App", "?", "this help"),
    ("", "ctrl-z", "suspend to the shell"),
    ("", "q", "quit"),
];

pub fn help(f: &mut Frame, body: Rect, theme: &Theme) {
    let w = 58.min(body.width);
    let h = (KEYS.len() as u16 + 4).min(body.height);
    let area = Rect {
        x: body.x + (body.width - w) / 2,
        y: body.y + (body.height - h) / 2,
        width: w,
        height: h,
    };
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(group, keys, what)| {
            Line::from(vec![
                Span::styled(format!("{group:<7}"), theme.heading),
                Span::styled(format!("{keys:<10}"), theme.key),
                Span::styled(*what, theme.text),
            ])
        })
        .collect();
    let block = card(theme, Borders::ALL)
        .title(Span::styled(" Keys ", theme.strong))
        .padding(Padding::uniform(1));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}
