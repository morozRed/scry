//! `scry worktrees` and `scry bench worktrees`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use clap::Args;
use scry_core::config::Config;
use scry_core::paths::Paths;
use scry_core::view::worktrees::{WorktreesView, bytes};
use scry_core::worktree::{self, ScanOptions, Timings, WorktreeScan};
use serde::Serialize;

#[derive(Args)]
pub struct WorktreesArgs {
    #[arg(long)]
    json: bool,
    /// A folder to look for repos in. Repeat for more. Defaults to `code_roots` from the
    /// config file.
    #[arg(long = "root", value_name = "DIR")]
    roots: Vec<PathBuf>,
    /// Skip sizing, which walks every file and takes most of the time. The same walk looks
    /// for repositories nested in each worktree, so without it none is called "can go".
    #[arg(long)]
    no_size: bool,
}

#[derive(Args)]
pub struct BenchArgs {
    #[arg(long, default_value_t = 5)]
    runs: usize,
    #[arg(long)]
    json: bool,
    #[arg(long = "root", value_name = "DIR")]
    roots: Vec<PathBuf>,
}

fn options(roots: Vec<PathBuf>) -> Result<ScanOptions> {
    let paths = Paths::from_env()?;
    let roots = if roots.is_empty() {
        Config::load(&paths)?.0.code_roots
    } else {
        roots
    };
    Ok(ScanOptions::new(&paths.user_home, roots))
}

pub fn run(args: WorktreesArgs) -> Result<()> {
    let opts = options(args.roots)?;
    let mut scan = worktree::scan(&opts);
    if !args.no_size {
        worktree::measure(&mut scan, &opts.home);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let view = WorktreesView::new(&scan, now, &opts.home);
    if args.json {
        #[derive(Serialize)]
        struct Out<'a> {
            #[serde(flatten)]
            view: &'a WorktreesView,
            roots: &'a [PathBuf],
            repos_scanned: usize,
            warnings: &'a [String],
            timings: Timings,
        }
        let out = Out {
            view: &view,
            roots: &scan.roots,
            repos_scanned: scan.repos,
            warnings: &scan.warnings,
            timings: scan.timings,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    print_table(&view);
    for w in &scan.warnings {
        eprintln!("warning: {w}");
    }
    println!(
        "{} Took {:.1} s.",
        view.summary,
        scan.timings.total_ms / 1000.0
    );
    Ok(())
}

fn print_table(view: &WorktreesView) {
    if view.rows.is_empty() {
        return;
    }
    let size = |b: Option<u64>| b.map_or("-".into(), bytes);
    let cells: Vec<[String; 8]> = view
        .rows
        .iter()
        .map(|r| {
            [
                r.status.word().to_string(),
                clip(&r.name, 32),
                clip(&r.repo, 18),
                clip(&r.branch, 28),
                size(r.allocated),
                // Reclaimable is a lower bound.
                r.reclaimable
                    .map_or("-".into(), |b| format!("≥ {}", bytes(b))),
                r.creator.clone(),
                r.last_commit.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    let head = [
        "STATE", "WORKTREE", "REPO", "BRANCH", "SIZE", "FREES", "CREATOR", "LAST",
    ];
    let mut width = head.map(str::len);
    for c in &cells {
        for (w, s) in width.iter_mut().zip(c) {
            *w = (*w).max(s.chars().count());
        }
    }
    let line = |c: &[&str]| {
        let mut out = String::new();
        for (i, (s, w)) in c.iter().zip(width).enumerate() {
            let pad = w.saturating_sub(s.chars().count());
            // Sizes read best right-aligned.
            if i == 4 || i == 5 {
                out.push_str(&" ".repeat(pad));
                out.push_str(s);
            } else {
                out.push_str(s);
                out.push_str(&" ".repeat(pad));
            }
            out.push_str("  ");
        }
        out.trim_end().to_string()
    };
    println!("{}", line(&head));
    for (c, r) in cells.iter().zip(&view.rows) {
        println!("{}", line(&c.each_ref().map(String::as_str)));
        println!("    {}", r.sentence);
    }
    println!();
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

pub fn bench(args: BenchArgs) -> Result<()> {
    if args.runs == 0 {
        bail!("--runs must be at least 1");
    }
    let opts = options(args.roots)?;
    let scans: Vec<WorktreeScan> = (0..args.runs)
        .map(|_| {
            let mut scan = worktree::scan(&opts);
            worktree::measure(&mut scan, &opts.home);
            scan
        })
        .collect();
    let last = scans.last().expect("at least one run");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    // The same view as `scry worktrees`, so both print the same counts.
    let view = WorktreesView::new(last, now, &opts.home);
    let entries: u64 = last
        .worktrees
        .iter()
        .filter_map(|w| w.size.as_ref())
        .map(|s| s.files + s.dirs)
        .sum();
    let median = |f: fn(&Timings) -> f64| {
        let mut v: Vec<f64> = scans.iter().map(|s| f(&s.timings)).collect();
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let first = scans[0].timings;
    let med = Timings {
        discover_ms: median(|t| t.discover_ms),
        git_ms: median(|t| t.git_ms),
        size_ms: median(|t| t.size_ms),
        total_ms: median(|t| t.total_ms),
    };
    if args.json {
        let v = serde_json::json!({
            "roots": opts.roots,
            "repos_scanned": last.repos,
            "repos": view.repos,
            "worktrees": view.rows.len(),
            "counts": view.counts,
            "can_go_frees_at_least": view.reclaimable,
            "entries_sized": entries,
            "runs": args.runs,
            "first": first,
            "median": med,
        });
        println!("{v}");
    } else {
        println!("{}", view.summary);
        println!(
            "{entries} entries sized under {}.",
            show_roots(&opts.roots, &opts.home)
        );
        println!(
            "First run {:.0} ms (discover {:.0}, git {:.0}, size {:.0}).",
            first.total_ms, first.discover_ms, first.git_ms, first.size_ms
        );
        println!(
            "Median of {} runs {:.0} ms (discover {:.0}, git {:.0}, size {:.0}).",
            args.runs, med.total_ms, med.discover_ms, med.git_ms, med.size_ms
        );
    }
    Ok(())
}

fn show_roots(roots: &[PathBuf], home: &Path) -> String {
    roots
        .iter()
        .map(|r| match r.strip_prefix(home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => r.display().to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}
