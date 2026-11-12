//! The terminal UI: a model, update, view loop. `app` holds the state and handles keys,
//! `ui` lays out a frame from the `kit` components, and each view lives in `views`.

pub mod app;
mod appearance;
mod filter;
mod kit;
mod motion;
pub mod theme;
mod tty;
pub mod ui;
mod views;

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, Event, KeyEvent};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{DefaultTerminal, Terminal};
use scry_core::config::Config;
use scry_core::engine::Labels;
use scry_core::live::Collector;
use scry_core::paths::Paths;
use scry_core::view::{Board, Load, Tab, Tone, WorktreeList};
use scry_core::worktree::{self, ScanOptions};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use app::{Action, App, Mode, Source};
use motion::Motion;
use theme::{Colors, Env, SHIPPED, Theme};
use tty::LateReply;

/// How long "Refreshed." stays in the footer after `r`.
const REFRESHED_FOR: Duration = Duration::from_millis(1500);
const INPUT_POLL: Duration = Duration::from_millis(250);
const FRAME: Duration = Duration::from_millis(16);
const SIGNAL_GRACE: Duration = Duration::from_secs(1);

/// The signal that asked Scry to stop, or 0.
static SIGNAL: AtomicI32 = AtomicI32::new(0);
/// What `enter` changed and `leave` has not put back yet.
static RAW: AtomicBool = AtomicBool::new(false);
static ALTERNATE: AtomicBool = AtomicBool::new(false);

pub fn run(fixture: Option<PathBuf>) -> Result<()> {
    let source = fixture.map_or(Source::Live, Source::Fixture);
    // Live data starts loading once the screen is up; a fixture is read before.
    let (board, live) = match &source {
        Source::Live => (Board::default(), Some(Live::read()?)),
        Source::Fixture(_) => (source.load()?, None),
    };

    tty::save();
    watch_signals()?;
    leave_on_panic();
    // Declared before the terminal, so it is dropped after it.
    let _restore = Restore;
    enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    // Asked in raw mode and before the event loop reads a key, so the answer is read here
    // and the question needs no terminal modes of its own.
    let mut answer = tty::Answer::default();
    let colors = Colors::detect(
        &Env::read(),
        || {
            answer = tty::ask(&SIGNAL);
            answer.colors
        },
        appearance::dark,
    );
    exit_on_signal();
    let theme = Theme::new(SHIPPED, colors);
    let mut motion = Motion::new(theme.motion);
    let mut app = App::new(board, source, theme);

    let (tx, rx) = mpsc::channel();
    let workers = live.map(|l| l.spawn(tx));

    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut motion,
        &rx,
        workers.as_ref(),
        answer.typed,
        answer.late,
    );
    exit_on_signal();
    result
}

/// What the live TUI reads from the config before the screen opens, so a broken file is
/// reported on the normal screen.
struct Live {
    /// `None` refreshes only on `r`.
    every: Option<Duration>,
    worktrees: ScanOptions,
    config: PathBuf,
}

impl Live {
    fn read() -> Result<Live> {
        let paths = Paths::from_env()?;
        let (config, _) = Config::load(&paths)?;
        let secs = config.tui.refresh_secs;
        Ok(Live {
            every: (secs > 0).then(|| Duration::from_secs(secs)),
            worktrees: ScanOptions::new(&paths.user_home, config.code_roots),
            config: paths.config,
        })
    }

    /// Starts the two readers off the UI thread: process snapshots, quick and on a timer,
    /// and worktrees, slow and only when asked.
    fn spawn(self, tx: mpsc::Sender<Update>) -> Workers {
        let (now, asks) = mpsc::channel();
        let every = self.every;
        let to = tx.clone();
        std::thread::spawn(move || read_processes(&to, &asks, every));
        let (worktrees, asks) = mpsc::channel();
        std::thread::spawn(move || read_worktrees(&tx, &asks, &self.worktrees, &self.config));
        Workers {
            now,
            worktrees,
            every,
        }
    }
}

/// What the readers send the UI thread.
enum Update {
    Started,
    Board(Box<Board>),
    Failed(String),
    Worktrees(Load<WorktreeList>),
}

/// Where the UI thread asks the readers for a reading.
struct Workers {
    now: mpsc::Sender<()>,
    worktrees: mpsc::Sender<()>,
    every: Option<Duration>,
}

impl Workers {
    /// Coming back to the worktree view reads it again when its last reading is this old.
    /// Sizing walked 1.6 million entries in 15 s and 38 s of CPU across cores on the
    /// reference Mac, so it never runs on a timer while the view sits open; `r` reads at once.
    const WORKTREES_STALE: Duration = Duration::from_secs(600);

    /// `opened` says the view was just switched to.
    fn worktrees_due(&self, app: &App, opened: bool, now: Instant) -> bool {
        app.tab == Tab::Worktrees
            && !app.fresh.worktrees
            && match app.fresh.worktrees_at {
                None => true,
                Some(t) => {
                    opened && self.every.is_some() && now.duration_since(t) >= Self::WORKTREES_STALE
                }
            }
    }
}

/// Reads at once, then after each interval or when asked. Asks that arrive during a
/// reading are answered by the next one.
fn read_processes(tx: &mpsc::Sender<Update>, asks: &mpsc::Receiver<()>, every: Option<Duration>) {
    let mut collector = Collector::new(Labels::All);
    loop {
        if tx.send(Update::Started).is_err() {
            return;
        }
        let update = match collector.board() {
            Ok(board) => Update::Board(Box::new(board)),
            Err(e) => Update::Failed(format!("{e:#}")),
        };
        if tx.send(update).is_err() {
            return;
        }
        let asked = match every {
            Some(d) => asks
                .recv_timeout(d)
                .map_err(|e| e == RecvTimeoutError::Disconnected),
            None => asks.recv().map_err(|_| true),
        };
        if asked == Err(true) {
            return;
        }
        while asks.try_recv().is_ok() {}
    }
}

/// Git state first, then sizes, which take seconds: the list shows before it is measured.
fn read_worktrees(
    tx: &mpsc::Sender<Update>,
    asks: &mpsc::Receiver<()>,
    opts: &ScanOptions,
    config: &Path,
) {
    while asks.recv().is_ok() {
        while asks.try_recv().is_ok() {}
        let list = |scan: &worktree::WorktreeScan, sizing| {
            Update::Worktrees(Load::Ready(WorktreeList::new(
                scan,
                unix_now(),
                &opts.home,
                sizing,
                config,
            )))
        };
        let mut scan = worktree::scan(opts);
        if tx.send(list(&scan, true)).is_err() {
            return;
        }
        worktree::measure(&mut scan, &opts.home);
        if tx.send(list(&scan, false)).is_err() {
            return;
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Raw mode, then the alternate screen. Each step is recorded once it worked, so when a
/// later one fails `leave` still undoes the earlier.
fn enter() -> io::Result<()> {
    tty::change(enable_raw_mode)?;
    RAW.store(true, Ordering::Relaxed);
    execute!(stdout(), EnterAlternateScreen)?;
    ALTERNATE.store(true, Ordering::Relaxed);
    Ok(())
}

/// Undoes what `enter` did, each step once, and shows the cursor. Both steps are tried
/// whatever the other did; the first error is returned.
fn leave() -> io::Result<()> {
    let mut result = Ok(());
    if RAW.swap(false, Ordering::Relaxed) {
        result = disable_raw_mode();
    }
    if ALTERNATE.swap(false, Ordering::Relaxed) {
        result = result.and(execute!(stdout(), LeaveAlternateScreen, Show));
    }
    result
}

/// Puts the terminal back on every way out of `run`, setup errors included.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        if let Err(e) = leave() {
            eprintln!("Failed to restore the terminal: {e}");
        }
    }
}

/// Puts the terminal back before a panic message is printed, so it lands on the normal
/// screen in cooked mode.
fn leave_on_panic() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = leave();
        hook(info);
    }));
}

/// After a signal, puts the terminal back and exits with 128 plus its number, as shells
/// report a process the signal ended.
fn exit_on_signal() {
    let sig = SIGNAL.load(Ordering::Relaxed);
    if sig != 0 {
        tty::restore_and_exit(128 + sig);
    }
}

/// Draws only when something changed, or while an effect runs. `typed` are keys read with
/// the colour answer; `late` picks the rest of an unfinished answer out of the keys.
fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    motion: &mut Motion,
    rx: &mpsc::Receiver<Update>,
    workers: Option<&Workers>,
    typed: Vec<KeyEvent>,
    mut late: Option<LateReply>,
) -> Result<()> {
    let mut dirty = true;
    let mut keys = VecDeque::from(typed);
    // What the header said when last drawn; a new word is a reason to draw.
    let mut said = None;
    while SIGNAL.load(Ordering::Relaxed) == 0 {
        for update in rx.try_iter() {
            dirty |= receive(app, motion, update);
        }
        let now = Instant::now();
        if app.toast.as_ref().is_some_and(|t| now >= t.until) {
            app.toast = None;
            dirty = true;
        }
        if let Some(held) = late.as_mut().and_then(|l| l.expire(now)) {
            keys.extend(held);
            late = None;
        }
        let tab = app.tab;
        while let Some(key) = keys.pop_front() {
            let had_sheet = matches!(app.mode, Mode::Sheet(_));
            match app.on_key(key) {
                Action::Quit => return Ok(()),
                Action::Suspend => suspend(terminal)?,
                Action::Rescan => match workers {
                    Some(w) => {
                        app.fresh.asked = true;
                        let _ = w.now.send(());
                        if app.tab == Tab::Worktrees && !app.fresh.worktrees {
                            app.fresh.worktrees = true;
                            let _ = w.worktrees.send(());
                        }
                    }
                    None => match app.source.load() {
                        Ok(board) => {
                            motion.rows_changed(app.replace_board(board));
                            app.notify_for("Refreshed.", Tone::Ok, REFRESHED_FOR);
                        }
                        Err(e) => app.notify(format!("Refresh failed: {e:#}"), Tone::Danger),
                    },
                },
                Action::Redraw => {}
                Action::None => continue,
            }
            dirty = true;
            if !had_sheet && matches!(app.mode, Mode::Sheet(_)) {
                motion.sheet_opened();
            }
        }
        if let Some(w) = workers
            && w.worktrees_due(app, app.tab != tab, now)
        {
            app.fresh.worktrees = true;
            let _ = w.worktrees.send(());
        }
        let says = app.freshness(now);
        if says != said {
            dirty = true;
        }
        if dirty || motion.running() {
            said = says;
            terminal.draw(|f| {
                let painted = ui::draw(f, app);
                motion.apply(f.buffer_mut(), &painted);
            })?;
            dirty = false;
        }
        let wait = if motion.running() {
            FRAME
        } else {
            let toast = app.toast.as_ref().map(|t| t.until);
            [toast, app.freshness_due()]
                .into_iter()
                .flatten()
                .map(|at| at.saturating_duration_since(now))
                .fold(INPUT_POLL, Duration::min)
        };
        if !event::poll(wait)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => match late.as_mut() {
                Some(l) => keys.extend(l.feed(key, Instant::now())),
                None => keys.push_back(key),
            },
            Event::Resize(..) => dirty = true,
            _ => {}
        }
    }
    Ok(())
}

/// Takes in what a reader sent, and says whether the screen changed beyond the header's
/// word on freshness, which the loop compares on its own. A process board keeps the
/// worktrees the app already has, since those come from their own reader.
fn receive(app: &mut App, motion: &mut Motion, update: Update) -> bool {
    let now = Instant::now();
    match update {
        Update::Started => {
            app.fresh.scanning = Some(now);
            false
        }
        Update::Board(mut board) => {
            board.worktrees = app.board.worktrees.clone();
            let changed = *board != app.board;
            if changed {
                motion.rows_changed(app.replace_board(*board));
            }
            finish_scan(app, now, None) || changed
        }
        Update::Failed(why) => {
            if app.board.ports == Load::Loading {
                app.board.ports = Load::Failed(why.clone());
            }
            finish_scan(app, now, Some(why))
        }
        Update::Worktrees(list) => {
            if list.ready().is_none_or(|l| !l.sizing) {
                app.fresh.worktrees = false;
                app.fresh.worktrees_at = Some(now);
            }
            let mut board = app.board.clone();
            board.worktrees = list;
            motion.rows_changed(app.replace_board(board));
            true
        }
    }
}

/// Ends a process scan; a toast follows one that `r` asked for, or one that failed.
fn finish_scan(app: &mut App, now: Instant, failed: Option<String>) -> bool {
    let f = &mut app.fresh;
    f.scanning = None;
    let asked = std::mem::take(&mut f.asked);
    match failed {
        None => {
            f.updated = Some(now);
            if asked {
                app.notify_for("Refreshed.", Tone::Ok, REFRESHED_FOR);
            }
            asked
        }
        Some(why) => {
            app.notify(format!("Refresh failed: {why}"), Tone::Danger);
            true
        }
    }
}

/// SIGTERM and SIGINT ask the loop to stop and restore the terminal, and restore it here
/// after a grace period if the loop has not. A hung-up terminal can leave crossterm spinning
/// inside `poll`, where the loop never sees the flag, so SIGHUP restores and exits at once.
fn watch_signals() -> Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])?;
    std::thread::spawn(move || {
        let Some(sig) = signals.forever().next() else {
            return;
        };
        SIGNAL.store(sig, Ordering::Relaxed);
        if sig != SIGHUP {
            std::thread::sleep(SIGNAL_GRACE);
        }
        tty::restore_and_exit(128 + sig);
    });
    Ok(())
}

/// Ctrl-Z: give the shell its terminal back, stop, and take it again on `fg`.
fn suspend(terminal: &mut DefaultTerminal) -> Result<()> {
    leave()?;
    // SAFETY: raising a signal at the calling thread has no memory effects.
    unsafe { libc::raise(libc::SIGTSTP) };
    enter()?;
    terminal.clear()?;
    Ok(())
}
