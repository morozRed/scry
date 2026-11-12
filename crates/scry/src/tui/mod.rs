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
use scry_core::disk::cache::SizeCache;
use scry_core::disk::clean::{self, Context, Done, Plan, Refusal};
use scry_core::disk::{self, Layout, catalog};
use scry_core::engine::Labels;
use scry_core::live::Collector;
use scry_core::paths::Paths;
use scry_core::store::Store;
use scry_core::view::{Board, DiskList, Load, Tab, Tone, WorktreeList};
use scry_core::worktree::{self, ScanOptions, WorktreeScan};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use app::{Action, App, Mode, Source};
use motion::Motion;
use theme::{Colors, Env, SHIPPED, Theme};
use tty::LateReply;

/// How long "Refreshed." stays in the footer after `r`.
const REFRESHED_FOR: Duration = Duration::from_millis(1500);
/// "Cleaning up…" stays until the outcome replaces it; a worktree's label check runs git
/// and walks the folder.
const CLEANING_FOR: Duration = Duration::from_secs(120);
/// How long a cleanup's outcome stays, long enough to read the Trash's size.
const DONE_FOR: Duration = Duration::from_secs(10);
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
    storage: Storage,
}

/// What the worktree and disk reader needs.
struct Storage {
    worktrees: ScanOptions,
    paths: Paths,
    destructive: bool,
}

impl Live {
    fn read() -> Result<Live> {
        let paths = Paths::from_env()?;
        let (config, _) = Config::load(&paths)?;
        let secs = config.tui.refresh_secs;
        Ok(Live {
            every: (secs > 0).then(|| Duration::from_secs(secs)),
            storage: Storage {
                worktrees: ScanOptions::new(&paths.user_home, config.code_roots),
                paths,
                destructive: config.destructive,
            },
        })
    }

    /// Starts the readers off the UI thread: process snapshots, quick and on a timer;
    /// worktrees and the disk, slower and only when asked; and cleanups, one at a time.
    fn spawn(self, tx: mpsc::Sender<Update>) -> Workers {
        let (now, asks) = mpsc::channel();
        let every = self.every;
        let to = tx.clone();
        std::thread::spawn(move || read_processes(&to, &asks, every));
        let (worktrees, asks) = mpsc::channel();
        let to = tx.clone();
        let storage = self.storage;
        let home = storage.paths.user_home.clone();
        let paths = storage.paths.clone();
        let destructive = storage.destructive;
        std::thread::spawn(move || read_storage(&to, &asks, &storage));
        let (clean, jobs) = mpsc::channel();
        let reread = worktrees.clone();
        std::thread::spawn(move || {
            run_cleanups(&tx, &jobs, &reread, &home, &paths, destructive);
        });
        Workers {
            now,
            worktrees,
            clean,
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
    Disk(Box<Load<DiskList>>),
    /// The disk reading is done, sizes included.
    StorageDone,
    Cleaned(Result<Done, Refusal>),
}

/// Where the UI thread asks the readers for a reading.
struct Workers {
    now: mpsc::Sender<()>,
    /// Worktrees and the disk, read together.
    worktrees: mpsc::Sender<()>,
    clean: mpsc::Sender<(Plan, Option<String>)>,
    every: Option<Duration>,
}

impl Workers {
    /// Coming back to Disk or Worktrees reads them again when the last reading is this old.
    /// Sizes come from the cache and only what changed is walked, but a cache that churns
    /// all the time (a browser's) still costs a walk, so it never runs on a timer while the
    /// view sits open; `r` reads at once.
    const WORKTREES_STALE: Duration = Duration::from_secs(120);

    /// `opened` says the view was just switched to.
    fn worktrees_due(&self, app: &App, opened: bool, now: Instant) -> bool {
        matches!(app.tab, Tab::Worktrees | Tab::Disk)
            && app.fresh.worktrees.is_none()
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

/// Worktrees and the disk, from the size cache first: the catalog's items at once, the
/// worktrees once git has read them, then the sizes that changed, walked again.
fn read_storage(tx: &mpsc::Sender<Update>, asks: &mpsc::Receiver<()>, s: &Storage) {
    let cache = SizeCache::open(&s.paths);
    let home = &s.worktrees.home;
    while asks.recv().is_ok() {
        while asks.try_recv().is_ok() {}
        let now = unix_now();
        let disk = |items: &[disk::Item], pending| {
            let list = disk::view(items, &cache, &s.paths, s.destructive, pending);
            Update::Disk(Box::new(Load::Ready(list)))
        };
        let worktrees = |scan: &WorktreeScan, sizing| {
            let list = WorktreeList::new(scan, now, home, sizing, &s.paths.config);
            Update::Worktrees(Load::Ready(list))
        };
        let split = cache.split();
        let found = catalog::locate(home, &s.worktrees.roots, &[]);
        let mut layout = Layout::new(&found, None, home, now, &split);
        let look = cache.look(&layout.parts, home);
        layout.fill(&look.cached, &look.changed());
        if tx.send(disk(&layout.items, true)).is_err() {
            return;
        }

        let mut scan = worktree::scan(&s.worktrees);
        let mut layout = Layout::new(&found, Some(&scan), home, now, &split);
        let look = cache.look(&layout.parts, home);
        layout.fill(&look.cached, &look.changed());
        let cached = layout.worktree_sizes(&scan);
        worktree::apply_sizes(&mut scan, cached);
        layout.relabel(&scan, home, now);
        let sizing = look.stale.iter().any(|&st| st);
        if tx.send(worktrees(&scan, sizing)).is_err()
            || tx.send(disk(&layout.items, false)).is_err()
        {
            return;
        }

        if sizing {
            let (sizes, _) = cache.measure(&layout.parts, &look, home, &layout.exclude);
            let sizes: Vec<_> = sizes.into_iter().map(Some).collect();
            layout.fill(&sizes, &look.held);
            let measured = layout.worktree_sizes(&scan);
            worktree::apply_sizes(&mut scan, measured);
            layout.relabel(&scan, home, now);
            if tx.send(worktrees(&scan, false)).is_err()
                || tx.send(disk(&layout.items, false)).is_err()
            {
                return;
            }
        }
        if tx.send(Update::StorageDone).is_err() {
            return;
        }
    }
}

/// Runs each confirmed cleanup, which checks its target again first, then asks for a new
/// reading so the lists show what is left.
fn run_cleanups(
    tx: &mpsc::Sender<Update>,
    jobs: &mpsc::Receiver<(Plan, Option<String>)>,
    reread: &mpsc::Sender<()>,
    home: &Path,
    paths: &Paths,
    destructive: bool,
) {
    let store = paths
        .ensure_state()
        .and_then(|()| Store::open(&paths.db()))
        .ok();
    while let Ok((plan, typed)) = jobs.recv() {
        let ctx = Context {
            home,
            store: store.as_ref(),
            actor: "tui",
            destructive,
            label: &clean::relabel,
        };
        let result = clean::execute(&plan, typed.as_deref(), &ctx);
        if tx.send(Update::Cleaned(result)).is_err() {
            return;
        }
        let _ = reread.send(());
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
            let had_sheet = matches!(app.mode, Mode::Sheet(_) | Mode::Clean(_));
            match app.on_key(key) {
                Action::Quit => return Ok(()),
                Action::Suspend => suspend(terminal)?,
                Action::Rescan => match workers {
                    Some(w) => {
                        app.fresh.asked = true;
                        let _ = w.now.send(());
                        if matches!(app.tab, Tab::Worktrees | Tab::Disk)
                            && app.fresh.worktrees.is_none()
                        {
                            app.fresh.worktrees = Some(now);
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
                Action::Clean(job) => match workers {
                    Some(w) => {
                        let _ = w.clean.send(*job);
                        app.notify_for("Cleaning up…", Tone::Quiet, CLEANING_FOR);
                    }
                    None => app.notify(
                        "Nothing was touched: this is a recorded fixture.",
                        Tone::Info,
                    ),
                },
                Action::Redraw => {}
                Action::None => continue,
            }
            dirty = true;
            if !had_sheet && matches!(app.mode, Mode::Sheet(_) | Mode::Clean(_)) {
                motion.sheet_opened();
            }
        }
        if let Some(w) = workers
            && w.worktrees_due(app, app.tab != tab, now)
        {
            app.fresh.worktrees = Some(now);
            let _ = w.worktrees.send(());
        }
        let says = app.activity(now);
        if Some(&says) != said.as_ref() {
            dirty = true;
        }
        if dirty || motion.running() {
            said = Some(says);
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
            [toast, app.activity_due(now)]
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
/// spinner, which the loop compares on its own. A board that changes nothing on screen,
/// such as ages that still round to the same text, draws nothing. A process board keeps
/// the worktrees the app already has, since those come from their own reader.
fn receive(app: &mut App, motion: &mut Motion, update: Update) -> bool {
    let now = Instant::now();
    match update {
        Update::Started => {
            app.fresh.scanning = Some(now);
            false
        }
        Update::Board(mut board) => {
            board.worktrees = app.board.worktrees.clone();
            board.disk = app.board.disk.clone();
            let changed = !app.looks_the_same(&board);
            let glow = app.replace_board(*board);
            if changed {
                motion.rows_changed(glow);
            }
            finish_scan(app, None) || changed
        }
        Update::Failed(why) => {
            if app.board.ports == Load::Loading {
                app.board.ports = Load::Failed(why.clone());
            }
            finish_scan(app, Some(why))
        }
        Update::Worktrees(list) => {
            let mut board = app.board.clone();
            board.worktrees = list;
            motion.rows_changed(app.replace_board(board));
            true
        }
        Update::Disk(list) => {
            let mut board = app.board.clone();
            board.disk = *list;
            motion.rows_changed(app.replace_board(board));
            true
        }
        Update::StorageDone => {
            app.fresh.worktrees = None;
            app.fresh.worktrees_at = Some(now);
            true
        }
        Update::Cleaned(Ok(done)) => {
            app.notify_for(done.short(), Tone::Ok, DONE_FOR);
            app.fresh.worktrees = Some(now);
            true
        }
        Update::Cleaned(Err(refusal)) => {
            app.notify_for(refusal.to_string(), Tone::Warn, DONE_FOR);
            true
        }
    }
}

/// Ends a process scan; a toast follows one that `r` asked for, or one that failed.
fn finish_scan(app: &mut App, failed: Option<String>) -> bool {
    let f = &mut app.fresh;
    f.scanning = None;
    let asked = std::mem::take(&mut f.asked);
    match failed {
        None => {
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
