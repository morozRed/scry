//! `scry disk` and `scry clean`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use scry_core::config::Config;
use scry_core::disk::cache::{CacheStats, SizeCache};
use scry_core::disk::clean::{self, Action, Context, Plan, Refusal};
use scry_core::disk::{self, Reading, Timings};
use scry_core::label::printable;
use scry_core::paths::Paths;
use scry_core::sys::now_ms;
use scry_core::view::disk::{DiskItem, DiskList};
use scry_core::view::worktrees::bytes;
use serde::Serialize;
use serde_json::json;

/// What fills the disk: free space, the forecast, and the largest items ranked by what
/// deleting them frees, each with the cleanup it allows.
#[derive(Args)]
pub struct DiskArgs {
    /// How many items to list.
    #[arg(long, default_value_t = 15)]
    top: usize,
    #[arg(long)]
    json: bool,
}

/// Clean up one item from `scry disk`: move it to the Trash, delete a cache after its name
/// is typed, or remove the record of a worktree whose folder is gone. Exits 0 when done and
/// 1 when it was refused or stopped partway, saying exactly what changed.
#[derive(Args)]
pub struct CleanArgs {
    /// The item's id as `scry disk` prints it.
    item: String,
    /// Do not ask. A delete also needs --confirm with the item's name.
    #[arg(long)]
    yes: bool,
    /// The item's name, which a delete needs typed.
    #[arg(long, value_name = "NAME")]
    confirm: Option<String>,
    #[arg(long)]
    json: bool,
}

/// A whole reading and its view.
pub struct Disk {
    pub reading: Reading,
    pub list: DiskList,
    pub cache: SizeCache,
    pub paths: Paths,
    pub config: Config,
}

pub fn read() -> Result<Disk> {
    let paths = Paths::from_env()?;
    let (config, _) = Config::load(&paths)?;
    let cache = SizeCache::open(&paths);
    let home = paths.user_home.clone();
    let reading = disk::read(&home, &config.code_roots, &cache, now_ms() / 1000);
    let list = disk::view(
        &reading.layout.items,
        &cache,
        &paths,
        config.destructive,
        false,
    );
    Ok(Disk {
        reading,
        list,
        cache,
        paths,
        config,
    })
}

pub fn run(args: DiskArgs) -> Result<()> {
    let d = read()?;
    let shown: Vec<&DiskItem> = d.list.items.iter().take(args.top).collect();
    if args.json {
        #[derive(Serialize)]
        struct Out<'a> {
            #[serde(flatten)]
            list: &'a DiskList,
            space_line: String,
            forecast_line: String,
            listed: usize,
            of: usize,
            cache: CacheStats,
            timings: Timings,
        }
        let mut list = d.list.clone();
        list.items.truncate(args.top);
        let out = Out {
            space_line: d.list.space_line(),
            forecast_line: d.list.forecast_line(),
            listed: list.items.len(),
            of: d.list.items.len(),
            list: &list,
            cache: d.reading.stats,
            timings: d.reading.timings,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!("{}", d.list.space_line());
    println!("{}", d.list.forecast_line());
    if let Some(t) = &d.list.trash {
        println!("{}", t.sentence());
    }
    println!();
    print_table(&shown, &d.paths.user_home);
    let more = d.list.items.len().saturating_sub(shown.len());
    if more > 0 {
        println!(
            "{more} smaller items not listed; --top {} lists them.",
            d.list.items.len()
        );
    }
    let mut tail = format!(
        "Cleaning up what allows it frees at least {}.",
        bytes(d.list.reclaimable)
    );
    if !d.config.destructive {
        tail.push_str(&format!(
            " Cleanup is off: set destructive = true in {} to allow it.",
            d.list.config
        ));
    }
    println!("{tail}");
    let s = d.reading.stats;
    println!(
        "Took {:.1} s: {} of {} parts from the cache, {} measured{}.",
        d.reading.timings.total_ms / 1000.0,
        s.fresh,
        s.parts,
        s.measured,
        if s.lost {
            " (FSEvents lost track, so all of them)"
        } else {
            ""
        }
    );
    Ok(())
}

fn print_table(items: &[&DiskItem], home: &Path) {
    if items.is_empty() {
        println!("Nothing in the catalog takes space here.");
        return;
    }
    let cells: Vec<[String; 5]> = items
        .iter()
        .map(|i| {
            [
                clip(&i.name, 34),
                i.kind.word().to_string(),
                i.size.map_or("-".into(), bytes),
                i.frees.map_or("-".into(), |b| {
                    format!("{}{}", if i.at_least { "≥ " } else { "" }, bytes(b))
                }),
                i.cleanup().to_string(),
            ]
        })
        .collect();
    let head = ["ITEM", "KIND", "SIZE", "FREES", "CLEANUP"];
    let mut width = head.map(str::len);
    for c in &cells {
        for (w, s) in width.iter_mut().zip(c) {
            *w = (*w).max(s.chars().count());
        }
    }
    let line = |c: &[&str]| {
        let mut out = String::new();
        for (i, (s, w)) in c.iter().zip(width).enumerate() {
            let pad = " ".repeat(w.saturating_sub(s.chars().count()));
            if i == 2 || i == 3 {
                out.push_str(&pad);
                out.push_str(s);
            } else {
                out.push_str(s);
                out.push_str(&pad);
            }
            out.push_str("  ");
        }
        out.trim_end().to_string()
    };
    println!("{}", line(&head));
    for (c, i) in cells.iter().zip(items) {
        println!("{}", printable(&line(&c.each_ref().map(String::as_str))));
        println!("    {}", printable(&shown_id(&i.id, home)));
    }
    println!();
}

/// An id with the home folder as `~`, which `scry clean` takes back.
fn shown_id(id: &str, home: &Path) -> String {
    let home = format!("{}/", home.display());
    id.replacen(&format!(":{home}"), ":~/", 1)
}

fn full_id(id: &str, home: &Path) -> String {
    id.replacen(":~/", &format!(":{}/", home.display()), 1)
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

pub fn clean(args: CleanArgs) -> Result<()> {
    let d = read()?;
    let home = d.paths.user_home.clone();
    let id = full_id(&args.item, &home);
    let Some(item) = d.reading.layout.items.iter().find(|i| i.id == id) else {
        let r = Refusal::NotFound { item: args.item };
        refuse(args.json, None, &r);
    };
    let plan = match clean::plan(item, d.config.destructive) {
        Ok(p) => p,
        Err(r) => refuse(args.json, None, &r),
    };
    let what = describe(&plan, &home);
    if let Some(w) = clean::writer_running(&plan.item) {
        eprintln!("{}", printable(&format!("Warning: {w}")));
    }
    let typed = match (args.yes, &plan.word) {
        (true, None) => None,
        (true, Some(_)) => args.confirm.clone(),
        (false, word) => {
            if !std::io::stdin().is_terminal() {
                let r = Refusal::Failed {
                    error: format!(
                        "would {what}. Pass --yes{} to confirm without a terminal",
                        if word.is_some() {
                            " and --confirm with its name"
                        } else {
                            ""
                        }
                    ),
                };
                refuse(args.json, Some(&plan), &r);
            }
            eprintln!("{}", printable(&format!("This will {what}.")));
            match word {
                Some(w) => eprint!("Type {w} to delete it for good: "),
                None => eprint!("Go ahead? [y/N] "),
            }
            std::io::stderr().flush()?;
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer)?;
            let answer = answer.trim().to_string();
            match word {
                Some(_) => Some(answer),
                None if matches!(answer.as_str(), "y" | "Y" | "yes") => None,
                None => refuse(
                    args.json,
                    Some(&plan),
                    &Refusal::Failed {
                        error: "declined; nothing was done".into(),
                    },
                ),
            }
        }
    };
    // Read again right before acting: the config may have changed while the prompt waited.
    let destructive = || clean::destructive_now(&d.paths);
    let ctx = Context {
        home: &home,
        store: d.cache.store(),
        actor: "cli",
        destructive: &destructive,
        label: &clean::relabel,
        moment: None,
    };
    match clean::execute(&plan, typed.as_deref(), &ctx) {
        Ok(done) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "plan": plan, "done": done, "message": done.sentence() })
                    )?
                );
            } else {
                println!("{}", printable(&done.sentence()));
            }
            Ok(())
        }
        Err(r) => refuse(args.json, Some(&plan), &r),
    }
}

/// "move ~/.codex/sessions (1.2 GB) to the Trash".
fn describe(plan: &Plan, home: &Path) -> String {
    let path = match plan.path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => plan.path.display().to_string(),
    };
    let size = bytes(plan.allocated);
    match plan.action {
        Action::Trash if plan.repo.is_some() => format!(
            "move the worktree's folder {path} ({size}) to the Trash; git keeps its record, so \
             Put Back in Finder gives a working checkout"
        ),
        Action::Trash => format!("move {path} ({size}) to the Trash"),
        Action::Delete => format!(
            "delete {path} ({size}) for good, freeing at least {}",
            bytes(plan.reclaimable)
        ),
        Action::Prune => format!(
            "remove {}, git's record of the missing worktree {path}, and nothing else; its \
             branch and every other worktree's record stay",
            plan.record
                .as_deref()
                .map_or(PathBuf::new(), Path::to_path_buf)
                .display()
        ),
    }
}

/// Says why nothing was done, or what was done before it stopped, as JSON on stdout or text
/// on stderr, and exits 1.
fn refuse(json: bool, plan: Option<&Plan>, r: &Refusal) -> ! {
    if json {
        let out = json!({
            "plan": plan,
            "refusal": r,
            "changed": r.changed(),
            "message": r.to_string()
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        eprintln!("{}", printable(&r.to_string()));
    }
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_print_with_a_tilde_and_read_back_whole() {
        let home = Path::new("/Users/dev");
        let id = "node_modules:/Users/dev/code/web/node_modules";
        assert_eq!(shown_id(id, home), "node_modules:~/code/web/node_modules");
        assert_eq!(full_id(&shown_id(id, home), home), id);
        assert_eq!(full_id("npm-cache", home), "npm-cache");
    }
}
