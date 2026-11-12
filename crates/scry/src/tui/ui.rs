//! Lays out one frame from the app state. Pure apart from the table's scroll offset.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use scry_core::view::{Load, Tab};

use super::app::{App, Mode};
use super::kit::states::Status;
use super::kit::table::ListView;
use super::kit::{chrome, detail, overlay, states, table};
use super::theme::{Chrome, Theme, glyph};
use super::views;

pub const MIN_W: u16 = 80;
pub const MIN_H: u16 = 24;
/// The detail pane moves beside the list from this width, where the list still keeps
/// `LIST_BESIDE` columns, so nothing shown in full below 150 is cut beside the pane.
const BESIDE_FROM: u16 = 150;
const LIST_BESIDE: u16 = 96;
const PANE_MAX: u16 = 64;

/// Where things landed, for effects that run after the frame is drawn.
#[derive(Default)]
pub struct Painted {
    pub sheet: Option<Rect>,
    pub rows: Vec<(String, Rect)>,
}

pub fn draw(f: &mut Frame, app: &App) -> Painted {
    let area = f.area();
    let theme = &app.theme;
    if area.width < MIN_W || area.height < MIN_H {
        let msg = format!(
            "Terminal too small. Scry needs {MIN_W}×{MIN_H}, this one is {}×{}.",
            area.width, area.height
        );
        f.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }), area);
        return Painted::default();
    }

    let rules = theme.chrome == Chrome::Rules;
    let [header, tabs, top, body, bottom, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(u16::from(rules)),
        Constraint::Length(1),
    ])
    .areas(area);
    chrome::header(f, header, theme, &app.board, &app.activity(Instant::now()));
    let badges = Tab::ALL.map(|t| (views::view(t).badge)(&app.board));
    chrome::tabs(f, tabs, theme, app.tab, badges);
    if rules {
        chrome::rule(f, top, theme);
        chrome::rule(f, bottom, theme);
    }

    let mut painted = Painted::default();
    let view = views::view(app.tab);
    let title = app.tab.title();
    let mut count = None;
    let rows = app.rows();
    // Overlays cover the whole body; the list and its states sit under the band.
    let whole = body;
    let body = match views::band(view, &app.board) {
        Some(lines) => {
            let [band, _, rest] = Layout::vertical([
                Constraint::Length(lines.len() as u16),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(body);
            chrome::band(f, band, theme, &lines);
            rest
        }
        None => body,
    };
    match &rows {
        Load::Ready(rows) if !rows.is_empty() => {
            let beside = body.width >= BESIDE_FROM;
            let (list, pane) = if beside {
                split_beside(f, body, theme)
            } else {
                (body, Rect::default())
            };
            app.list_width.set(list.width);
            let shown = app.shown(rows);
            count = Some((shown.len(), rows.len()));
            let (list, pane) = if beside {
                (list, pane)
            } else {
                let tallest = rows
                    .iter()
                    .filter_map(|r| r.detail.as_ref())
                    .map(|d| detail::height(theme, d, body.width))
                    .max();
                let cap = if body.height >= 24 { 7 } else { 5 };
                let pane = match (shown.is_empty(), tallest) {
                    (false, Some(h)) => h.clamp(1, cap),
                    _ => 0,
                };
                stack(f, body, theme, shown.len(), pane)
            };
            let cursor = app.cursor(&shown);
            painted.rows = table::table(
                f,
                list,
                theme,
                &ListView {
                    columns: view.columns,
                    rows,
                    shown: &shown,
                    cursor,
                    sort: app.sort(),
                },
                &app.state().offset,
            );
            if shown.is_empty() {
                states::status(
                    f,
                    list,
                    theme,
                    title,
                    Status::Empty {
                        title: "No rows match the filter.",
                        hint: "esc clears it",
                    },
                );
            }
            if let Some(d) = shown.get(cursor).and_then(|s| s.row.detail.as_ref()) {
                detail::detail(f, pane, theme, d, beside);
            }
        }
        other => {
            let empty = views::empty(view, &app.board);
            let status = match other {
                Load::Ready(_) if views::empty_is_clear(view) => Status::Clear {
                    title: &empty.0,
                    hint: &empty.1,
                },
                Load::Ready(_) => Status::Empty {
                    title: &empty.0,
                    hint: &empty.1,
                },
                Load::Loading => Status::Loading,
                Load::Unavailable(why) => Status::Unavailable(why),
                Load::Failed(why) => Status::Failed(why),
            };
            states::status(f, body, theme, title, status);
        }
    }

    let left = footer_left(app, count);
    let hints: Vec<(&str, &str)> = match &app.mode {
        Mode::Normal => normal_hints(app, view, &rows),
        Mode::Filter => vec![("⏎", "keep"), ("esc", "clear"), ("↑/↓", "move")],
        Mode::Sheet(s) => vec![("⏎", overlay::confirm_label(s.kind)), ("esc", "cancel")],
        Mode::Clean(s) => vec![("⏎", overlay::clean_label(s)), ("esc", "cancel")],
        Mode::Help => vec![("esc", "close")],
    };
    chrome::footer(f, footer, theme, left, &hints);

    match &app.mode {
        Mode::Help => {
            overlay::scrim(f, whole, theme);
            overlay::help(f, whole, theme);
        }
        Mode::Sheet(sheet) => {
            overlay::scrim(f, whole, theme);
            painted.sheet = Some(overlay::sheet(f, whole, theme, sheet));
        }
        Mode::Clean(sheet) => {
            overlay::scrim(f, whole, theme);
            painted.sheet = Some(overlay::clean_sheet(f, whole, theme, sheet));
        }
        _ => {}
    }
    painted
}

/// Keys that do something on this view as it is now: no moving, filtering or sorting
/// without a list, and no sorting without a sortable column on screen.
fn normal_hints(
    app: &App,
    view: &views::ViewDef,
    rows: &Load<Vec<views::Row>>,
) -> Vec<(&'static str, &'static str)> {
    let mut hints = Vec::new();
    match rows {
        Load::Ready(rows) if !rows.is_empty() => {
            hints.push(("j/k", "move"));
            hints.extend(view.hints.iter().copied());
            hints.push(("/", "filter"));
            if app.columns().iter().any(|&i| view.columns[i].sortable) {
                hints.push(("s", "sort"));
            }
        }
        Load::Failed(_) => hints.push(("r", "retry")),
        _ => {}
    }
    if !matches!(rows, Load::Failed(_)) {
        hints.push(("r", "refresh"));
    }
    hints.extend([("?", "help"), ("q", "quit")]);
    hints
}

/// From `BESIDE_FROM` columns the detail pane sits beside the list, full height.
fn split_beside(f: &mut Frame, body: Rect, theme: &Theme) -> (Rect, Rect) {
    let rules = theme.chrome == Chrome::Rules;
    // A rule gets a blank column on its list side, so the last column never touches it.
    let gap = if rules { 2 } else { 1 };
    let w = (body.width - gap - LIST_BESIDE).min(PANE_MAX);
    let [list, gap, pane] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(gap),
        Constraint::Length(w),
    ])
    .areas(body);
    if rules {
        let bar = vec![Line::raw("│"); gap.height as usize];
        let at = Rect {
            x: gap.right() - 1,
            width: 1,
            ..gap
        };
        f.render_widget(Paragraph::new(bar).style(theme.border), at);
    }
    (list, pane)
}

/// Below `BESIDE_FROM` columns the pane, `pane` rows tall, sits right under the list, and
/// the rows the list does not need stay empty below it.
fn stack(f: &mut Frame, body: Rect, theme: &Theme, rows: usize, pane: u16) -> (Rect, Rect) {
    let gap = u16::from(theme.chrome == Chrome::Rules && pane > 0);
    let room = body.height.saturating_sub(pane + gap);
    // With nothing to list, the list area holds the empty state.
    let list = if rows == 0 {
        room
    } else {
        (1 + rows).min(room as usize) as u16
    };
    let [list, gap, pane, _] = Layout::vertical([
        Constraint::Length(list),
        Constraint::Length(gap),
        Constraint::Length(pane),
        Constraint::Min(0),
    ])
    .areas(body);
    if gap.height > 0 {
        chrome::rule(f, gap, theme);
    }
    (list, pane)
}

/// The filter while it is typed, else a toast, else the filter that is applied.
fn footer_left<'a>(app: &App, count: Option<(usize, usize)>) -> Vec<Span<'a>> {
    let theme = &app.theme;
    let editing = app.mode == Mode::Filter;
    if let Some(t) = app.toast.as_ref().filter(|_| !editing) {
        return vec![
            Span::styled(glyph(t.tone), theme.tone(t.tone)),
            Span::styled(format!(" {}", t.text), theme.text),
        ];
    }
    let filter = &app.state().filter;
    if filter.is_empty() && !editing {
        return Vec::new();
    }
    let mut spans = vec![
        Span::styled("/", theme.key),
        Span::styled(filter.clone(), theme.text),
    ];
    if editing {
        spans.push(Span::styled("▏", theme.accent));
    }
    if let Some((shown, total)) = count {
        spans.push(Span::styled(format!("  {shown} of {total}"), theme.muted));
    }
    spans
}
