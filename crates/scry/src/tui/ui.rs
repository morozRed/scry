//! Lays out one frame from the app state. Pure apart from the table's scroll offset.

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
const BESIDE_FROM: u16 = 120;

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
    chrome::header(f, header, theme, &app.board, &app.source.label());
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
    match app.rows() {
        Load::Ready(rows) if !rows.is_empty() => {
            let shown = app.shown(&rows);
            count = Some((shown.len(), rows.len()));
            let cursor = app.cursor(&shown);
            let (list, pane, beside) = split(f, body, theme);
            let state = app.state();
            painted.rows = table::table(
                f,
                list,
                theme,
                &ListView {
                    columns: view.columns,
                    shown: &shown,
                    cursor,
                    sort: state.sort,
                },
                &state.offset,
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
            let status = match &other {
                Load::Ready(_) => Status::Empty {
                    title: view.empty.0,
                    hint: view.empty.1,
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
        Mode::Normal => [("j/k", "move")]
            .into_iter()
            .chain(view.hints.iter().copied())
            .chain([("/", "filter"), ("s", "sort"), ("?", "help"), ("q", "quit")])
            .collect(),
        Mode::Filter => vec![("⏎", "keep"), ("esc", "clear"), ("↑/↓", "move")],
        Mode::Sheet(s) => vec![("⏎", overlay::confirm_label(s.kind)), ("esc", "cancel")],
        Mode::Help => vec![("esc", "close")],
    };
    chrome::footer(f, footer, theme, left, &hints);

    match &app.mode {
        Mode::Help => overlay::help(f, body, theme),
        Mode::Sheet(sheet) => painted.sheet = Some(overlay::sheet(f, body, theme, sheet)),
        _ => {}
    }
    painted
}

/// Splits the body into list and detail: below the list under 120 columns, beside it
/// from 120.
fn split(f: &mut Frame, body: Rect, theme: &Theme) -> (Rect, Rect, bool) {
    let rules = theme.chrome == Chrome::Rules;
    if body.width >= BESIDE_FROM {
        let w = (body.width * 36 / 100).clamp(40, 56);
        let [list, gap, pane] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(w),
        ])
        .areas(body);
        if rules {
            let bar = vec![Line::raw("│"); gap.height as usize];
            f.render_widget(Paragraph::new(bar).style(theme.border), gap);
        }
        (list, pane, true)
    } else {
        let h = if body.height >= 24 { 7 } else { 5 };
        let [list, gap, pane] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(u16::from(rules)),
            Constraint::Length(h),
        ])
        .areas(body);
        if rules {
            chrome::rule(f, gap, theme);
        }
        (list, pane, false)
    }
}

fn footer_left<'a>(app: &App, count: Option<(usize, usize)>) -> Vec<Span<'a>> {
    let theme = &app.theme;
    if let Some(t) = &app.toast {
        return vec![
            Span::styled(glyph(t.tone), theme.tone(t.tone)),
            Span::styled(format!(" {}", t.text), theme.text),
        ];
    }
    let filter = &app.state().filter;
    let editing = app.mode == Mode::Filter;
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
