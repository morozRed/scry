//! Things drawn over the body: the confirmation sheet and the help card.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Row, Table, Wrap};
use scry_core::view::{human_age, human_bytes};

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

    let wide = inner.width >= 90;
    let mut widths = vec![
        Constraint::Length(6),
        Constraint::Length(4),
        Constraint::Fill(2),
    ];
    let mut titles = vec!["PID", "AGE", "COMMAND"];
    if wide {
        widths.extend([Constraint::Fill(2), Constraint::Length(8)]);
        titles.extend(["SESSION", "EVIDENCE"]);
    }
    widths.push(Constraint::Length(6));
    titles.push("FREES");
    let rows = sheet.targets.iter().map(|t| {
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
    f.render_widget(
        Table::new(rows, widths)
            .header(Row::new(titles).style(theme.heading))
            .column_spacing(2),
        list,
    );
    area
}

/// Lines a sentence gains when word-wrapped to `width`.
fn wrapped_extra(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    let mut lines = 1;
    let mut used = 0;
    for word in text.split_whitespace() {
        let w = word.chars().count();
        if used > 0 && used + 1 + w > width {
            lines += 1;
            used = w;
        } else {
            used += w + usize::from(used > 0);
        }
    }
    lines - 1
}

const KEYS: [(&str, &str, &str); 12] = [
    ("Move", "j k ↑ ↓", "up and down, ctrl-d ctrl-u by page"),
    ("", "g G", "first and last row"),
    ("", "⏎  h l", "open or fold, go to"),
    ("Views", "1-5  tab", "switch view"),
    ("Find", "/", "filter, fuzzy; esc clears"),
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
