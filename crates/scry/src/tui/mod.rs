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
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, Event, KeyEvent};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use scry_core::view::{Board, Tone};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use app::{Action, App, Mode, Source};
use motion::Motion;
use theme::{Colors, Env, SHIPPED, Theme};
use tty::LateReply;

const SAMPLE_EVERY: Duration = Duration::from_secs(2);
const INPUT_POLL: Duration = Duration::from_millis(250);
const FRAME: Duration = Duration::from_millis(16);
const SIGNAL_GRACE: Duration = Duration::from_secs(1);

/// The signal that asked Scry to stop, or 0.
static SIGNAL: AtomicI32 = AtomicI32::new(0);

pub fn run(fixture: Option<PathBuf>) -> Result<()> {
    let source = fixture.map_or(Source::Live, Source::Fixture);
    let live = matches!(source, Source::Live);
    let board = source.load()?;

    tty::save();
    watch_signals()?;
    let mut terminal = tty::change(ratatui::try_init)?;
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

    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut motion,
        &rx,
        answer.typed,
        answer.late,
    );
    exit_on_signal();
    tty::change(ratatui::restore);
    result
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
    rx: &mpsc::Receiver<Board>,
    typed: Vec<KeyEvent>,
    mut late: Option<LateReply>,
) -> Result<()> {
    let mut dirty = true;
    let mut keys = VecDeque::from(typed);
    while SIGNAL.load(Ordering::Relaxed) == 0 {
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
        if let Some(held) = late.as_mut().and_then(|l| l.expire(now)) {
            keys.extend(held);
            late = None;
        }
        while let Some(key) = keys.pop_front() {
            let had_sheet = matches!(app.mode, Mode::Sheet(_));
            match app.on_key(key) {
                Action::Quit => return Ok(()),
                Action::Suspend => suspend(terminal)?,
                Action::Rescan => match app.source.load() {
                    Ok(board) if board == app.board => {
                        app.notify("No changes since the last scan.", Tone::Ok)
                    }
                    Ok(board) => motion.rows_changed(app.replace_board(board)),
                    Err(e) => app.notify(format!("Rescan failed: {e:#}"), Tone::Danger),
                },
                Action::Redraw => {}
                Action::None => continue,
            }
            dirty = true;
            if !had_sheet && matches!(app.mode, Mode::Sheet(_)) {
                motion.sheet_opened();
            }
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
    tty::change(|| -> Result<()> {
        disable_raw_mode()?;
        execute!(std::io::stdout(), LeaveAlternateScreen, Show)?;
        Ok(())
    })?;
    // SAFETY: raising a signal at the calling thread has no memory effects.
    unsafe { libc::raise(libc::SIGTSTP) };
    tty::change(|| -> Result<()> {
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen)?;
        Ok(())
    })?;
    terminal.clear()?;
    Ok(())
}
