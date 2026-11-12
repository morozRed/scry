//! `scry overview`, `scry agents`, `scry leftovers` and `scry watch`: the Overview and Agents
//! views as text and JSON, from the same [`Board`] the TUI draws.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Args;
use scry_core::attribution::{Known, Lineage, Policy};
use scry_core::engine::Labels;
use scry_core::fixture;
use scry_core::home::Home;
use scry_core::label::printable;
use scry_core::live::{Collector, History};
use scry_core::model::Snapshot;
use scry_core::view::agents::{self, AgentRow, Leftover};
use scry_core::view::{Board, Tab, human_age, human_bytes};

/// Print what needs attention and exit.
#[derive(Args, Default)]
pub struct OverviewArgs {
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    /// Read a recorded snapshot instead of this machine.
    #[arg(long)]
    fixture: Option<PathBuf>,
}

/// List agent sessions, and with --tree the processes each one started.
#[derive(Args)]
pub struct AgentsArgs {
    /// Show each session's processes under it.
    #[arg(long)]
    tree: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    /// Read a recorded snapshot instead of this machine.
    #[arg(long)]
    fixture: Option<PathBuf>,
}

/// List processes left running by agent sessions that ended, with the evidence.
#[derive(Args)]
pub struct LeftoversArgs {
    /// Print JSON instead of a table.
    #[arg(long)]
    json: bool,
    /// Read a recorded snapshot instead of this machine.
    #[arg(long)]
    fixture: Option<PathBuf>,
}

/// Print the board as one JSON line now and again each time it changes.
#[derive(Args)]
pub struct WatchArgs {
    /// Print JSON lines. Required: the stream is for programs, such as a menu bar app.
    #[arg(long, required = true)]
    json: bool,
    /// Seconds between readings.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..))]
    interval: u64,
}

struct Reading {
    snap: Snapshot,
    lineage: Lineage,
    history: Option<History>,
    home: Option<Home>,
}

fn read(fixture: Option<PathBuf>, labels: Labels) -> Result<Reading> {
    if let Some(path) = fixture {
        let snap = fixture::load(&path)?;
        let lineage = Lineage::build(&snap, &Known::default(), &Policy::default());
        return Ok(Reading {
            snap,
            lineage,
            history: None,
            home: None,
        });
    }
    let mut c = Collector::new(labels);
    let r = c.read()?;
    Ok(Reading {
        snap: r.snap,
        lineage: r.lineage,
        history: Some(r.history),
        home: Home::from_env(),
    })
}

impl Reading {
    fn board(&self) -> Board {
        Board::build(&self.snap, &self.lineage, self.home.as_ref())
    }

    /// What the reading lacked, as a line to print under the answer.
    fn caveat(&self) -> Option<&'static str> {
        match self.history {
            Some(History::SpoolOnly) => Some(
                "No daemon store yet, so children of an agent that has exited are found only by their env stamps. `scry daemon install` records the rest.",
            ),
            Some(History::Unreadable(_)) => {
                Some("The store could not be read, so recorded ancestry is missing.")
            }
            _ => None,
        }
    }
}

pub fn overview(a: OverviewArgs) -> Result<()> {
    let r = read(a.fixture, Labels::All)?;
    let board = r.board();
    let o = &board.overview;
    if a.json {
        println!("{}", serde_json::to_string_pretty(o)?);
        return Ok(());
    }
    println!("{}", o.headline());
    for item in &o.attention {
        let size = item
            .size
            .map(|s| format!("  {}", human_bytes(s)))
            .unwrap_or_default();
        let context = item
            .context
            .as_deref()
            .map(|c| format!("  {c}"))
            .unwrap_or_default();
        println!(
            "  {} {}{}{}  → {}",
            item.state.word(),
            printable(&item.title),
            printable(&context),
            size,
            tab_command(item.tab)
        );
    }
    if let Some(line) = o.checked_line() {
        println!("{line}");
    }
    println!("{}.", o.counts_line());
    if let Some(c) = r.caveat() {
        println!("{c}");
    }
    Ok(())
}

fn tab_command(tab: Tab) -> &'static str {
    match tab {
        Tab::Overview => "scry overview",
        Tab::Ports => "scry ports",
        Tab::Agents => "scry agents --tree",
        Tab::Disk => "scry disk",
        Tab::Worktrees => "scry worktrees",
    }
}

pub fn agents(a: AgentsArgs) -> Result<()> {
    let r = read(a.fixture, Labels::All)?;
    let rows: Vec<AgentRow> = agents::rows(&r.snap, &r.lineage, r.home.as_ref())
        .into_iter()
        .filter(|row| a.tree || row.depth == 0)
        .collect();
    if a.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("No agent sessions are running.");
        println!(
            "Scry recognises Claude Code, Codex, OpenCode and Gemini CLI by their executables, and lists their sessions here while they run."
        );
        println!("`scry hook install claude` makes Claude Code attribution exact.");
        return Ok(());
    }
    print!("{}", table(&rows));
    if let Some(c) = r.caveat() {
        println!("{c}");
    }
    Ok(())
}

fn table(rows: &[AgentRow]) -> String {
    let head = [
        "SESSION / PROCESS",
        "PID",
        "RAM",
        "PORT",
        "AGE",
        "EVIDENCE",
        "STATE",
    ];
    let cells: Vec<[String; 7]> = rows
        .iter()
        .map(|r| {
            let name = match r.depth {
                0 => r.session_name(),
                d => format!("{}{}", "  ".repeat(d as usize), r.label),
            };
            [
                name,
                r.pid.map(|p| p.to_string()).unwrap_or_default(),
                r.ram.map(human_bytes).unwrap_or_default(),
                r.ports
                    .iter()
                    .map(|p| format!(":{p}"))
                    .collect::<Vec<_>>()
                    .join(","),
                human_age(r.age_s),
                r.evidence.map(|e| e.word().to_string()).unwrap_or_default(),
                r.state.map(|s| s.word().to_string()).unwrap_or_default(),
            ]
            .map(|c| printable(&c))
        })
        .collect();
    columns(&head, &cells)
}

fn columns<const N: usize>(head: &[&str; N], cells: &[[String; N]]) -> String {
    let mut width = head.map(|h| h.chars().count());
    for c in cells {
        for (w, s) in width.iter_mut().zip(c) {
            *w = (*w).max(s.chars().count().min(60));
        }
    }
    let line = |c: &[String]| {
        let parts: Vec<String> = c
            .iter()
            .zip(width)
            .map(|(s, w)| {
                let s: String = s.chars().take(60).collect();
                format!("{s:<w$}")
            })
            .collect();
        parts.join("  ").trim_end().to_string() + "\n"
    };
    let mut out = line(&head.map(String::from));
    for c in cells {
        out.push_str(&line(c));
    }
    out
}

pub fn leftovers(a: LeftoversArgs) -> Result<()> {
    let r = read(a.fixture, Labels::All)?;
    let list: Vec<Leftover> = agents::leftovers(&r.snap, &r.lineage, r.home.as_ref());
    if a.json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    if list.is_empty() {
        let own = r.snap.own_processes().count();
        println!(
            "No leftovers. Checked {own} of your processes in {} agent sessions against the leftover rule.",
            r.lineage.sessions.len()
        );
        if let Some(c) = r.caveat() {
            println!("{c}");
        }
        return Ok(());
    }
    let head = [
        "PID", "COMMAND", "SESSION", "EVIDENCE", "AGE", "RAM", "PORT",
    ];
    let cells: Vec<[String; 7]> = list
        .iter()
        .map(|l| {
            [
                l.pid.to_string(),
                l.command.clone(),
                format!("{} {}", l.agent.short(), l.session),
                l.evidence.word().to_string(),
                human_age(l.age_s),
                l.ram.map(human_bytes).unwrap_or_default(),
                l.ports
                    .iter()
                    .map(|p| format!(":{p}"))
                    .collect::<Vec<_>>()
                    .join(","),
            ]
            .map(|c| printable(&c))
        })
        .collect();
    print!("{}", columns(&head, &cells));
    if let Some(c) = r.caveat() {
        println!("{c}");
    }
    Ok(())
}

/// One JSON line per change. A change is a row added, removed or changing state, not RAM or
/// age moving, so an idle machine prints nothing after the first line.
pub fn watch(a: WatchArgs) -> Result<()> {
    let mut c = Collector::new(Labels::All);
    let mut last: Option<Vec<(String, Option<&'static str>)>> = None;
    let stdout = std::io::stdout();
    loop {
        match c.board() {
            Ok(board) => {
                let sig = signature(&board);
                if last.as_ref() != Some(&sig) {
                    let line = serde_json::json!({
                        "at_ms": scry_core::sys::now_ms(),
                        "board": board,
                    });
                    let mut out = stdout.lock();
                    // A reader that went away ends the stream quietly.
                    if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                        return Ok(());
                    }
                    last = Some(sig);
                }
            }
            Err(e) => eprintln!("scry watch: {e:#}"),
        }
        std::thread::sleep(Duration::from_secs(a.interval));
    }
}

fn signature(b: &Board) -> Vec<(String, Option<&'static str>)> {
    let mut sig: Vec<(String, Option<&'static str>)> = b
        .overview
        .attention
        .iter()
        .map(|a| (a.id.clone(), Some(a.state.word())))
        .collect();
    if let Some(rows) = b.agents.ready() {
        sig.extend(
            rows.iter()
                .map(|r| (r.id.clone(), r.state.map(|s| s.word()))),
        );
    }
    if let Some(rows) = b.ports.ready() {
        sig.extend(rows.iter().map(|r| {
            (
                format!("{}-{}-{}", r.proto.name(), r.port, r.pid),
                r.state.map(|s| s.word()),
            )
        }));
    }
    sig
}
