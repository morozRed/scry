//! Renders TUI frames from fixtures as SVG into docs/screens.
//! `cargo run -p scry --example shots`; `scripts/shots.sh` also makes PNGs.

mod svg;

use std::path::{Path, PathBuf};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use scry::tui::theme::{Colors, Look, Rgb, SHIPPED, Theme};
use scry::tui::{App, Source, ui};
use scry_core::view::{Board, Load};
use svg::{Palette, TERMINAL_BASIC, TOMORROW_NIGHT};

const SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];
const PALETTES: [&Palette; 2] = [&TERMINAL_BASIC, &TOMORROW_NIGHT];

fn main() -> anyhow::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("docs/screens");
    std::fs::create_dir_all(out.join("compare"))?;
    let design = Board::parse(&std::fs::read_to_string(root.join("fixtures/design.json"))?)?;
    let mut n = 0;

    for (look, name) in [
        (Look::Native, "native"),
        (Look::Tinted, "tinted"),
        (Look::Panels, "panels"),
    ] {
        for pal in PALETTES {
            for (w, h) in SIZES {
                let theme = Theme::new(look, colors(look, pal));
                let file = format!("compare/{name}-{}-{w}x{h}", pal.name);
                n += shot(&out, &file, &design, theme.clone(), "3jj", pal, w, h)?;
            }
            let theme = Theme::new(look, colors(look, pal));
            let file = format!("compare/{name}-{}-sheet", pal.name);
            n += shot(&out, &file, &design, theme, "3X", pal, 120, 40)?;
        }
    }

    let views = [
        ("overview", ""),
        ("ports", "2jj"),
        ("agents", "3jj"),
        ("disk", "4"),
        ("worktrees", "5"),
    ];
    for pal in PALETTES {
        let theme = || Theme::new(SHIPPED, colors(SHIPPED, pal));
        let p = pal.name;
        for (view, keys) in views {
            for (w, h) in SIZES {
                n += shot(
                    &out,
                    &format!("{view}-{p}-{w}x{h}"),
                    &design,
                    theme(),
                    keys,
                    pal,
                    w,
                    h,
                )?;
            }
        }
        n += shot(
            &out,
            &format!("sheet-{p}"),
            &design,
            theme(),
            "3X",
            pal,
            120,
            40,
        )?;
        n += shot(
            &out,
            &format!("filter-{p}"),
            &design,
            theme(),
            "3/nxt",
            pal,
            80,
            24,
        )?;
        n += shot(
            &out,
            &format!("help-{p}"),
            &design,
            theme(),
            "3?",
            pal,
            80,
            24,
        )?;
        n += shot(
            &out,
            &format!("ansi-{p}"),
            &design,
            Theme::new(SHIPPED, Colors::Ansi),
            "3jj",
            pal,
            80,
            24,
        )?;
        let mut states = design.clone();
        states.ports = Load::Loading;
        n += shot(
            &out,
            &format!("state-loading-{p}"),
            &states,
            theme(),
            "2",
            pal,
            80,
            24,
        )?;
        states.ports = Load::Failed("sysctl net.inet.tcp.pcblist_n returned EPERM.".into());
        n += shot(
            &out,
            &format!("state-failed-{p}"),
            &states,
            theme(),
            "2",
            pal,
            80,
            24,
        )?;
        states.ports = Load::Ready(Vec::new());
        n += shot(
            &out,
            &format!("state-empty-{p}"),
            &states,
            theme(),
            "2",
            pal,
            80,
            24,
        )?;
    }
    let mono = Theme::new(SHIPPED, Colors::Mono);
    n += shot(
        &out,
        "no-color-dark",
        &design,
        mono,
        "3jj",
        &TOMORROW_NIGHT,
        80,
        24,
    )?;

    println!("Wrote {n} screens to {}.", out.canonicalize()?.display());
    Ok(())
}

/// The colours a terminal with this palette would report: truecolor for the tinted look,
/// which reads the background, and the palette itself for the ANSI looks.
fn colors(look: Look, pal: &Palette) -> Colors {
    match look {
        Look::Tinted => Colors::True {
            bg: Rgb(pal.bg.0, pal.bg.1, pal.bg.2),
            fg: Rgb(pal.fg.0, pal.fg.1, pal.fg.2),
        },
        _ => Colors::Ansi,
    }
}

#[allow(clippy::too_many_arguments)]
fn shot(
    out: &Path,
    name: &str,
    board: &Board,
    theme: Theme,
    keys: &str,
    pal: &Palette,
    w: u16,
    h: u16,
) -> anyhow::Result<usize> {
    let mut app = App::new(
        board.clone(),
        Source::Fixture(PathBuf::from("design.json")),
        theme,
    );
    for c in keys.chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    let mut term = Terminal::new(TestBackend::new(w, h))?;
    term.draw(|f| {
        ui::draw(f, &app);
    })?;
    std::fs::write(
        out.join(format!("{name}.svg")),
        svg::render(term.backend().buffer(), pal),
    )?;
    Ok(1)
}
