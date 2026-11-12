//! Times frames of every view at 120×40 in the Tinted look, drawn through crossterm into
//! a sink: layout, widgets, the diff and the escape codes, everything but the terminal.
//! `cargo run --release -p scry --example frames`

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::{TerminalOptions, Viewport};
use scry::tui::app::{App, Source};
use scry::tui::theme::{Colors, Rgb, SHIPPED, Theme};
use scry::tui::ui;
use scry_core::view::Board;

const FRAMES: usize = 2000;

fn main() -> anyhow::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let board = Board::parse(&std::fs::read_to_string(root.join("fixtures/design.json"))?)?;
    let theme = Theme::new(
        SHIPPED,
        Colors::True {
            bg: Rgb(0x1d, 0x1f, 0x21),
            fg: Rgb(0xc5, 0xc8, 0xc6),
        },
    );
    println!("{FRAMES} frames per view at 120×40, Tinted on Tomorrow Night");
    println!(
        "{:<10} {:>22} {:>22}",
        "view", "a key (median, p99)", "full repaint"
    );
    for (view, keys) in [
        ("overview", "1"),
        ("ports", "2"),
        ("agents", "3"),
        ("disk", "4"),
        ("worktrees", "5"),
        ("agents /e", "3/e\n"),
    ] {
        let mut app = App::new(
            board.clone(),
            Source::Fixture(PathBuf::from("design.json")),
            theme.clone(),
        );
        for c in keys.chars() {
            press(
                &mut app,
                if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                },
            );
        }
        let step = time(&mut app, false)?;
        let full = time(&mut app, true)?;
        println!(
            "{view:<10} {:>10} {:>11} {:>10} {:>11}",
            ms(step.0),
            ms(step.1),
            ms(full.0),
            ms(full.1)
        );
    }
    Ok(())
}

fn press(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

/// Median and p99 of `FRAMES` draws, each after a j or k, or onto a blank screen.
fn time(app: &mut App, repaint: bool) -> anyhow::Result<(Duration, Duration)> {
    let blank = || {
        Terminal::with_options(
            CrosstermBackend::new(std::io::sink()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 120, 40)),
            },
        )
    };
    let mut term = blank()?;
    let mut times = Vec::with_capacity(FRAMES);
    for i in 0..FRAMES {
        if repaint {
            term = blank()?;
        } else {
            press(app, KeyCode::Char(if i % 8 < 4 { 'j' } else { 'k' }));
        }
        let t = Instant::now();
        term.draw(|f| {
            ui::draw(f, app);
        })?;
        times.push(t.elapsed());
    }
    times.sort();
    Ok((times[FRAMES / 2], times[FRAMES * 99 / 100]))
}

fn ms(d: Duration) -> String {
    format!("{:.3} ms", d.as_secs_f64() * 1000.0)
}
