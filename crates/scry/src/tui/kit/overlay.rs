//! Things drawn over the body: the confirmation sheet and the help card.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Row, Table, Wrap};
use scry_core::view::{human_age, human_bytes};

use super::wrapped_lines;
use crate::tui::app::{Sheet, SheetKind};
use crate::tui::theme::{Chrome, Theme};

/// A sheet that would leave fewer rows than this of the list above it covers the body, so
/// no lone row peeks out above its edge.
const PEEK_MIN: u16 = 4;
/// The plain background kept around the help card, so nothing under it touches its edge.
const MARGIN: (u16, u16) = (2, 1);

/// Raised above the screen on its own surface. In the tinted look the fill is the edge, so
/// it ends on a cell boundary; a line through the edge cells would leave half a cell of
/// fill outside it. The ANSI look has no fill and draws `lines` instead.
fn card(theme: &Theme, lines: Borders) -> Block<'static> {
    let block = Block::new().style(theme.raised);
    match theme.chrome {
        Chrome::Rules => block.borders(lines).border_style(theme.border),
        Chrome::Bands => block,
    }
}

/// Fades what an overlay covers to the faint ink, so the overlay is the one thing to read
/// and the edges of the text under it do not compete with it.
pub fn scrim(f: &mut Frame, area: Rect, theme: &Theme) {
    let style = theme
        .faint
        .remove_modifier(Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED);
    f.buffer_mut().set_style(area, style);
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

    let padding = Padding::new(2, 2, 1, 1);
    let probe = card(theme, Borders::TOP).padding(padding).inner(body);
    let head = text.len() as u16 + wrapped_lines(what, probe.width);
    let mut height = ((body.height - probe.height) + head + 1 + n as u16).min(body.height);
    if body.height - height < PEEK_MIN {
        height = body.height;
    }
    // A sheet as tall as the body sits right under the ANSI look's rule, which is its edge.
    let lines = if height == body.height {
        Borders::NONE
    } else {
        Borders::TOP
    };
    let block = card(theme, lines).padding(padding);
    let area = Rect {
        y: body.bottom() - height,
        height,
        ..body
    };
    let inner = block.inner(area);
    // A row of plain background keeps the list off the sheet's edge.
    let above = area.y.saturating_sub(1).max(body.y);
    let gap = Rect {
        y: above,
        height: area.y - above,
        ..area
    };
    f.render_widget(Clear, gap);
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
        let (name, args) = t.command.split_once(' ').unwrap_or((&t.command, ""));
        let mut command = vec![Span::styled(name.to_string(), theme.strong)];
        if !args.is_empty() {
            command.push(Span::styled(format!(" {args}"), theme.text));
        }
        let mut cells = vec![
            Line::styled(t.pid.to_string(), theme.text),
            Line::styled(human_age(t.age_s), theme.text),
            Line::from(command),
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
    ("", "r", "refresh now"),
    ("App", "?", "this help"),
    ("", "ctrl-z", "suspend to the shell"),
    ("", "q", "quit"),
];

/// The keys, centred over the body on a raised card with a margin of plain background.
pub fn help(f: &mut Frame, body: Rect, theme: &Theme) {
    let mut lines = vec![Line::styled("Keys", theme.strong), Line::raw("")];
    lines.extend(KEYS.iter().map(|(group, keys, what)| {
        Line::from(vec![
            Span::styled(format!("{group:<7}"), theme.heading),
            Span::styled(format!("{keys:<10}"), theme.key),
            Span::styled(*what, theme.text),
        ])
    }));
    // The same outer size in both looks: in the ANSI look a line takes the outer cell on
    // each side.
    let block = match theme.chrome {
        Chrome::Bands => card(theme, Borders::NONE).padding(Padding::new(3, 3, 1, 1)),
        Chrome::Rules => card(theme, Borders::ALL).padding(Padding::horizontal(2)),
    };
    let widest = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let w = (widest + 6).min(body.width);
    let h = (lines.len() as u16 + 2).min(body.height);
    let area = Rect {
        x: body.x + (body.width - w) / 2,
        y: body.y + (body.height - h) / 2,
        width: w,
        height: h,
    };
    let (mx, my) = MARGIN;
    let margin = Rect {
        x: area.x.saturating_sub(mx).max(body.x),
        y: area.y.saturating_sub(my).max(body.y),
        width: w + 2 * mx,
        height: h + 2 * my,
    }
    .intersection(body);
    f.render_widget(Clear, margin);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}
