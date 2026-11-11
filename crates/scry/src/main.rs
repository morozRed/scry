mod tui;

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
        #[arg(long)]
        json: bool,
        #[arg(long)]
        fixture: Option<PathBuf>,
    },
    /// Write a redacted snapshot of this machine as JSON.
    Snapshot {
        /// File to write. Prints to stdout without it.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Measure Scry itself.
    Bench {
        #[command(subcommand)]
        what: BenchWhat,
    },
}

#[derive(Subcommand)]
enum BenchWhat {
    /// Time a full process scan.
    Scan {
        #[arg(long, default_value_t = 20)]
        runs: usize,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
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
        Some(Command::Bench {
            what: BenchWhat::Scan { runs, json },
        }) => bench_scan(runs, json),
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
    let snap = fixture::redact(sys::scan()?, &fixture::home_dir()?);
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

fn bench_scan(runs: usize, json: bool) -> Result<()> {
    if runs == 0 {
        bail!("--runs must be at least 1");
    }
    let mut times = Vec::with_capacity(runs);
    let mut count = 0;
    for _ in 0..runs {
        let t = Instant::now();
        count = sys::scan()?.processes.len();
        times.push(t.elapsed());
    }
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let (min, median, max) = (ms(times[0]), ms(times[runs / 2]), ms(times[runs - 1]));
    if json {
        println!(
            "{}",
            serde_json::json!({ "processes": count, "runs": runs, "min_ms": min, "median_ms": median, "max_ms": max })
        );
    } else {
        println!(
            "Full scan of {count} processes: median {median:.1} ms (min {min:.1}, max {max:.1}) over {runs} runs."
        );
    }
    Ok(())
}
