//! The UI model and its update: what is on screen, which mode the keys are in, and how
//! each key changes it. Nothing here draws.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use scry_core::disk::catalog::Kind;
use scry_core::disk::clean::{Allows, Plan};
use scry_core::engine::Labels;
use scry_core::view::{Board, Load, Tab, Target, Tone};

use super::theme::Theme;
use super::ui::MIN_W;
use super::views::{self, Query, Row, Shown, Sort};

const TOAST_FOR: Duration = Duration::from_secs(4);
/// A scan shorter than this never shows the spinner, so a quick automatic refresh changes
/// nothing on screen.
const SHOW_SCAN_AFTER: Duration = Duration::from_millis(300);
/// A reading that takes longer than this also says what it is doing, beside the spinner.
const SAY_SCAN_AFTER: Duration = Duration::from_secs(1);
/// The header's spinner while a reading is in flight, drawn muted at 10 frames a second.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPIN_FRAME: Duration = Duration::from_millis(100);
const PAGE: isize = 10;

pub enum Source {
    Live,
    Fixture(PathBuf),
}

impl Source {
    /// One reading. The live TUI reads in the background instead; see `tui::run`.
    pub fn load(&self) -> Result<Board> {
        match self {
            Source::Live => scry_core::live::Collector::new(Labels::All).board(),
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
    /// A cleanup waiting for its confirmation; a delete waits for its name to be typed.
    Clean(CleanSheet),
    Help,
}

/// What a cleanup will touch, as its confirmation shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct CleanSheet {
    pub plan: Plan,
    /// The folder as the list shows it, home written as `~`.
    pub shown: String,
    /// Typed so far; a delete needs the plan's word.
    pub typed: String,
}

/// A confirmation listing exactly what an action would touch.
#[derive(Clone, Debug, PartialEq)]
pub struct Sheet {
    pub kind: SheetKind,
    pub targets: Vec<Target>,
    /// First target on screen when they do not all fit. Kept as it is through a resize; the
    /// sheet draws it clamped to the size of the moment.
    pub offset: Cell<usize>,
    /// The highest `offset` that still fills the sheet at the size last drawn.
    pub max_offset: Cell<usize>,
}

impl Sheet {
    pub fn new(kind: SheetKind, targets: Vec<Target>) -> Sheet {
        Sheet {
            kind,
            targets,
            offset: Cell::new(0),
            max_offset: Cell::new(usize::MAX),
        }
    }
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

/// Which live readings are in flight, for the header. A fixture never changes, so it has none.
#[derive(Debug, Default)]
pub struct Fresh {
    /// When the process scan in progress started.
    pub scanning: Option<Instant>,
    /// `r` asked for the scan in progress: it shows at once and ends with a toast.
    pub asked: bool,
    /// When the worktree and disk reading in progress started.
    pub worktrees: Option<Instant>,
    /// When the last worktree and disk reading, sizes included, finished.
    pub worktrees_at: Option<Instant>,
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
    /// Run this cleanup, with what was typed to confirm a delete.
    Clean(Box<(Plan, Option<String>)>),
}

pub struct App {
    pub board: Board,
    pub source: Source,
    pub theme: Theme,
    pub tab: Tab,
    pub mode: Mode,
    pub views: [ViewState; 5],
    pub toast: Option<Toast>,
    pub fresh: Fresh,
    /// How wide the list was last drawn, which decides the columns on screen. Keys act on
    /// what was last drawn.
    pub list_width: Cell<u16>,
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
            fresh: Fresh::default(),
            list_width: Cell::new(MIN_W),
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

    /// The columns on screen at the last drawn width.
    pub fn columns(&self) -> Vec<usize> {
        views::visible(views::view(self.tab).columns, self.list_width.get())
    }

    /// The sort in effect: none while its column is hidden, so the order never follows a
    /// column without an arrow on screen.
    pub fn sort(&self) -> Option<Sort> {
        let columns = self.columns();
        self.state().sort.filter(|s| columns.contains(&s.column))
    }

    fn filtering(&self) -> bool {
        !self.state().filter.trim().is_empty()
    }

    pub fn shown<'a>(&self, rows: &'a [Row]) -> Vec<Shown<'a>> {
        let s = self.state();
        views::shown(
            rows,
            &Query {
                sort: self.sort(),
                filter: &s.filter,
                folded: &s.folded,
                columns: &self.columns(),
            },
        )
    }

    /// The selected row, or the first row the filter matched when that is gone.
    pub fn cursor(&self, shown: &[Shown]) -> usize {
        let selected = self.state().selected.as_deref();
        shown
            .iter()
            .position(|s| Some(s.row.id.as_str()) == selected)
            .or_else(|| shown.iter().position(|s| s.matched))
            .unwrap_or(0)
    }

    pub fn selected_row(&self) -> Option<Row> {
        let Load::Ready(rows) = self.rows() else {
            return None;
        };
        let shown = self.shown(&rows);
        shown.get(self.cursor(&shown)).map(|s| s.row.clone())
    }

    /// Swaps in a new board and returns the ids of rows in the current view that are new or
    /// whose state changed, so they can glow. Numbers that move on every sample, such as
    /// RAM, do not count.
    pub fn replace_board(&mut self, board: Board) -> Vec<String> {
        let before = self.rows();
        self.board = board;
        match (before, self.rows()) {
            (Load::Ready(old), Load::Ready(new)) => new
                .iter()
                .filter(|r| {
                    old.iter()
                        .find(|o| o.id == r.id)
                        .is_none_or(|o| o.state() != r.state())
                })
                .map(|r| r.id.clone())
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn notify(&mut self, text: impl Into<String>, tone: Tone) {
        self.notify_for(text, tone, TOAST_FOR);
    }

    pub fn notify_for(&mut self, text: impl Into<String>, tone: Tone, how_long: Duration) {
        self.toast = Some(Toast {
            text: text.into(),
            tone,
            until: Instant::now() + how_long,
        });
    }

    /// Whether a new board would put anything different on screen in this view: the header,
    /// the tab badges or the rows. Sort keys are left out, since they hold exact numbers,
    /// such as an age in seconds, under text that moves less often.
    pub fn looks_the_same(&self, board: &Board) -> bool {
        let header = |b: &Board| match &b.pulse {
            Some(p) => (Some(p.clone()), None),
            None => (None, Some(b.overview.counts_line())),
        };
        let badges = |b: &Board| Tab::ALL.map(|t| (views::view(t).badge)(b));
        let rows = |b: &Board| (views::view(self.tab).rows)(b);
        header(&self.board) == header(board)
            && badges(&self.board) == badges(board)
            && match (rows(&self.board), rows(board)) {
                (Load::Ready(a), Load::Ready(b)) => {
                    a.len() == b.len() && a.iter().zip(&b).all(|(a, b)| a.looks_like(b))
                }
                (a, b) => a == b,
            }
    }

    /// The reading in flight that this view waits on: when it started, and what to call it
    /// once it is slow.
    fn reading(&self) -> Option<(Instant, &'static str)> {
        let f = &self.fresh;
        if matches!(self.tab, Tab::Worktrees | Tab::Disk)
            && let Some(started) = f.worktrees
        {
            let sizing = match self.tab {
                Tab::Disk => self.board.disk.ready().is_some_and(|d| d.sizing),
                _ => self.board.worktrees.ready().is_some_and(|w| w.sizing),
            };
            let doing = match (sizing, self.tab) {
                (true, _) => "measuring sizes…",
                (false, Tab::Disk) => "reading the disk…",
                (false, _) => "reading worktrees…",
            };
            return Some((started, doing));
        }
        f.scanning.map(|started| (started, "refreshing…"))
    }

    /// The header's right end: a fixture's name, or for live data a spinner while a reading
    /// runs and nothing when idle. A quick automatic reading never shows; one that `r` asked
    /// for shows at once, and one past a second says what it is doing.
    pub fn activity(&self, now: Instant) -> String {
        if !matches!(self.source, Source::Live) {
            return self.source.label();
        }
        let Some((started, doing)) = self.reading() else {
            return String::new();
        };
        let ran = now.saturating_duration_since(started);
        if !self.fresh.asked && ran < SHOW_SCAN_AFTER {
            return String::new();
        }
        let frame = SPINNER[(ran.as_millis() / SPIN_FRAME.as_millis()) as usize % SPINNER.len()];
        if ran >= SAY_SCAN_AFTER {
            format!("{frame} {doing}")
        } else {
            frame.to_string()
        }
    }

    /// When `activity` changes by itself with nothing arriving: the spinner appears or
    /// turns. `None` while nothing is read.
    pub fn activity_due(&self, now: Instant) -> Option<Instant> {
        let (started, _) = self.reading()?;
        let ran = now.saturating_duration_since(started);
        if !self.fresh.asked && ran < SHOW_SCAN_AFTER {
            return Some(started + SHOW_SCAN_AFTER);
        }
        let frames = ran.as_millis() / SPIN_FRAME.as_millis() + 1;
        Some(started + SPIN_FRAME * frames as u32)
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let page = |app: &mut App, by: isize| match app.mode {
                Mode::Sheet(_) => app.scroll_sheet(by),
                _ => app.step(by),
            };
            return match key.code {
                KeyCode::Char('c') => Action::Quit,
                KeyCode::Char('z') => Action::Suspend,
                KeyCode::Char('u') if self.mode == Mode::Filter => {
                    self.state_mut().filter.clear();
                    self.follow_filter()
                }
                KeyCode::Char('d') => page(self, PAGE),
                KeyCode::Char('u') => page(self, -PAGE),
                _ => Action::None,
            };
        }
        match &self.mode {
            Mode::Normal => self.on_normal(key.code),
            Mode::Filter if key.modifiers.contains(KeyModifiers::ALT) => Action::None,
            Mode::Filter => self.on_filter(key.code),
            Mode::Sheet(_) => self.on_sheet(key.code),
            Mode::Clean(_) => self.on_clean(key.code),
            Mode::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('?' | 'q') | KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    Action::Redraw
                }
                _ => Action::None,
            },
        }
    }

    /// Only `q` and ctrl-c quit, so an extra esc never closes Scry.
    fn on_normal(&mut self, code: KeyCode) -> Action {
        match code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc if !self.state().filter.is_empty() => {
                self.state_mut().filter.clear();
                Action::Redraw
            }
            KeyCode::Esc => Action::None,
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
                if let Some(sort) = self.sort() {
                    self.state_mut().sort = Some(Sort {
                        descending: !sort.descending,
                        ..sort
                    });
                }
                Action::Redraw
            }
            KeyCode::Char('x') => self.ask_stop(),
            KeyCode::Char('X') => self.ask_reap(),
            KeyCode::Char('t') => self.ask_clean(),
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
                self.follow_filter()
            }
            KeyCode::Down => self.step(1),
            KeyCode::Up => self.step(-1),
            KeyCode::Char(c) => {
                self.state_mut().filter.push(c);
                self.follow_filter()
            }
            _ => Action::None,
        }
    }

    /// Keeps the selection while it still matches, else moves it to the first row that
    /// does, never to a row kept only as a match's ancestor.
    fn follow_filter(&mut self) -> Action {
        if let Load::Ready(rows) = self.rows() {
            let shown = self.shown(&rows);
            let selected = self.state().selected.as_deref();
            let stays = shown
                .iter()
                .any(|s| s.matched && Some(s.row.id.as_str()) == selected);
            let first = shown.iter().find(|s| s.matched).map(|s| s.row.id.clone());
            if !stays && first.is_some() {
                self.state_mut().selected = first;
            }
        }
        Action::Redraw
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
            KeyCode::Char('j') | KeyCode::Down => self.scroll_sheet(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_sheet(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_sheet(PAGE),
            KeyCode::PageUp => self.scroll_sheet(-PAGE),
            KeyCode::Char('g') | KeyCode::Home => self.scroll_sheet(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.scroll_sheet(isize::MAX / 2),
            _ => Action::None,
        }
    }

    /// Enter confirms, once a delete's word is typed exactly; esc cancels. A delete's sheet
    /// takes every other key as typing.
    fn on_clean(&mut self, code: KeyCode) -> Action {
        let Mode::Clean(sheet) = &mut self.mode else {
            return Action::None;
        };
        let word = sheet.plan.word.clone();
        match (code, &word) {
            (KeyCode::Esc, _) | (KeyCode::Char('n' | 'q'), None) => self.enter(Mode::Normal),
            (KeyCode::Enter | KeyCode::Char('y'), None) | (KeyCode::Enter, Some(_)) => {
                if let Some(w) = &word
                    && sheet.typed.trim() != w
                {
                    self.notify(format!("Type {w} exactly to delete it."), Tone::Warn);
                    return Action::Redraw;
                }
                let Mode::Clean(sheet) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    return Action::None;
                };
                let typed = word.map(|_| sheet.typed);
                Action::Clean(Box::new((sheet.plan, typed)))
            }
            (KeyCode::Backspace, Some(_)) => {
                sheet.typed.pop();
                Action::Redraw
            }
            (KeyCode::Char(c), Some(_)) => {
                sheet.typed.push(c);
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    /// `t` on Disk or Worktrees: a confirmation for what the item allows, or a word on why
    /// there is none.
    fn ask_clean(&mut self) -> Action {
        if !matches!(self.tab, Tab::Disk | Tab::Worktrees) {
            return Action::None;
        }
        let Some(row) = self.selected_row() else {
            return Action::None;
        };
        let Some(item) = views::disk_item(self.tab, &self.board, &row.id) else {
            let why = match self.board.disk {
                Load::Ready(_) => "Only worktrees that can go or can be pruned are cleaned up.",
                _ => "Still reading the disk.",
            };
            self.notify(why, Tone::Quiet);
            return Action::Redraw;
        };
        let (text, tone) = match (&item.plan, &item.allows) {
            (Some(plan), _) => {
                let sheet = CleanSheet {
                    plan: plan.clone(),
                    shown: item.path.clone(),
                    typed: String::new(),
                };
                return self.enter(Mode::Clean(sheet));
            }
            (None, Allows::Command { command }) => (
                format!("Scry runs nothing here. Run it yourself: {command}"),
                Tone::Info,
            ),
            // A worktree's reason is its label; a catalog entry's is in its detail already.
            (None, Allows::Never { why }) if item.kind == Kind::Worktree => {
                (why.clone(), Tone::Quiet)
            }
            (None, Allows::Never { .. }) => (
                "Scry does not clean this up; it shows its size only.".to_string(),
                Tone::Quiet,
            ),
            (None, Allows::Off { .. }) => {
                let config = self
                    .board
                    .disk
                    .ready()
                    .map_or("config.toml".to_string(), |l| l.config.clone());
                (
                    format!("Cleanup is off: set destructive = true in {config}."),
                    Tone::Quiet,
                )
            }
            (None, _) => ("It is not measured yet.".to_string(), Tone::Quiet),
        };
        self.notify(text, tone);
        Action::Redraw
    }

    fn scroll_sheet(&mut self, by: isize) -> Action {
        let Mode::Sheet(sheet) = &self.mode else {
            return Action::None;
        };
        // From where the sheet is on screen, so a key never moves an offset that a smaller
        // terminal clamped away.
        let max = sheet
            .max_offset
            .get()
            .min(sheet.targets.len().saturating_sub(1));
        let at = sheet.offset.get().min(max) as isize;
        sheet
            .offset
            .set(at.saturating_add(by).clamp(0, max as isize) as usize);
        Action::Redraw
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

    /// Enter jumps to the row an Overview line stands for, or folds and unfolds a parent.
    /// On a row without children it does nothing.
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
        let Load::Ready(rows) = self.rows() else {
            return Action::None;
        };
        if !rows
            .iter()
            .position(|r| r.id == row.id)
            .is_some_and(|i| has_children(&rows, i))
        {
            return Action::None;
        }
        let folded = self.state().folded.contains(&row.id);
        self.fold(!folded)
    }

    /// `h` folds a parent, or moves from a child to its parent; `l` unfolds.
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
        if !has_children(&rows, i) {
            if !fold || rows[i].depth == 0 {
                return Action::None;
            }
            let parent = rows[..i].iter().rev().find(|r| r.depth < rows[i].depth);
            self.state_mut().selected = parent.map(|p| p.id.clone());
            return Action::Redraw;
        }
        // The filter shows every match whatever is folded, so a fold made now would change
        // nothing on screen until the filter is cleared.
        if self.filtering() {
            self.notify(
                "Folding is off while a filter is applied. esc clears it.",
                Tone::Quiet,
            );
            return Action::Redraw;
        }
        let id = rows[i].id.clone();
        let state = self.state_mut();
        if fold {
            state.folded.insert(id);
        } else {
            state.folded.remove(&id);
        }
        Action::Redraw
    }

    /// `s` walks the sortable columns on screen, then back to the natural order. Numbers
    /// sort largest first, words from A.
    fn cycle_sort(&mut self) -> Action {
        let columns = views::view(self.tab).columns;
        let sortable: Vec<usize> = self
            .columns()
            .into_iter()
            .filter(|&i| columns[i].sortable)
            .collect();
        let next = match self.sort() {
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
            Some(t) => self.enter(Mode::Sheet(Sheet::new(SheetKind::Stop, vec![t]))),
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
        // Nothing to reap is an all-clear only when the view knows what is running.
        match self.rows() {
            Load::Ready(_) => {}
            Load::Unavailable(why) | Load::Failed(why) => {
                self.notify(why, Tone::Quiet);
                return Action::Redraw;
            }
            Load::Loading => {
                self.notify(
                    format!("Still reading {}.", self.tab.title().to_lowercase()),
                    Tone::Quiet,
                );
                return Action::Redraw;
            }
        }
        let targets = reap(&self.board);
        if targets.is_empty() {
            self.notify("No leftovers to reap.", Tone::Ok);
            return Action::Redraw;
        }
        self.enter(Mode::Sheet(Sheet::new(SheetKind::Reap, targets)))
    }
}

fn has_children(rows: &[Row], i: usize) -> bool {
    rows.get(i + 1).is_some_and(|n| n.depth > rows[i].depth)
}
