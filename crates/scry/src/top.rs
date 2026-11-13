//! `scry top`: what uses the CPU and memory, as the Top view shows it.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Args;
use scry_core::attribution::{Known, Lineage, Policy};
use scry_core::engine::Labels;
use scry_core::fixture;
use scry_core::label::printable;
use scry_core::live::Collector;
use scry_core::view::usage::{self, UsageView, percent};
use scry_core::view::{Load, human_bytes};

/// Show what uses the CPU and memory, grouped by app, heaviest first.
#[derive(Args)]
pub struct TopArgs {
    /// Print JSON instead of a table.
    #[arg(long)]
    json: bool,
    /// List every process of each group, not only the groups.
    #[arg(long)]
    tree: bool,
    /// How many groups to print; 0 prints all.
    #[arg(short = 'n', long, default_value_t = 15)]
    limit: usize,
    /// Milliseconds between the two readings CPU shares are measured over.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(100..=10_000))]
    interval: u64,
    /// Read a recorded snapshot instead of this machine. It has no CPU shares.
    #[arg(long)]
    fixture: Option<PathBuf>,
}

pub fn run(a: TopArgs) -> Result<()> {
    let view = match &a.fixture {
        Some(path) => {
            let snap = fixture::load(path)?;
            let lineage = Lineage::build(&snap, &Known::default(), &Policy::default());
            usage::build(&snap, &lineage, None, scry_core::sys::logical_cores(), None)
        }
        None => {
            let mut c = Collector::new(Labels::All);
            c.board()?;
            std::thread::sleep(Duration::from_millis(a.interval));
            match c.board()?.usage {
                Load::Ready(v) => v,
                Load::Failed(e) | Load::Unavailable(e) => anyhow::bail!(e),
                Load::Loading => anyhow::bail!("no reading"),
            }
        }
    };
    let mut view = view;
    trim(&mut view, a.limit, a.tree);
    if a.json {
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }
    print(&view);
    Ok(())
}

/// Keeps the first `limit` groups, and their processes only for `--tree`.
fn trim(v: &mut UsageView, limit: usize, tree: bool) {
    let mut groups = 0;
    v.rows.retain(|r| {
        if r.depth == 0 {
            groups += 1;
        }
        (limit == 0 || groups <= limit) && (tree || r.depth == 0)
    });
}

fn print(v: &UsageView) {
    match v.cpu {
        Some(c) => print!("CPU {} of {} cores", percent(c), v.cores),
        None => print!("CPU not measured"),
    }
    if let (Some(used), Some(total)) = (v.ram_used, v.ram_total) {
        print!(", memory {} of {}", human_bytes(used), human_bytes(total));
    }
    println!(".");
    let name_w = v
        .rows
        .iter()
        .map(|r| r.label.chars().count() + 2 * r.depth as usize)
        .max()
        .unwrap_or(0)
        .min(40);
    println!(
        "{:<name_w$}  {:>5}  {:>5}  {:>5}  STATE",
        "NAME", "CPU", "RAM", "PROCS"
    );
    for r in &v.rows {
        let name: String = format!("{}{}", "  ".repeat(r.depth as usize), printable(&r.label))
            .chars()
            .take(name_w)
            .collect();
        let cpu = r.cpu.map(percent).unwrap_or_else(|| "-".into());
        let ram = r.ram.map(human_bytes).unwrap_or_else(|| "-".into());
        let procs = if r.depth == 0 {
            r.count.to_string()
        } else {
            String::new()
        };
        let state = r.state.map(|s| s.word()).unwrap_or("");
        let context = r
            .context
            .as_deref()
            .map(|c| format!("  {}", printable(c)))
            .unwrap_or_default();
        println!("{name:<name_w$}  {cpu:>5}  {ram:>5}  {procs:>5}  {state}{context}");
    }
}
