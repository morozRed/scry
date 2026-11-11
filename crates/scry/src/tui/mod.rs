//! The terminal UI: a model, update, view loop. `app` holds the state and handles keys,
//! `ui` lays out a frame from the `kit` components, and each view lives in `views`.

pub mod app;
mod filter;
mod kit;
mod motion;
pub mod theme;
pub mod ui;
mod views;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use scry_core::view::{Board, Tone};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use app::{Action, App, Mode, Source};
use motion::Motion;
use theme::Theme;

const SAMPLE_EVERY: Duration = Duration::from_secs(2);
const INPUT_POLL: Duration = Duration::from_millis(250);
const FRAME: Duration = Duration::from_millis(16);
const SIGNAL_GRACE: Duration = Duration::from_secs(1);

pub fn run(fixture: Option<PathBuf>) -> Result<()> {
    let source = fixture.map_or(Source::Live, Source::Fixture);
    let live = matches!(source, Source::Live);
    let board = source.load()?;
    // Asks the terminal for its colours, so it runs before raw mode and the alternate screen.
    let theme = Theme::detect();
    let mut motion = Motion::new(theme.motion);
    let mut app = App::new(board, source, theme);

    let stop = Arc::new(AtomicBool::new(false));
    watch_signals(Arc::clone(&stop))?;

    let (tx, rx) = mpsc::channel();
    if live {
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(SAMPLE_EVERY);
                let Ok(snap) = scry_core::sys::scan() else {
                    continue;
                };
                if tx.send(Board::from_snapshot(&snap)).is_err() {
                    break;
                }
            }
        });
    }

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &mut motion, &rx, &stop);
    ratatui::restore();
    result
}

/// Draws only when something changed, or while an effect runs.
fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    motion: &mut Motion,
    rx: &mpsc::Receiver<Board>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut dirty = true;
    while !stop.load(Ordering::Relaxed) {
        if let Some(board) = rx.try_iter().last()
            && board != app.board
        {
            motion.rows_changed(app.replace_board(board));
            dirty = true;
        }
        let now = Instant::now();
        if app.toast.as_ref().is_some_and(|t| now >= t.until) {
            app.toast = None;
            dirty = true;
        }
        if dirty || motion.running() {
            terminal.draw(|f| {
                let painted = ui::draw(f, app);
                motion.apply(f.buffer_mut(), &painted);
            })?;
            dirty = false;
        }
        let wait = if motion.running() {
            FRAME
        } else {
            app.toast
                .as_ref()
                .map_or(INPUT_POLL, |t| t.until.saturating_duration_since(now))
                .min(INPUT_POLL)
        };
        if !event::poll(wait)? {
            continue;
        }
        let had_sheet = matches!(app.mode, Mode::Sheet(_));
        match event::read()? {
            Event::Key(key) => match app.on_key(key) {
                Action::Quit => break,
                Action::Suspend => {
                    suspend(terminal)?;
                    dirty = true;
                }
                Action::Rescan => {
                    match app.source.load() {
                        Ok(board) if board == app.board => {
                            app.notify("No changes since the last scan.", Tone::Ok)
                        }
                        Ok(board) => motion.rows_changed(app.replace_board(board)),
                        Err(e) => app.notify(format!("Rescan failed: {e:#}"), Tone::Danger),
                    }
                    dirty = true;
                }
                Action::Redraw => dirty = true,
                Action::None => {}
            },
            Event::Resize(..) => dirty = true,
            _ => {}
        }
        if !had_sheet && matches!(app.mode, Mode::Sheet(_)) {
            motion.sheet_opened();
        }
    }
    Ok(())
}

/// SIGTERM and SIGINT ask the loop to stop and restore the terminal. A hung-up terminal can
/// leave crossterm spinning inside `poll`, where the loop never sees the flag, so SIGHUP
/// exits at once and the others exit after a grace period.
fn watch_signals(stop: Arc<AtomicBool>) -> Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])?;
    std::thread::spawn(move || {
        let Some(sig) = signals.forever().next() else {
            return;
        };
        stop.store(true, Ordering::Relaxed);
        if sig != SIGHUP {
            std::thread::sleep(SIGNAL_GRACE);
            ratatui::restore();
        }
        std::process::exit(128 + sig);
    });
    Ok(())
}

/// Ctrl-Z: give the shell its terminal back, stop, and take it again on `fg`.
fn suspend(terminal: &mut DefaultTerminal) -> Result<()> {
    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen, Show)?;
    unsafe { libc::raise(libc::SIGTSTP) };
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(())
}
