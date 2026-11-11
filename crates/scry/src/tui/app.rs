use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use scry_core::view::{Board, Load, Tab, Target, Tone};

use super::theme::Theme;
use super::views::{self, Row, Shown, Sort};

const TOAST_FOR: Duration = Duration::from_secs(4);
const PAGE: isize = 10;

pub enum Source {
    Live,
    Fixture(PathBuf),
}

impl Source {
    pub fn load(&self) -> Result<Board> {
        match self {
            Source::Live => Ok(Board::from_snapshot(&scry_core::sys::scan()?)),
            Source::Fixture(path) => load_fixture(path),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Source::Live => "live".into(),
            Source::Fixture(p) => format!(
                "fixture {}",
                p.file_name().unwrap_or(p.as_os_str()).to_string_lossy()
            ),
        }
    }
}

/// A recorded `Snapshot` is drawn through the same pure function as live data; a recorded
/// `Board` (design fixtures) is drawn as it is.
fn load_fixture(path: &Path) -> Result<Board> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if json.get("processes").is_some() {
        let snap = scry_core::fixture::parse(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Board::from_snapshot(&snap))
    } else {
        Board::parse(&text).with_context(|| format!("parsing {} as a board", path.display()))
    }
}

/// One mode at a time: the keys mean different things in each.
#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Normal,
    /// Typing into the `/` bar of the current view.
    Filter,
    Sheet(Sheet),
    Help,
}

/// A confirmation listing exactly what an action would touch.
#[derive(Clone, Debug, PartialEq)]
pub struct Sheet {
    pub kind: SheetKind,
    pub targets: Vec<Target>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SheetKind {
    Stop,
    Reap,
}

/// What each view remembers while you look elsewhere.
#[derive(Default)]
pub struct ViewState {
    pub selected: Option<String>,
    pub filter: String,
    pub sort: Option<Sort>,
    pub folded: HashSet<String>,
    /// First row on screen; the table moves it to keep the cursor visible.
    pub offset: Cell<usize>,
}

pub struct Toast {
    pub text: String,
    pub tone: Tone,
    pub until: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Quit,
    Suspend,
    Rescan,
}

pub struct App {
    pub board: Board,
    pub source: Source,
    pub theme: Theme,
    pub tab: Tab,
    pub mode: Mode,
    pub views: [ViewState; 5],
    pub toast: Option<Toast>,
}

impl App {
    pub fn new(board: Board, source: Source, theme: Theme) -> Self {
        App {
            board,
            source,
            theme,
            tab: Tab::Overview,
            mode: Mode::Normal,
            views: Default::default(),
            toast: None,
        }
    }

    pub fn state(&self) -> &ViewState {
        &self.views[self.tab.index()]
    }

    fn state_mut(&mut self) -> &mut ViewState {
        &mut self.views[self.tab.index()]
    }

    pub fn rows(&self) -> Load<Vec<Row>> {
        (views::view(self.tab).rows)(&self.board)
    }

    pub fn shown<'a>(&self, rows: &'a [Row]) -> Vec<Shown<'a>> {
        let s = self.state();
        views::shown(rows, s.sort, &s.filter, &s.folded)
    }

    pub fn cursor(&self, shown: &[Shown]) -> usize {
        let selected = self.state().selected.as_deref();
        shown
            .iter()
            .position(|s| Some(s.row.id.as_str()) == selected)
            .unwrap_or(0)
    }

    pub fn selected_row(&self) -> Option<Row> {
        let Load::Ready(rows) = self.rows() else {
            return None;
        };
        let shown = self.shown(&rows);
        shown.get(self.cursor(&shown)).map(|s| s.row.clone())
    }

    /// Swaps in a new board and returns the ids of rows in the current view that are new
    /// or changed, so they can be highlighted.
    pub fn replace_board(&mut self, board: Board) -> Vec<String> {
        let before = self.rows();
        self.board = board;
        match (before, self.rows()) {
            (Load::Ready(old), Load::Ready(new)) => new
                .iter()
                .filter(|r| old.iter().find(|o| o.id == r.id) != Some(*r))
                .map(|r| r.id.clone())
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn notify(&mut self, text: impl Into<String>, tone: Tone) {
        self.toast = Some(Toast {
            text: text.into(),
            tone,
            until: Instant::now() + TOAST_FOR,
        });
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => Action::Quit,
                KeyCode::Char('z') => Action::Suspend,
                KeyCode::Char('u') if self.mode == Mode::Filter => {
                    self.state_mut().filter.clear();
                    Action::Redraw
                }
                KeyCode::Char('d') => self.step(PAGE),
                KeyCode::Char('u') => self.step(-PAGE),
                _ => Action::None,
            };
        }
        match &self.mode {
            Mode::Normal => self.on_normal(key.code),
            Mode::Filter => self.on_filter(key.code),
            Mode::Sheet(_) => self.on_sheet(key.code),
            Mode::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('?' | 'q') | KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    Action::Redraw
                }
                _ => Action::None,
            },
        }
    }

    fn on_normal(&mut self, code: KeyCode) -> Action {
        match code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc if !self.state().filter.is_empty() => {
                self.state_mut().filter.clear();
                Action::Redraw
            }
            KeyCode::Esc => Action::Quit,
            KeyCode::Char('?') => self.enter(Mode::Help),
            KeyCode::Char('/') => self.enter(Mode::Filter),
            KeyCode::Char(c @ '1'..='5') => self.go(Tab::ALL[(c as u8 - b'1') as usize]),
            KeyCode::Tab => self.go(Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()]),
            KeyCode::BackTab => {
                self.go(Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()])
            }
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::PageDown => self.step(PAGE),
            KeyCode::PageUp => self.step(-PAGE),
            KeyCode::Char('g') | KeyCode::Home => self.step(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.step(isize::MAX / 2),
            KeyCode::Enter => self.open(),
            KeyCode::Char('l') | KeyCode::Right => self.fold(false),
            KeyCode::Char('h') | KeyCode::Left => self.fold(true),
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('S') => {
                let s = &mut self.state_mut().sort;
                if let Some(sort) = s {
                    sort.descending = !sort.descending;
                }
                Action::Redraw
            }
            KeyCode::Char('x') => self.ask_stop(),
            KeyCode::Char('X') => self.ask_reap(),
            KeyCode::Char('r') => Action::Rescan,
            _ => Action::None,
        }
    }

    fn on_filter(&mut self, code: KeyCode) -> Action {
        match code {
            KeyCode::Esc => {
                self.state_mut().filter.clear();
                self.enter(Mode::Normal)
            }
            KeyCode::Enter => self.enter(Mode::Normal),
            KeyCode::Backspace => {
                self.state_mut().filter.pop();
                Action::Redraw
            }
            KeyCode::Down => self.step(1),
            KeyCode::Up => self.step(-1),
            KeyCode::Char(c) => {
                self.state_mut().filter.push(c);
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    fn on_sheet(&mut self, code: KeyCode) -> Action {
        match code {
            KeyCode::Enter | KeyCode::Char('y') => {
                let Mode::Sheet(sheet) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    return Action::None;
                };
                let verb = match sheet.kind {
                    SheetKind::Stop => "Stopping",
                    SheetKind::Reap => "Reaping",
                };
                self.notify(
                    format!("Nothing was sent. {verb} is not built yet."),
                    Tone::Info,
                );
                Action::Redraw
            }
            KeyCode::Esc | KeyCode::Char('n' | 'q') => self.enter(Mode::Normal),
            _ => Action::None,
        }
    }

    fn enter(&mut self, mode: Mode) -> Action {
        self.mode = mode;
        Action::Redraw
    }

    fn go(&mut self, tab: Tab) -> Action {
        self.tab = tab;
        Action::Redraw
    }

    fn step(&mut self, by: isize) -> Action {
        let Load::Ready(rows) = self.rows() else {
            return Action::None;
        };
        let shown = self.shown(&rows);
        if shown.is_empty() {
            return Action::None;
        }
        let at = self.cursor(&shown) as isize;
        let to = at.saturating_add(by).clamp(0, shown.len() as isize - 1) as usize;
        let id = shown[to].row.id.clone();
        self.state_mut().selected = Some(id);
        Action::Redraw
    }

    /// Enter folds a parent, or jumps to the row an Overview line stands for.
    fn open(&mut self) -> Action {
        let Some(row) = self.selected_row() else {
            return Action::None;
        };
        if let Some((tab, target)) = row.jump {
            self.tab = tab;
            if let Some(id) = target {
                let state = self.state_mut();
                state.filter.clear();
                state.selected = Some(id);
            }
            return Action::Redraw;
        }
        let folded = self.state().folded.contains(&row.id);
        self.fold(!folded)
    }

    fn fold(&mut self, fold: bool) -> Action {
        let Load::Ready(rows) = self.rows() else {
            return Action::None;
        };
        let Some(i) = self
            .selected_row()
            .and_then(|sel| rows.iter().position(|r| r.id == sel.id))
        else {
            return Action::None;
        };
        let has_children = rows.get(i + 1).is_some_and(|n| n.depth > rows[i].depth);
        let id = rows[i].id.clone();
        let state = self.state_mut();
        if !has_children {
            if !fold || rows[i].depth == 0 {
                return Action::None;
            }
            let parent = rows[..i].iter().rev().find(|r| r.depth < rows[i].depth);
            state.selected = parent.map(|p| p.id.clone());
        } else if fold {
            state.folded.insert(id);
        } else {
            state.folded.remove(&id);
        }
        Action::Redraw
    }

    /// `s` walks the sortable columns, then back to the natural order. Numbers sort largest
    /// first, words from A.
    fn cycle_sort(&mut self) -> Action {
        let columns = views::view(self.tab).columns;
        let sortable: Vec<usize> = (0..columns.len())
            .filter(|&i| columns[i].sortable)
            .collect();
        let next = match self.state().sort {
            None => sortable.first().copied(),
            Some(s) => sortable.iter().copied().find(|&i| i > s.column),
        };
        let rows = self.rows();
        let sort = next.map(|column| Sort {
            column,
            descending: rows.ready().is_some_and(|r| views::is_numeric(r, column)),
        });
        self.state_mut().sort = sort;
        Action::Redraw
    }

    fn ask_stop(&mut self) -> Action {
        match self.selected_row().and_then(|r| r.target) {
            Some(t) => self.enter(Mode::Sheet(Sheet {
                kind: SheetKind::Stop,
                targets: vec![t],
            })),
            None => {
                self.notify("Nothing on this row can be stopped.", Tone::Quiet);
                Action::Redraw
            }
        }
    }

    fn ask_reap(&mut self) -> Action {
        let Some(reap) = views::view(self.tab).reap else {
            return Action::None;
        };
        let targets = reap(&self.board);
        if targets.is_empty() {
            self.notify("No leftovers to reap.", Tone::Ok);
            return Action::Redraw;
        }
        self.enter(Mode::Sheet(Sheet {
            kind: SheetKind::Reap,
            targets,
        }))
    }
}
