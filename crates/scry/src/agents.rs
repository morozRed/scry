//! `scry overview`, `scry agents`, `scry leftovers` and `scry watch`: the Overview and Agents
//! views as text and JSON, from the same [`Board`] the TUI draws.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
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
    config_error: Option<String>,
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
            config_error: None,
        });
    }
    let mut c = Collector::new(labels);
    let config_error = c.config_error().map(str::to_string);
    let r = c.read()?;
    Ok(Reading {
        snap: r.snap,
        lineage: r.lineage,
        history: Some(r.history),
        home: Home::from_env(),
        config_error,
    })
}

impl Reading {
    fn board(&self) -> Board {
        let mut board = Board::build(&self.snap, &self.lineage, self.home.as_ref());
        if let Some(e) = &self.config_error {
            board.overview.config_unread(e);
        }
        board
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
    if let Some(e) = &r.config_error {
        println!("Your config could not be read, so no process is marked a leftover: {e}");
    }
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
    // Without the keep list the rule would select what the user asked to keep.
    if let Some(e) = &r.config_error {
        bail!("{e}\nScry selects no leftovers until the config is fixed; `scry doctor` checks it.");
    }
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
    print!("{}", leftover_table(&list));
    println!("{}", agents::PERSISTENT);
    if let Some(c) = r.caveat() {
        println!("{c}");
    }
    Ok(())
}

/// Leftovers as `scry leftovers` and `scry reap` print them, in the order given.
pub fn leftover_table(list: &[Leftover]) -> String {
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
    columns(&head, &cells)
}

/// One JSON line per change in what the board shows: the sessions, processes, ports and
/// attention items, their states, labels and ports. Ages, RAM, the pulse and the counts of
/// every process on the machine move on almost every reading by themselves, so they do not
/// start a line on their own.
pub fn watch(a: WatchArgs) -> Result<()> {
    let mut c = Collector::new(Labels::All);
    let mut last: Option<serde_json::Value> = None;
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

/// Fields that move by themselves: ages, RAM, the header's pulse, and counts of every process
/// on the machine (with the `checked` lines that quote them).
const RESTLESS: [&str; 10] = [
    "age_s",
    "age_secs",
    "ram",
    "frees",
    "size",
    "own_footprint",
    "process_count",
    "own_count",
    "checked",
    "pulse",
];

/// The board as JSON without what moves by itself: the [`RESTLESS`] fields, and the ages and
/// sizes that [`human_age`] and [`human_bytes`] write into sentences.
fn signature(b: &Board) -> serde_json::Value {
    let mut v = serde_json::to_value(b).unwrap_or_default();
    strip_restless(&mut v);
    v
}

fn strip_restless(v: &mut serde_json::Value) {
    use serde_json::Value;
    match v {
        Value::Object(m) => {
            m.retain(|k, _| !RESTLESS.contains(&k.as_str()));
            m.values_mut().for_each(strip_restless);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_restless),
        Value::String(s) => *s = without_sizes(&without_ages(s)),
        _ => {}
    }
}

/// `ended 41m ago, started 2h ago` → `ended ago, started ago`, and `4s old` → `old`.
fn without_ages(s: &str) -> String {
    if !s.contains(" ago") && !s.contains(" old") {
        return s.to_string();
    }
    let words: Vec<&str> = s.split(' ').collect();
    // By bytes: a word can be `…`, whose last byte is not a character of its own.
    let age = |w: &str| match w.as_bytes() {
        [digits @ .., unit] if !digits.is_empty() => {
            digits.iter().all(u8::is_ascii_digit) && matches!(unit, b's' | b'm' | b'h' | b'd')
        }
        _ => false,
    };
    words
        .iter()
        .enumerate()
        .filter(|&(i, w)| {
            !(age(w)
                && words.get(i + 1).is_some_and(|n| {
                    matches!(n.trim_end_matches([',', '.', ';', ')']), "ago" | "old")
                }))
        })
        .map(|(_, w)| *w)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `It holds 412M and :3000.` → `It holds and :3000.`: a size as [`human_bytes`] writes
/// it, a number with a unit letter, as a word of its own.
fn without_sizes(s: &str) -> String {
    let size = |w: &str| {
        let w = w.trim_end_matches([',', '.', ';', ')']);
        let Some(num) = w.strip_suffix(['B', 'K', 'M', 'G', 'T']) else {
            return false;
        };
        !num.is_empty()
            && num.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && num.bytes().next().is_some_and(|b| b.is_ascii_digit())
    };
    if !s.split(' ').any(size) {
        return s.to_string();
    }
    s.split(' ')
        .map(|w| if size(w) { "" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use scry_core::view::Load;

    #[test]
    fn watch_signatures_ignore_ages_and_nothing_else() {
        let board = Board::parse(include_str!("../../../fixtures/design.json")).unwrap();
        let sig = signature(&board);
        let mut older = board.clone();
        if let Load::Ready(rows) = &mut older.agents {
            for r in rows.iter_mut() {
                r.age_s += 7200;
                r.detail.sentence = r.detail.sentence.replace(" 2h ago", " 4h ago");
            }
        }
        for a in &mut older.overview.attention {
            a.detail.sentence = a
                .detail
                .sentence
                .replace("started 2h ago", "started 3d ago");
        }
        assert_eq!(signature(&older), sig, "only ages moved");

        // RAM, the pulse and machine-wide counts move on every reading: no line.
        let mut busier = board.clone();
        if let Load::Ready(rows) = &mut busier.agents {
            for r in rows.iter_mut() {
                r.ram = r.ram.map(|b| b * 3 + 1);
            }
        }
        for a in &mut busier.overview.attention {
            a.size = a.size.map(|b| b + 1);
            a.detail.sentence = a.detail.sentence.replace("953M", "1.2G");
        }
        assert_ne!(
            serde_json::to_value(&busier).unwrap(),
            serde_json::to_value(&board).unwrap()
        );
        assert!(
            serde_json::to_string(&busier)
                .unwrap()
                .contains("hold 1.2G and :3000"),
            "a size in a sentence moved"
        );
        busier.overview.process_count += 7;
        busier.overview.own_footprint += 1 << 20;
        busier.overview.checked = vec!["420 of your processes".into()];
        busier.pulse = None;
        assert_eq!(signature(&busier), sig, "only RAM and counts moved");

        // A new state, label or port does.
        let mut left = board.clone();
        if let Load::Ready(rows) = &mut left.agents {
            let r = rows.iter_mut().find(|r| r.depth > 0).unwrap();
            r.state = Some(scry_core::view::State::Kept);
        }
        assert_ne!(signature(&left), sig);
        let mut relabelled = board.clone();
        if let Load::Ready(rows) = &mut relabelled.agents {
            rows[1].label.push_str(" --watch");
        }
        assert_ne!(signature(&relabelled), sig);
        let mut ported = board.clone();
        if let Load::Ready(rows) = &mut ported.agents {
            rows[1].ports.push(5173);
        }
        assert_ne!(signature(&ported), sig);
        let mut fewer = board.clone();
        if let Load::Ready(rows) = &mut fewer.agents {
            rows.pop();
        }
        assert_ne!(signature(&fewer), sig);
        let mut retitled = board.clone();
        retitled.overview.attention[0].title.push('!');
        assert_ne!(signature(&retitled), sig);
        assert_eq!(
            without_ages("ended 41m ago, started 2h ago. Port 3000 at 5s"),
            "ended ago, started ago. Port 3000 at 5s"
        );
        assert_eq!(
            without_sizes("It holds 412M and :3000. Together 1.5G, 3 ports, 512B."),
            "It holds  and :3000. Together  3 ports, "
        );
        assert_eq!(without_sizes("v2 of acme 3000M5"), "v2 of acme 3000M5");
        // A label's `…` beside an age, as live data has it.
        assert_eq!(
            without_ages("`python3 …` started 2h ago, in …"),
            "`python3 …` started ago, in …"
        );
        assert_eq!(
            without_ages("It is 4s old, under the 5m grace period."),
            "It is old, under the 5m grace period."
        );
    }
}
