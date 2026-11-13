//! The UI model and its update: what is on screen, which mode the keys are in, and how
//! each key changes it. Nothing here draws.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use scry_core::disk::catalog::Kind;
use scry_core::disk::clean::{Allows, Plan};
use scry_core::engine::Labels;
use scry_core::keep::{self, KeepAs, KeepEdit};
use scry_core::paths::Paths;
use scry_core::reap::Report;
use scry_core::view::{Board, Load, Tab, Target, Tone};

use super::filter::Fuzzy;
use super::kit::treemap::{self, Dir};
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
    Clean(Box<CleanSheet>),
    Help,
    /// The selected row's detail over the whole body, scrolled this far. The view's own
    /// keys still act on the row.
    Detail(Cell<usize>),
    /// Every command for this view, found by typing.
    Palette(Box<Palette>),
}

/// The command palette: what was typed, and which match is selected.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Palette {
    pub query: String,
    pub cursor: usize,
}

/// One entry of the palette. Running it presses `key` in the list.
pub struct Command {
    pub label: &'static str,
    pub keys: &'static str,
    pub key: KeyCode,
    pub when: fn(&App) -> bool,
}

fn always(_: &App) -> bool {
    true
}

pub static COMMANDS: [Command; 22] = [
    Command {
        label: "Show details",
        keys: "⏎",
        key: KeyCode::Enter,
        when: |a| a.tab != Tab::Overview && a.selected_row().is_some(),
    },
    Command {
        label: "Go to this item",
        keys: "⏎",
        key: KeyCode::Enter,
        when: |a| a.selected_row().is_some_and(|r| r.jump.is_some()),
    },
    Command {
        label: "Fold or unfold the group",
        keys: "space",
        key: KeyCode::Char(' '),
        when: |a| matches!(a.tab, Tab::Agents | Tab::Top),
    },
    Command {
        label: "Stop the selected process",
        keys: "x",
        key: KeyCode::Char('x'),
        when: |a| a.selected_row().is_some_and(|r| r.target.is_some()),
    },
    Command {
        label: "Reap leftovers",
        keys: "X",
        key: KeyCode::Char('X'),
        when: |a| views::view(a.tab).reap.is_some(),
    },
    Command {
        label: "Keep the selected process",
        keys: "K",
        key: KeyCode::Char('K'),
        when: |a| a.selected_row().is_some_and(|r| r.target.is_some()),
    },
    Command {
        label: "Clean up the selected item",
        keys: "t",
        key: KeyCode::Char('t'),
        when: |a| matches!(a.tab, Tab::Disk | Tab::Worktrees),
    },
    Command {
        label: "Switch between treemap and list",
        keys: "v",
        key: KeyCode::Char('v'),
        when: |a| a.tab == Tab::Disk,
    },
    Command {
        label: "Filter",
        keys: "/",
        key: KeyCode::Char('/'),
        when: always,
    },
    Command {
        label: "Sort by the next column",
        keys: "s",
        key: KeyCode::Char('s'),
        when: |a| !a.disk_map_on(),
    },
    Command {
        label: "Reverse the sort",
        keys: "S",
        key: KeyCode::Char('S'),
        when: |a| a.sort().is_some(),
    },
    Command {
        label: "Go to Overview",
        keys: "1",
        key: KeyCode::Char('1'),
        when: always,
    },
    Command {
        label: "Go to Ports",
        keys: "2",
        key: KeyCode::Char('2'),
        when: always,
    },
    Command {
        label: "Go to Agents",
        keys: "3",
        key: KeyCode::Char('3'),
        when: always,
    },
    Command {
        label: "Go to Disk",
        keys: "4",
        key: KeyCode::Char('4'),
        when: always,
    },
    Command {
        label: "Go to Worktrees",
        keys: "5",
        key: KeyCode::Char('5'),
        when: always,
    },
    Command {
        label: "Go to Top: what uses the CPU and memory",
        keys: "6",
        key: KeyCode::Char('6'),
        when: always,
    },
    Command {
        label: "Go to the first row",
        keys: "g",
        key: KeyCode::Char('g'),
        when: always,
    },
    Command {
        label: "Go to the last row",
        keys: "G",
        key: KeyCode::Char('G'),
        when: always,
    },
    Command {
        label: "Refresh now",
        keys: "r",
        key: KeyCode::Char('r'),
        when: always,
    },
    Command {
        label: "Show every key",
        keys: "?",
        key: KeyCode::Char('?'),
        when: always,
    },
    Command {
        label: "Quit",
        keys: "q",
        key: KeyCode::Char('q'),
        when: always,
    },
];

/// Where the last frame put what the mouse can click.
#[derive(Clone, Debug, Default)]
pub struct Hits {
    pub tabs: Vec<(Tab, Rect)>,
    pub rows: Vec<(String, Rect)>,
    pub tiles: Vec<(String, Rect)>,
}

/// What a cleanup will touch, as its confirmation shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct CleanSheet {
    pub plan: Plan,
    /// The folder as the list shows it, home written as `~`.
    pub shown: String,
    /// Typed so far; a delete needs the plan's word.
    pub typed: String,
    /// Said above the confirmation: the agent that writes there is running.
    pub warning: Option<String>,
}

/// A confirmation listing exactly what an action would touch. What it lists and what its
/// signal follows are fixed when it opens: nothing that finishes meanwhile changes them.
#[derive(Clone, Debug, PartialEq)]
pub struct Sheet {
    pub kind: SheetKind,
    /// For `Kill`, the stop or reap whose targets outlived SIGTERM, which decides the checks
    /// before SIGKILL. Otherwise `kind`.
    pub origin: SheetKind,
    pub targets: Vec<Target>,
    /// For `Keep`: the change to the config, shown as a diff, and the config as shown.
    pub keep: Option<Box<(KeepEdit, String)>>,
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
            origin: kind,
            targets,
            keep: None,
            offset: Cell::new(0),
            max_offset: Cell::new(usize::MAX),
        }
    }

    /// SIGKILL for what outlived SIGTERM in a stop or reap, with each age as of now.
    pub fn kill(origin: SheetKind, targets: &[Target]) -> Sheet {
        let now_us = scry_core::sys::now_ms() * 1000;
        let targets = targets
            .iter()
            .map(|t| Target {
                age_s: t
                    .start_us
                    .map_or(t.age_s, |s| ((now_us - s).max(0) / 1_000_000) as u64),
                ..t.clone()
            })
            .collect();
        Sheet {
            origin,
            ..Sheet::new(SheetKind::Kill, targets)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SheetKind {
    Stop,
    Reap,
    /// SIGKILL for what outlived SIGTERM, a second confirmation after a stop or a reap.
    Kill,
    /// Adding a process to the keep list.
    Keep,
}

/// A confirmed action for the live TUI to carry out off the UI thread.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// `Stop`, `Reap` or `Kill`.
    pub kind: SheetKind,
    /// What a `Kill` follows, which decides the check before its signal: a reaped process
    /// must still be a leftover.
    pub origin: SheetKind,
    pub targets: Vec<Target>,
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

#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Redraw,
    Quit,
    Suspend,
    Rescan,
    /// A reading for the lists to show a change at once, with no "Refreshed." after it.
    Reread,
    Act(Request),
    /// Run this cleanup, with what was typed to confirm a delete.
    Clean(Box<(Plan, Option<String>)>),
}

pub struct App {
    pub board: Board,
    pub source: Source,
    pub theme: Theme,
    pub tab: Tab,
    pub mode: Mode,
    pub views: [ViewState; 6],
    pub toast: Option<Toast>,
    pub fresh: Fresh,
    /// How wide the list was last drawn, which decides the columns on screen. Keys act on
    /// what was last drawn.
    pub list_width: Cell<u16>,
    /// Where the config is, in the live TUI. `K` writes the keep list there; a fixture has
    /// none, so nothing is written.
    pub paths: Option<Paths>,
    /// A confirmed action is running.
    pub acting: Option<SheetKind>,
    /// What outlived SIGTERM in the last stop or reap, and which it was. `X` offers SIGKILL
    /// for them until it is sent or declined.
    pub stubborn: Option<(SheetKind, Vec<Target>)>,
    /// Says which agent that writes into a disk item is running, if one is; asked as a
    /// cleanup's sheet opens. Nothing for a fixture.
    pub writer: fn(&str) -> Option<String>,
    /// Disk shows its items as a treemap; `v` switches to the list and back.
    pub disk_map: bool,
    /// What the last frame drew where, for the mouse and for moving between tiles.
    pub hits: RefCell<Hits>,
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
            paths: None,
            acting: None,
            stubborn: None,
            writer: |_| None,
            disk_map: true,
            hits: RefCell::default(),
        }
    }

    /// Disk is showing the treemap.
    pub fn disk_map_on(&self) -> bool {
        self.tab == Tab::Disk && self.disk_map
    }

    /// The palette's matches for what was typed: those that contain it first, each group in
    /// the table's order.
    pub fn commands(&self, query: &str) -> Vec<&'static Command> {
        let mut fuzzy = Fuzzy::new(query);
        let mut found: Vec<&'static Command> = COMMANDS
            .iter()
            .filter(|c| (c.when)(self))
            .filter(|c| fuzzy.as_mut().is_none_or(|f| f.row(&[c.label]).is_some()))
            .collect();
        // What was typed as it stands comes first; scattered letters after.
        let typed = query.trim().to_lowercase();
        found.sort_by_key(|c| !c.label.to_lowercase().contains(&typed));
        found
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
                fold_all: views::view(self.tab).fold_all,
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
        // Where the selected row stood, so when it goes, as after a cleanup, the cursor stays
        // on the row that takes its place rather than jumping to the top.
        let was_at = before.ready().and_then(|rows| {
            let shown = self.shown(rows);
            let at = self.cursor(&shown);
            let selected = self.state().selected.as_deref()?;
            (shown.get(at)?.row.id == selected).then_some(at)
        });
        self.board = board;
        if let (Some(at), Load::Ready(rows)) = (was_at, self.rows()) {
            let shown = self.shown(&rows);
            let selected = self.state().selected.clone();
            let still = shown.iter().any(|s| Some(&s.row.id) == selected.as_ref());
            if !still && let Some(next) = shown.get(at.min(shown.len().saturating_sub(1))) {
                let id = next.row.id.clone();
                self.state_mut().selected = Some(id);
            }
        }
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
                Mode::Detail(_) => app.scroll_detail(by),
                _ => app.step(by),
            };
            return match key.code {
                KeyCode::Char('c') => Action::Quit,
                KeyCode::Char('z') => Action::Suspend,
                KeyCode::Char('p') => match self.mode {
                    Mode::Palette(_) => self.palette_step(-1),
                    Mode::Normal | Mode::Detail(_) => self.open_palette(),
                    _ => Action::None,
                },
                KeyCode::Char('n') if matches!(self.mode, Mode::Palette(_)) => self.palette_step(1),
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
            Mode::Detail(_) => self.on_detail(key.code),
            Mode::Palette(_) => self.on_palette(key.code),
        }
    }

    /// The expanded detail scrolls with the movement keys and closes with esc or enter;
    /// every other key does what it does in the list, for the same row.
    fn on_detail(&mut self, code: KeyCode) -> Action {
        match code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Backspace | KeyCode::Char('q') => {
                self.enter(Mode::Normal)
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll_detail(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_detail(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_detail(PAGE),
            KeyCode::PageUp => self.scroll_detail(-PAGE),
            KeyCode::Char('g') | KeyCode::Home => self.scroll_detail(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.scroll_detail(isize::MAX / 2),
            other => {
                let tab = self.tab;
                let action = self.on_normal(other);
                if self.tab != tab {
                    self.mode = Mode::Normal;
                }
                action
            }
        }
    }

    fn scroll_detail(&mut self, by: isize) -> Action {
        let Mode::Detail(scroll) = &self.mode else {
            return Action::None;
        };
        scroll.set(scroll.get().saturating_add_signed(by));
        Action::Redraw
    }

    fn open_palette(&mut self) -> Action {
        self.enter(Mode::Palette(Box::default()))
    }

    fn palette_step(&mut self, by: isize) -> Action {
        let Mode::Palette(p) = &self.mode else {
            return Action::None;
        };
        let n = self.commands(&p.query).len();
        let Mode::Palette(p) = &mut self.mode else {
            return Action::None;
        };
        if n > 0 {
            p.cursor = (p.cursor as isize + by).rem_euclid(n as isize) as usize;
        }
        Action::Redraw
    }

    fn on_palette(&mut self, code: KeyCode) -> Action {
        let Mode::Palette(p) = &mut self.mode else {
            return Action::None;
        };
        match code {
            KeyCode::Esc => self.enter(Mode::Normal),
            KeyCode::Up => self.palette_step(-1),
            KeyCode::Down | KeyCode::Tab => self.palette_step(1),
            KeyCode::Backspace => {
                p.query.pop();
                p.cursor = 0;
                Action::Redraw
            }
            KeyCode::Char(c) => {
                p.query.push(c);
                p.cursor = 0;
                Action::Redraw
            }
            KeyCode::Enter => {
                let (query, cursor) = (p.query.clone(), p.cursor);
                let picked = self.commands(&query).get(cursor).map(|c| c.key);
                self.mode = Mode::Normal;
                match picked {
                    Some(key) => self.on_normal(key),
                    None => Action::Redraw,
                }
            }
            _ => Action::None,
        }
    }

    /// The wheel moves the selection, or scrolls what is open; a click picks a view, a row
    /// or a tile, and a click on the row already picked opens it as enter would.
    pub fn on_mouse(&mut self, m: MouseEvent) -> Action {
        let at = Position::new(m.column, m.row);
        let by = match m.kind {
            MouseEventKind::ScrollDown => 1,
            MouseEventKind::ScrollUp => -1,
            MouseEventKind::Down(MouseButton::Left) => 0,
            _ => return Action::None,
        };
        match (&self.mode, by) {
            (Mode::Detail(_), 0) => return Action::None,
            (Mode::Detail(_), by) => return self.scroll_detail(by * 3),
            (Mode::Sheet(_), 0) => return Action::None,
            (Mode::Sheet(_), by) => return self.scroll_sheet(by),
            (Mode::Palette(_), by) if by != 0 => return self.palette_step(by),
            (Mode::Normal | Mode::Filter, by) if by != 0 => {
                return if self.disk_map_on() {
                    Action::None
                } else {
                    self.step(by)
                };
            }
            (Mode::Normal | Mode::Filter, _) => {}
            _ => return Action::None,
        }
        let hits = self.hits.borrow().clone();
        if let Some((tab, _)) = hits.tabs.iter().find(|(_, r)| r.contains(at)) {
            self.mode = Mode::Normal;
            return self.go(*tab);
        }
        let picked = hits
            .rows
            .iter()
            .chain(&hits.tiles)
            .find(|(_, r)| r.contains(at))
            .map(|(id, _)| id.clone());
        let Some(id) = picked else {
            return Action::None;
        };
        if self.mode == Mode::Filter {
            self.mode = Mode::Normal;
        }
        if self.selected_row().is_some_and(|r| r.id == id) {
            return self.on_normal(KeyCode::Enter);
        }
        self.state_mut().selected = Some(id);
        Action::Redraw
    }

    /// Moves to the tile next to the selected one on screen.
    fn move_tile(&mut self, dir: Dir) -> Action {
        let Some(row) = self.selected_row() else {
            return Action::None;
        };
        let next = treemap::neighbour(&self.hits.borrow().tiles, &row.id, dir);
        match next {
            Some(id) => {
                self.state_mut().selected = Some(id);
                Action::Redraw
            }
            None => Action::None,
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
            KeyCode::Char(':') => self.open_palette(),
            KeyCode::Char('v') if self.tab == Tab::Disk => {
                self.disk_map = !self.disk_map;
                Action::Redraw
            }
            KeyCode::Char('h') | KeyCode::Left if self.disk_map_on() => self.move_tile(Dir::Left),
            KeyCode::Char('l') | KeyCode::Right if self.disk_map_on() => self.move_tile(Dir::Right),
            KeyCode::Char('j') | KeyCode::Down if self.disk_map_on() => self.move_tile(Dir::Down),
            KeyCode::Char('k') | KeyCode::Up if self.disk_map_on() => self.move_tile(Dir::Up),
            KeyCode::Char(c @ '1'..='6') => self.go(Tab::ALL[(c as u8 - b'1') as usize]),
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
            KeyCode::Char(' ') => self.toggle_fold(),
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
            KeyCode::Char('K') => self.ask_keep(),
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
                self.confirm(sheet)
            }
            KeyCode::Esc | KeyCode::Char('n' | 'q') => {
                if let Mode::Sheet(s) = &self.mode
                    && s.kind == SheetKind::Kill
                {
                    // Only what this sheet offered; a later stop's leftovers stay offered.
                    if self.stubborn.as_ref() == Some(&(s.origin, s.targets.clone())) {
                        self.stubborn = None;
                    }
                    self.notify("SIGKILL was not sent.", Tone::Quiet);
                }
                self.enter(Mode::Normal)
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll_sheet(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_sheet(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_sheet(PAGE),
            KeyCode::PageUp => self.scroll_sheet(-PAGE),
            KeyCode::Char('g') | KeyCode::Home => self.scroll_sheet(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.scroll_sheet(isize::MAX / 2),
            _ => Action::None,
        }
    }

    /// Carries out a confirmed sheet: writes the keep list at once, and hands a signal to
    /// the live TUI's worker with the sheet's own targets and origin, which the worker
    /// checks again in full before each signal. A fixture sends and writes nothing.
    fn confirm(&mut self, sheet: Sheet) -> Action {
        if sheet.kind == SheetKind::Keep {
            let Some((edit, _)) = sheet.keep.map(|b| *b) else {
                return Action::Redraw;
            };
            let Some(paths) = self.paths.clone() else {
                self.notify("This is a fixture, so nothing was written.", Tone::Info);
                return Action::Redraw;
            };
            return match keep::apply_audited(&edit, &paths, "tui") {
                Ok(()) => {
                    let file = self.config_name(&edit);
                    self.notify_for(
                        format!("Kept `{}`: added to {file} line {}.", edit.entry, edit.line),
                        Tone::Ok,
                        TOAST_FOR * 2,
                    );
                    Action::Reread
                }
                Err(e) => {
                    self.notify(format!("Nothing was written: {e:#}"), Tone::Danger);
                    Action::Redraw
                }
            };
        }
        if !matches!(self.source, Source::Live) {
            self.notify("This is a fixture, so nothing was sent.", Tone::Info);
            return Action::Redraw;
        }
        if let Some(kind) = self.acting {
            self.notify(format!("Still {}…", doing(kind)), Tone::Quiet);
            return Action::Redraw;
        }
        let origin = sheet.origin;
        if sheet.kind == SheetKind::Kill
            && self.stubborn.as_ref() == Some(&(origin, sheet.targets.clone()))
        {
            self.stubborn = None;
        }
        let n = sheet.targets.len();
        self.acting = Some(sheet.kind);
        // Shown until the outcome replaces it.
        self.notify_for(
            format!(
                "{} {n} {}…",
                capital(doing(sheet.kind)),
                if n == 1 { "process" } else { "processes" }
            ),
            Tone::Info,
            Duration::from_secs(3600),
        );
        Action::Act(Request {
            kind: sheet.kind,
            origin,
            targets: sheet.targets,
        })
    }

    /// Takes in what a confirmed action did: the outcome in the footer, and SIGKILL offered
    /// on a sheet of its own for whatever outlived SIGTERM.
    pub fn acted(&mut self, req: &Request, result: Result<Report, String>) {
        self.acting = None;
        let report = match result {
            Ok(r) => r,
            Err(why) => {
                self.notify_for(why, Tone::Danger, TOAST_FOR * 2);
                return;
            }
        };
        let still: Vec<Target> = report
            .still_running()
            .iter()
            .filter_map(|d| req.targets.iter().find(|t| t.pid == d.target.id.pid))
            .cloned()
            .collect();
        let tone = if !still.is_empty() {
            Tone::Warn
        } else if report.stopped() == report.done.len() {
            Tone::Ok
        } else {
            Tone::Quiet
        };
        let mut text = report.summary();
        if let Some(u) = report.unaudited().first() {
            text = format!(
                "{text} The outcome for pid {} was not audited.",
                u.target.id.pid
            );
            self.notify_for(text, Tone::Danger, TOAST_FOR * 2);
        } else {
            if let Some(r) = report.done.iter().find_map(|d| d.refusal.as_ref()) {
                text = format!("{}: {}", text.trim_end_matches('.'), r.brief());
            }
            self.notify_for(text, tone, TOAST_FOR * 2);
        }
        // An open sheet is never touched: it keeps its own targets and origin.
        if still.is_empty() || req.kind == SheetKind::Kill {
            // What another action left still waits for its SIGKILL.
            let about_these = |(_, offered): &(SheetKind, Vec<Target>)| {
                offered
                    .iter()
                    .any(|o| req.targets.iter().any(|t| t.pid == o.pid))
            };
            if self.stubborn.as_ref().is_some_and(about_these) {
                self.stubborn = None;
            }
            return;
        }
        self.stubborn = Some((req.origin, still.clone()));
        if self.mode == Mode::Normal {
            self.mode = Mode::Sheet(Sheet::kill(req.origin, &still));
        }
    }

    /// The config's path as the sheet and the footer show it; a fixture's is only a name.
    pub fn config_name(&self, edit: &KeepEdit) -> String {
        self.paths
            .as_ref()
            .map_or_else(|| "config.toml".into(), |p| p.tilde(&edit.config))
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
                let CleanSheet { plan, typed, .. } = *sheet;
                Action::Clean(Box::new((plan, word.map(|_| typed))))
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
                    warning: (self.writer)(&plan.item),
                };
                return self.enter(Mode::Clean(Box::new(sheet)));
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
        let mut max = sheet.max_offset.get();
        if sheet.keep.is_none() {
            max = max.min(sheet.targets.len().saturating_sub(1));
        }
        let at = sheet.offset.get().min(max) as isize;
        sheet
            .offset
            .set(at.saturating_add(by).clamp(0, max as isize) as usize);
        Action::Redraw
    }

    fn enter(&mut self, mode: Mode) -> Action {
        // A sheet asks for a decision; an older word in the footer would read as its answer.
        if matches!(mode, Mode::Sheet(_) | Mode::Clean(_)) {
            self.toast = None;
        }
        self.mode = mode;
        Action::Redraw
    }

    fn go(&mut self, tab: Tab) -> Action {
        self.tab = tab;
        if matches!(self.mode, Mode::Detail(_)) {
            self.mode = Mode::Normal;
        }
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

    /// Enter jumps to the row an Overview line stands for, or opens the selected row's
    /// detail over the whole body.
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
        if row.detail.is_none() {
            return Action::None;
        }
        self.enter(Mode::Detail(Cell::new(0)))
    }

    /// Space folds and unfolds a parent. On a row without children it does nothing.
    fn toggle_fold(&mut self) -> Action {
        let Some(row) = self.selected_row() else {
            return Action::None;
        };
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
        let folded = self.state().folded.contains(&row.id) != views::view(self.tab).fold_all;
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
        // A view that starts folded keeps the rows opened instead.
        let fold = fold != views::view(self.tab).fold_all;
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

    /// `K`: shows how the keep list would change to spare the selected process.
    fn ask_keep(&mut self) -> Action {
        let Some(t) = self.selected_row().and_then(|r| r.target) else {
            self.notify("Nothing on this row can be kept.", Tone::Quiet);
            return Action::Redraw;
        };
        let entry = match &t.keep {
            Some(KeepAs::Entry(e)) => e.clone(),
            Some(KeepAs::Refused(_)) => {
                let what = super::kit::fit(vec![t.command.as_str().into()], 24);
                self.notify(
                    format!("No entry keeps only `{what}`. Add one to keep by hand."),
                    Tone::Quiet,
                );
                return Action::Redraw;
            }
            None => {
                self.notify(
                    "Only a process an agent session started can go on the keep list.",
                    Tone::Quiet,
                );
                return Action::Redraw;
            }
        };
        // A fixture plans against an empty config of its own and never writes it.
        let edit = match &self.paths {
            Some(paths) => keep::plan(paths, &entry),
            None => keep::plan(&fixture_paths(), &entry),
        };
        match edit {
            Ok(edit) if edit.already => {
                let file = self.config_name(&edit);
                self.notify(
                    format!("`{entry}` is already on the keep list in {file}."),
                    Tone::Quiet,
                );
                Action::Redraw
            }
            Ok(edit) => {
                let mut sheet = Sheet::new(SheetKind::Keep, vec![t]);
                let file = self.config_name(&edit);
                sheet.keep = Some(Box::new((edit, file)));
                self.enter(Mode::Sheet(sheet))
            }
            Err(e) => {
                self.notify(
                    format!("The keep list cannot be changed: {e:#}"),
                    Tone::Danger,
                );
                Action::Redraw
            }
        }
    }

    fn ask_reap(&mut self) -> Action {
        let Some(reap) = views::view(self.tab).reap else {
            return Action::None;
        };
        if let Some((origin, still)) = &self.stubborn {
            let sheet = Sheet::kill(*origin, still);
            return self.enter(Mode::Sheet(sheet));
        }
        // Nothing to reap is an all-clear only once the agent sessions were read, which the
        // Overview needs as much as the Agents view.
        match (views::view(Tab::Agents).rows)(&self.board) {
            Load::Ready(_) => {}
            Load::Unavailable(why) | Load::Failed(why) => {
                self.notify(why, Tone::Quiet);
                return Action::Redraw;
            }
            Load::Loading => {
                self.notify("Still reading agent sessions.", Tone::Quiet);
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

/// Paths whose config does not exist, for a fixture's `K` to plan against.
fn fixture_paths() -> Paths {
    let home = std::env::temp_dir().join("scry-fixture");
    Paths::new(home.clone(), Some(home.join("no-config-here")))
}

/// The words for an action in progress.
fn doing(kind: SheetKind) -> &'static str {
    match kind {
        SheetKind::Stop => "stopping",
        SheetKind::Reap => "reaping",
        SheetKind::Kill => "sending SIGKILL to",
        SheetKind::Keep => "keeping",
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn has_children(rows: &[Row], i: usize) -> bool {
    rows.get(i + 1).is_some_and(|n| n.depth > rows[i].depth)
}
