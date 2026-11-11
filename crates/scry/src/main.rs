mod daemon_cmd;
mod doctor_cmd;
mod ports;
mod stop;
mod worktrees;

use scry::tui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use scry_core::view::Overview;
use scry_core::{Snapshot, fixture, sys};

#[derive(Parser)]
#[command(name = "scry", version, about = "See what your agents left running")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the terminal UI. Bare `scry` does this when run in a terminal.
    Tui {
        /// Draw from a recorded snapshot instead of this machine.
        #[arg(long)]
        fixture: Option<PathBuf>,
    },
    /// Print what needs attention and exit.
    Overview {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Read a recorded snapshot instead of this machine.
        #[arg(long)]
        fixture: Option<PathBuf>,
    },
    /// Write a redacted snapshot of this machine as JSON.
    Snapshot {
        /// File to write. Prints to stdout without it.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    Ports(ports::PortsArgs),
    Why(ports::WhyArgs),
    Stop(stop::StopArgs),
    /// List linked worktrees with their state, size and probable creator.
    Worktrees(worktrees::WorktreesArgs),
    /// Measure Scry itself.
    Bench {
        #[command(subcommand)]
        what: BenchWhat,
    },
    /// Install, inspect or run the background recorder.
    Daemon {
        #[command(subcommand)]
        action: daemon_cmd::Action,
    },
    /// Check that Scry can work on this Mac, with a fix for each problem.
    Doctor {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum BenchWhat {
    /// Time the process scan, the socket table and a full snapshot.
    Scan {
        /// How many times to time each, at least 1.
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
        runs: u32,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Time a worktrees scan: discovery, git and sizing.
    Worktrees(worktrees::BenchArgs),
}

/// The exit code for arguments Scry cannot parse, from sysexits(3). Clap's own code, 2, is
/// what `scry stop` returns for a process that is still running.
const USAGE: i32 = 64;

fn main() -> Result<()> {
    let cli = Cli::try_parse().unwrap_or_else(|e| {
        let _ = e.print();
        std::process::exit(if e.use_stderr() { USAGE } else { 0 });
    });
    match cli.command {
        None if std::io::stdout().is_terminal() => tui::run(None),
        None => print_overview(&load(None)?, false),
        Some(Command::Tui { fixture }) => {
            if !std::io::stdout().is_terminal() {
                bail!("scry tui needs a terminal. Use `scry overview` for plain output.");
            }
            tui::run(fixture)
        }
        Some(Command::Overview { json, fixture }) => print_overview(&load(fixture)?, json),
        Some(Command::Snapshot { out }) => snapshot(out),
        Some(Command::Ports(a)) => ports::ports(a),
        Some(Command::Why(a)) => ports::why(a),
        Some(Command::Stop(a)) => stop::run(a),
        Some(Command::Worktrees(args)) => worktrees::run(args),
        Some(Command::Bench {
            what: BenchWhat::Scan { runs, json },
        }) => bench_scan(runs, json),
        Some(Command::Daemon { action }) => daemon_cmd::run(action),
        Some(Command::Doctor { json }) => doctor_cmd::run(json),
        Some(Command::Bench {
            what: BenchWhat::Worktrees(args),
        }) => worktrees::bench(args),
    }
}

pub(crate) fn load(fixture: Option<PathBuf>) -> Result<Snapshot> {
    match fixture {
        Some(path) => fixture::load(&path),
        None => sys::scan(),
    }
}

fn print_overview(snap: &Snapshot, json: bool) -> Result<()> {
    let o = Overview::from_snapshot(snap);
    if json {
        println!("{}", serde_json::to_string_pretty(&o)?);
    } else {
        println!("{}", o.headline());
        println!("{}.", o.counts_line());
    }
    Ok(())
}

fn snapshot(out: Option<PathBuf>) -> Result<()> {
    let snap = fixture::redact(
        scry_core::engine::Engine::new().snapshot()?,
        &fixture::home_dir()?,
    );
    let text = serde_json::to_string_pretty(&snap)?;
    match out {
        Some(path) => {
            std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
            eprintln!(
                "Wrote {} processes to {}.",
                snap.processes.len(),
                path.display()
            );
        }
        None => println!("{text}"),
    }
    Ok(())
}

fn bench_scan(runs: u32, json: bool) -> Result<()> {
    let runs = runs as usize;
    let (mut procs, mut socks, mut full) = (Vec::new(), Vec::new(), Vec::new());
    let (mut count, mut sockets) = (0, 0);
    let mut engine = scry_core::engine::Engine::new();
    for _ in 0..runs {
        let t = Instant::now();
        count = sys::scan()?.processes.len();
        procs.push(t.elapsed());
        let t = Instant::now();
        sockets = sys::sockets::read_all()?.len();
        socks.push(t.elapsed());
        let t = Instant::now();
        engine.snapshot()?;
        full.push(t.elapsed());
    }
    let stats = |times: &mut Vec<Duration>| {
        times.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        (ms(times[0]), ms(times[runs / 2]), ms(times[runs - 1]))
    };
    let (min, median, max) = stats(&mut procs);
    let (s_min, s_median, s_max) = stats(&mut socks);
    let (f_min, f_median, f_max) = stats(&mut full);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "processes": count, "runs": runs, "min_ms": min, "median_ms": median, "max_ms": max,
                "sockets": { "count": sockets, "min_ms": s_min, "median_ms": s_median, "max_ms": s_max },
                "snapshot": { "min_ms": f_min, "median_ms": f_median, "max_ms": f_max },
            })
        );
    } else {
        // local-ci.sh reads the last "median N ms" in this output as the process scan time.
        println!(
            "Socket table of {sockets} sockets: {s_median:.1} ms median (min {s_min:.1}, max {s_max:.1})."
        );
        println!(
            "Snapshot with sockets, labels and projects: {f_median:.1} ms median (min {f_min:.1}, max {f_max:.1}), arguments cached after the first run."
        );
        println!(
            "Full scan of {count} processes: median {median:.1} ms (min {min:.1}, max {max:.1}) over {runs} runs."
        );
    }
    Ok(())
}
