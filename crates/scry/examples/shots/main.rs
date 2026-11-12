//! Renders TUI frames from fixtures as SVG into docs/screens.
//! `cargo run -p scry --example shots`; `scripts/shots.sh` also makes PNGs.

mod svg;

use std::path::{Path, PathBuf};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use scry::tui::app::{App, Mode, Request, SheetKind, Source};
use scry::tui::theme::{Colors, Look, Rgb, SHIPPED, Theme};
use scry::tui::ui;
use scry_core::ProcId;
use scry_core::control::Outcome;
use scry_core::keep::KeepAs;
use scry_core::reap::{Done, ReapTarget, Report};
use scry_core::view::agents::AgentRow;
use scry_core::view::{Board, Detail, Evidence, Load, State};
use svg::{Palette, SOLARIZED_LIGHT, TERMINAL_BASIC, TOMORROW_NIGHT};

const SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];
const PALETTES: [&Palette; 2] = [&TERMINAL_BASIC, &TOMORROW_NIGHT];

fn main() -> anyhow::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("docs/screens");
    std::fs::create_dir_all(out.join("compare"))?;
    let design = Board::parse(&std::fs::read_to_string(root.join("fixtures/design.json"))?)?;
    let long = with_leftovers(&design, 20);
    let keepable = keepable(&design);
    let shots = Shots { out: &out };
    let mut n = 0;

    for (look, name) in [(Look::Native, "native"), (Look::Tinted, "tinted")] {
        for pal in PALETTES {
            let theme = || Theme::new(look, colors(look, pal));
            for (w, h) in SIZES {
                let file = format!("compare/{name}-{}-{w}x{h}", pal.name);
                n += shots.take(&file, &design, theme(), "3jj", pal, (w, h))?;
            }
            let file = format!("compare/{name}-{}-sheet", pal.name);
            n += shots.take(&file, &design, theme(), "3X", pal, (120, 40))?;
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
            for size in SIZES {
                let (w, h) = size;
                n += shots.take(
                    &format!("{view}-{p}-{w}x{h}"),
                    &design,
                    theme(),
                    keys,
                    pal,
                    size,
                )?;
            }
        }
        for (name, board, keys, size) in [
            ("agents-{p}-150x40", &design, "3jj", (150, 40)),
            ("ports-{p}-150x40", &design, "2jj", (150, 40)),
            ("sheet-{p}", &design, "3X", (120, 40)),
            ("sheet-{p}-80x24", &design, "3X", (80, 24)),
            ("keep-{p}", &keepable, "3jjK", (80, 24)),
            ("sheet-long-{p}", &long, "3X", (80, 24)),
            ("sheet-long-end-{p}", &long, "3XG", (80, 24)),
            ("sheet-long-{p}-150x40", &long, "3X", (150, 40)),
            ("filter-{p}", &design, "3/vite", (80, 24)),
            ("disk-delete-{p}", &design, "4ltXcode Deriv", (80, 24)),
            ("disk-trash-{p}", &design, "4vjjjjjjjjjjjjjt", (120, 40)),
            ("disk-list-{p}-120x40", &design, "4vj", (120, 40)),
            ("disk-{p}-150x40", &design, "4l", (150, 40)),
            ("overview-{p}-150x40", &design, "", (150, 40)),
            ("detail-{p}", &design, "3jj\n", (80, 24)),
            ("palette-{p}", &design, "3:re", (120, 40)),
            ("help-{p}", &design, "3?", (80, 24)),
        ] {
            let name = name.replace("{p}", p);
            n += shots.take(&name, board, theme(), keys, pal, size)?;
        }
        // The reap sheet confirmed: the outcome in the footer, SIGKILL offered on its own.
        for size in [(120, 40), (80, 24)] {
            let (w, h) = size;
            n += shots.take_then(
                &format!("sheet-done-{p}-{w}x{h}"),
                &design,
                theme(),
                "3X",
                pal,
                size,
                confirmed,
            )?;
        }
        let ansi = || Theme::new(SHIPPED, Colors::Ansi);
        n += shots.take(&format!("ansi-{p}"), &design, ansi(), "3jj", pal, (80, 24))?;
        n += shots.take(
            &format!("ansi-{p}-150x40"),
            &design,
            ansi(),
            "3jj",
            pal,
            (150, 40),
        )?;
        n += shots.take(
            &format!("ansi-sheet-long-{p}"),
            &long,
            ansi(),
            "3X",
            pal,
            (80, 24),
        )?;
        let mut states = design.clone();
        for (state, load) in [
            ("loading", Load::Loading),
            (
                "failed",
                Load::Failed("sysctl net.inet.tcp.pcblist_n returned EPERM.".into()),
            ),
            ("empty", Load::Ready(Vec::new())),
        ] {
            states.ports = load;
            let name = format!("state-{state}-{p}");
            n += shots.take(&name, &states, theme(), "2", pal, (80, 24))?;
        }
        for (state, board) in [
            ("sizing", worktrees(&design, true)),
            ("empty", worktrees(&design, false)),
        ] {
            let name = format!("worktrees-{state}-{p}");
            n += shots.take(&name, &board, theme(), "5", pal, (80, 24))?;
        }
        // A first run with nothing to report: what each view checked and how to fill it.
        let mut first = design.clone();
        first.overview.attention.clear();
        first.agents = Load::Ready(Vec::new());
        first.ports = Load::Ready(Vec::new());
        n += shots.take(
            &format!("first-run-overview-{p}"),
            &first,
            theme(),
            "",
            pal,
            (80, 24),
        )?;
        n += shots.take(
            &format!("first-run-agents-{p}"),
            &first,
            theme(),
            "3",
            pal,
            (80, 24),
        )?;
    }
    let sol = &SOLARIZED_LIGHT;
    n += shots.take(
        "agents-solarized-light-80x24",
        &design,
        Theme::new(SHIPPED, colors(SHIPPED, sol)),
        "3jj",
        sol,
        (80, 24),
    )?;
    let mono = Theme::new(SHIPPED, Colors::Mono);
    n += shots.take(
        "no-color-dark",
        &design,
        mono,
        "3jj",
        &TOMORROW_NIGHT,
        (80, 24),
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

/// The design board with one more ended session that left `n` processes, for a reap
/// sheet longer than the screen.
fn with_leftovers(design: &Board, n: usize) -> Board {
    let mut board = design.clone();
    let Load::Ready(rows) = &mut board.agents else {
        return board;
    };
    let row = |id: String, depth: u8, label: String, pid, state, evidence| AgentRow {
        id,
        depth,
        label,
        context: (depth == 0).then(|| "acme/batch".into()),
        pid,
        start_us: None,
        keep: None,
        ancestors: 0,
        session_id: None,
        ram: Some(48 << 20),
        ports: vec![],
        age_s: 5 * 3600,
        evidence,
        state: Some(state),
        detail: Detail::default(),
    };
    rows.push(row(
        "s-batch".into(),
        0,
        "claude".into(),
        None,
        State::Ended,
        None,
    ));
    for i in 0..n {
        rows.push(row(
            format!("w-{i}"),
            1,
            format!("node worker-{i:02}"),
            Some(61000 + i as i32),
            State::Leftover,
            Some(Evidence::Stamp),
        ));
    }
    board
}

/// The design board with the keep entry a live reading gives each process row: the real
/// path of the script node runs.
fn keepable(design: &Board) -> Board {
    let mut board = design.clone();
    if let Load::Ready(rows) = &mut board.agents {
        for r in rows.iter_mut().filter(|r| r.depth > 0) {
            let tool = r.label.split(' ').next().unwrap_or("node");
            r.keep = Some(KeepAs::Entry(format!(
                "~/sources/acme/node_modules/{tool}/dist/bin/{tool}"
            )));
        }
    }
    board
}

/// What the live TUI shows once a confirmed reap comes back with one process that ignored
/// SIGTERM. Nothing is signalled: the outcome is made up from the sheet's targets.
fn confirmed(app: &mut App) {
    let Mode::Sheet(sheet) = std::mem::replace(&mut app.mode, Mode::Normal) else {
        return;
    };
    let last = sheet.targets.len() - 1;
    let done = sheet
        .targets
        .iter()
        .enumerate()
        .map(|(i, t)| Done {
            target: ReapTarget {
                id: ProcId {
                    pid: t.pid,
                    start_us: 0,
                },
                command: t.command.clone(),
                session: t.session.clone().unwrap_or_default(),
                session_id: None,
                evidence: t.evidence,
                ram: t.frees,
            },
            outcome: Some(if i == last {
                Outcome::StillRunning { after_ms: 3000 }
            } else {
                Outcome::Exited {
                    signal: "SIGTERM".into(),
                    after_ms: 120,
                }
            }),
            refusal: None,
            unaudited: None,
        })
        .collect();
    let req = Request {
        kind: SheetKind::Reap,
        origin: SheetKind::Reap,
        targets: sheet.targets,
    };
    app.acted(&req, Ok(Report { done }));
}

/// The design board's worktrees while their sizes are measured, or none found at all.
fn worktrees(design: &Board, sizing: bool) -> Board {
    let mut board = design.clone();
    if let Load::Ready(list) = &mut board.worktrees {
        list.sizing = sizing;
        if sizing {
            for w in &mut list.rows {
                w.size = None;
                w.frees = None;
                for fact in w.detail.facts.iter_mut().filter(|f| f.label == "size") {
                    fact.value = "measuring…".into();
                }
            }
        } else {
            list.rows.clear();
        }
    }
    board
}

struct Shots<'a> {
    out: &'a Path,
}

impl Shots<'_> {
    fn take(
        &self,
        name: &str,
        board: &Board,
        theme: Theme,
        keys: &str,
        pal: &Palette,
        size: (u16, u16),
    ) -> anyhow::Result<usize> {
        self.take_then(name, board, theme, keys, pal, size, |_| {})
    }

    /// [`Shots::take`], with `then` changing the app after the keys, as a reply from the
    /// live TUI's worker would.
    #[allow(clippy::too_many_arguments)]
    fn take_then(
        &self,
        name: &str,
        board: &Board,
        theme: Theme,
        keys: &str,
        pal: &Palette,
        (w, h): (u16, u16),
        then: impl FnOnce(&mut App),
    ) -> anyhow::Result<usize> {
        let mut app = App::new(
            board.clone(),
            Source::Fixture(PathBuf::from("design.json")),
            theme,
        );
        let mut term = Terminal::new(TestBackend::new(w, h))?;
        // Keys act on the columns last drawn, as in the real loop.
        term.draw(|f| {
            ui::draw(f, &app);
        })?;
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                c => KeyCode::Char(c),
            };
            app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        then(&mut app);
        term.draw(|f| {
            ui::draw(f, &app);
        })?;
        std::fs::write(
            self.out.join(format!("{name}.svg")),
            svg::render(term.backend().buffer(), pal),
        )?;
        Ok(1)
    }
}
