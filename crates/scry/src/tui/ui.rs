//! Lays out one frame from the app state: a header line and a rule, the body, and the
//! footer. The body is the view's list with the selected row's detail beside it from
//! `BESIDE_FROM` columns, or under it when narrower. The Overview opens with gauges; Disk
//! with its volume and, until `v`, a treemap in place of the list. Pure apart from the
//! list's scroll offset and where things landed, which the mouse reads.

use std::cell::Cell;
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Span;
use ratatui::widgets::{Paragraph, Wrap};
use scry_core::view::{Load, Tab};

use super::app::{App, Hits, Mode};
use super::kit::detail::{self, Pane};
use super::kit::states::Status;
use super::kit::table::ListView;
use super::kit::treemap::{self, Item};
use super::kit::{chrome, gauges, overlay, states, table};
use super::theme::{Theme, glyph};
use super::views::{self, Chip, Row, ViewDef};

pub const MIN_W: u16 = 80;
pub const MIN_H: u16 = 24;
/// The detail pane moves beside the list from this width.
const BESIDE_FROM: u16 = 100;
const PANE_MIN: u16 = 40;
const PANE_MAX: u16 = 60;

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
        *app.hits.borrow_mut() = Hits::default();
        return Painted::default();
    }

    let [header, rule, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let badges = Tab::ALL.map(|t| (views::view(t).badge)(&app.board));
    let tabs = chrome::header(
        f,
        header,
        theme,
        &app.board,
        (app.tab, badges),
        app.tab != Tab::Overview,
        &app.activity(Instant::now()),
    );
    chrome::rule(f, rule, theme);

    let mut painted = Painted::default();
    let mut hits = Hits {
        tabs,
        ..Hits::default()
    };
    let view = views::view(app.tab);
    let rows = app.rows();
    let mut count = None;

    if let (Mode::Detail(scroll), Load::Ready(rows)) = (&app.mode, &rows)
        && let Some(row) = selected(app, rows)
        && let Some(pane) = pane(app, view, &row, true)
    {
        detail::beside(f, body, theme, &pane, scroll);
    } else {
        let main = match app.tab {
            Tab::Overview => overview_top(f, body, app),
            _ => body,
        };
        match &rows {
            Load::Ready(rows) if !rows.is_empty() => {
                count = Some(list(f, main, app, view, rows, &mut hits, &mut painted));
            }
            other => {
                let (list_area, _) = split(f, main, theme, None);
                let mut list_area = section(f, list_area, app, view);
                // Disk's volume stays above a list with nothing in it.
                if app.tab == Tab::Disk
                    && let Some(lines) = views::disk_lines(&app.board)
                {
                    let space = app.board.disk.ready().and_then(|d| d.space.as_ref());
                    let [_, vol, rest] = Layout::vertical([
                        Constraint::Length(1),
                        Constraint::Length(treemap::VOLUME_H),
                        Constraint::Min(0),
                    ])
                    .areas(list_area);
                    treemap::volume(f, vol, theme, space, &lines);
                    list_area = rest;
                }
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
                states::status(f, list_area, theme, app.tab.title(), status);
            }
        }
    }

    let left = footer_left(app, count);
    let hints = hints(app, view, &rows);
    chrome::footer(f, footer, theme, left, &hints);

    match &app.mode {
        Mode::Help => {
            overlay::scrim(f, body, theme);
            overlay::help(f, body, theme);
        }
        Mode::Sheet(sheet) => {
            overlay::scrim(f, body, theme);
            painted.sheet = Some(overlay::sheet(f, body, theme, sheet));
        }
        Mode::Clean(sheet) => {
            overlay::scrim(f, body, theme);
            painted.sheet = Some(overlay::clean_sheet(f, body, theme, sheet));
        }
        Mode::Palette(p) => {
            overlay::scrim(f, body, theme);
            let entries: Vec<(&str, &str)> = app
                .commands(&p.query)
                .iter()
                .map(|c| (c.label, c.keys))
                .collect();
            overlay::palette(f, body, theme, &p.query, &entries, p.cursor);
        }
        _ => {}
    }
    *app.hits.borrow_mut() = hits;
    painted
}

/// The selected row of a list, as the list shows it.
fn selected(app: &App, rows: &[Row]) -> Option<Row> {
    let shown = app.shown(rows);
    shown.get(app.cursor(&shown)).map(|s| s.row.clone())
}

/// The gauges over the Overview's list, and a rule under them. Returns what is left.
fn overview_top(f: &mut Frame, body: Rect, app: &App) -> Rect {
    let [_, top, _, rule, rest] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(gauges::HEIGHT),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(body);
    let inner = Rect {
        x: top.x + 2,
        width: top.width.saturating_sub(4),
        ..top
    };
    gauges::gauges(f, inner, &app.theme, &app.board);
    chrome::rule(f, rule, &app.theme);
    rest
}

/// The list area and the pane: side by side from `BESIDE_FROM`, else the pane, `stacked`
/// rows tall, right under the list's `rows` with a rule between. The list area is inset.
fn split(
    f: &mut Frame,
    body: Rect,
    theme: &Theme,
    stacked: Option<(u16, u16)>,
) -> (Rect, Option<(Rect, bool)>) {
    let inset = |r: Rect, x: u16| Rect {
        x: r.x + x,
        y: r.y + 1,
        width: r.width.saturating_sub(2 * x),
        height: r.height.saturating_sub(1),
    };
    if body.width >= BESIDE_FROM {
        let w = (body.width * 2 / 5).clamp(PANE_MIN, PANE_MAX);
        let [list, bar, pane] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(w),
        ])
        .areas(body);
        chrome::bar(f, bar, theme);
        return (inset(list, 2), Some((pane, true)));
    }
    let Some((rows, pane_h)) = stacked.filter(|s| s.1 > 0) else {
        return (inset(body, 1), None);
    };
    // The list's top inset, its rows, then the rule and the pane.
    let room = body.height.saturating_sub(pane_h + 1);
    let list_h = (1 + rows).min(room);
    let [list, rule, pane, _] = Layout::vertical([
        Constraint::Length(list_h),
        Constraint::Length(1),
        Constraint::Length(pane_h),
        Constraint::Min(0),
    ])
    .areas(body);
    chrome::rule(f, rule, theme);
    (inset(list, 1), Some((pane, false)))
}

/// The section title over a list: its name, its note, and what orders it. Returns the area
/// under it.
fn section(f: &mut Frame, area: Rect, app: &App, view: &ViewDef) -> Rect {
    let [title, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let note = (view.note)(&app.board).unwrap_or_default();
    let order = match app.sort() {
        Some(s) => format!(
            "by {} {}",
            view.columns[s.column].title,
            if s.descending { "↓" } else { "↑" }
        ),
        None => String::new(),
    };
    chrome::section(f, title, &app.theme, view.title, &note, &order);
    rest
}

/// Draws a view's rows, as a list or Disk's treemap, and the selected row's pane. Returns
/// how many rows show of how many.
fn list(
    f: &mut Frame,
    body: Rect,
    app: &App,
    view: &ViewDef,
    rows: &[Row],
    hits: &mut Hits,
    painted: &mut Painted,
) -> (usize, usize) {
    let theme = &app.theme;
    let shown = app.shown(rows);
    let cursor = app.cursor(&shown);
    let sel = shown.get(cursor).map(|s| s.row.clone());
    let narrow = body.width < BESIDE_FROM;
    let stacked = sel
        .as_ref()
        .and_then(|r| pane(app, view, r, false))
        .map(|p| {
            let cap = if body.height >= 24 { 6 } else { 4 };
            detail::stacked_height(&p, body.width).min(cap)
        });
    // Disk's volume and treemap fill the list area whatever the rows.
    let map = app.disk_map_on();
    let tall = map || app.tab == Tab::Disk;
    let list_rows = if tall {
        u16::MAX / 2
    } else {
        shown.len() as u16 + 1
    };
    let (list_area, pane_at) = split(
        f,
        body,
        theme,
        narrow.then_some((list_rows, stacked.unwrap_or(0))),
    );
    app.list_width.set(list_area.width);
    let mut under = section(f, list_area, app, view);

    if app.tab == Tab::Disk
        && let Some(lines) = views::disk_lines(&app.board)
    {
        let space = app.board.disk.ready().and_then(|d| d.space.as_ref());
        // On a short terminal the gaps go first, so the treemap keeps its rows.
        let gap = u16::from(under.height >= 30);
        let [_, vol, _, rest] = Layout::vertical([
            Constraint::Length(gap),
            Constraint::Length(treemap::VOLUME_H),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(under);
        treemap::volume(f, vol, theme, space, &lines);
        under = rest;
    }

    if map {
        let items: Vec<Item> = app
            .board
            .disk
            .ready()
            .map(|d| {
                d.items
                    .iter()
                    .filter(|i| shown.iter().any(|s| s.row.id == i.id))
                    .map(|i| Item {
                        id: &i.id,
                        name: &i.name,
                        size: i.size.unwrap_or(0),
                        kind: i.kind,
                        cleanable: views::disk_cleanable(i),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut kinds = Vec::new();
        for i in &items {
            if !kinds.contains(&i.kind) {
                kinds.push(i.kind);
            }
        }
        let used = treemap::legend(f.buffer_mut(), under, theme, &kinds);
        let gap = u16::from(under.height >= 20);
        let map_area = Rect {
            y: under.y + used + gap,
            height: under.height.saturating_sub(used + gap),
            ..under
        };
        let tiles = treemap::treemap(
            f,
            map_area,
            theme,
            &items,
            sel.as_ref().map(|r| r.id.as_str()),
        );
        painted.rows = tiles.clone();
        hits.tiles = tiles;
    } else {
        painted.rows = table::table(
            f,
            under,
            theme,
            &ListView {
                columns: view.columns,
                rows,
                shown: &shown,
                cursor,
            },
            &app.state().offset,
        );
        hits.rows = painted.rows.clone();
    }
    if shown.is_empty() {
        states::status(
            f,
            under,
            theme,
            app.tab.title(),
            Status::Empty {
                title: "No rows match the filter.",
                hint: "esc clears it",
            },
        );
    }

    if let (Some((pane_area, beside)), Some(row)) = (pane_at, &sel)
        && let Some(p) = pane(app, view, row, beside)
    {
        if beside {
            detail::beside(f, pane_area, theme, &p, &Cell::new(0));
        } else {
            detail::stacked(f, pane_area, theme, &p);
        }
    }
    (shown.len(), rows.len())
}

/// The pane for `row`: its detail, its state and chips, and with room the view's ranking.
fn pane<'a>(app: &'a App, view: &'a ViewDef, row: &'a Row, full: bool) -> Option<Pane<'a>> {
    let theme = &app.theme;
    let detail = row.detail.as_ref()?;
    let mut chips = Vec::new();
    if let Some(s) = row.state() {
        chips.push((
            format!("{} {}", glyph(s.tone()), s.word()),
            theme.chip(s.tone()),
        ));
    }
    for c in &row.chips {
        chips.push(match c {
            Chip::Tone(text, tone) => (text.clone(), theme.chip(*tone)),
            Chip::Kind(text, kind) => (text.clone(), theme.kind_chip(*kind)),
        });
    }
    Some(Pane {
        detail,
        chips,
        about: view.about,
        bars: if full {
            view.bars.and_then(|b| b(&app.board))
        } else {
            None
        },
    })
}

/// Keys that do something on this view as it is now.
fn hints(app: &App, view: &ViewDef, rows: &Load<Vec<Row>>) -> Vec<(&'static str, &'static str)> {
    let mut hints = Vec::new();
    match &app.mode {
        Mode::Filter => return vec![("⏎", "keep"), ("esc", "clear"), ("↑/↓", "move")],
        Mode::Sheet(s) => return vec![("⏎", overlay::confirm_label(s.kind)), ("esc", "cancel")],
        Mode::Clean(s) => return vec![("⏎", overlay::clean_label(s)), ("esc", "cancel")],
        Mode::Help => return vec![("esc", "close")],
        Mode::Palette(_) => return vec![("⏎", "run"), ("↑/↓", "move"), ("esc", "close")],
        Mode::Detail(_) => {
            hints.extend([("j/k", "scroll"), ("esc", "back")]);
            hints.extend(view.hints.iter().copied().filter(|(k, _)| *k != "⏎"));
        }
        Mode::Normal => match rows {
            Load::Ready(rows) if !rows.is_empty() => {
                if app.disk_map_on() {
                    hints.push(("hjkl", "move"));
                } else {
                    hints.push(("j/k", "move"));
                }
                if !view.hints.iter().any(|(k, _)| *k == "⏎") {
                    hints.push(("⏎", "details"));
                }
                hints.extend(view.hints.iter().copied());
                if app.tab == Tab::Disk {
                    hints.push(("v", if app.disk_map { "list" } else { "treemap" }));
                }
                hints.push(("/", "filter"));
                if !app.disk_map_on() && app.columns().iter().any(|&i| view.columns[i].sortable) {
                    hints.push(("s", "sort"));
                }
            }
            Load::Failed(_) => hints.push(("r", "retry")),
            _ => {}
        },
    }
    if !matches!(rows, Load::Failed(_)) {
        hints.push(("r", "refresh"));
    }
    hints.extend([("^p", "commands"), ("?", "help"), ("q", "quit")]);
    hints
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
