//! `scry reap` and `scry keep`.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::Result;
use clap::Args;
use scry_core::control::{Refusal, TERM_GRACE};
use scry_core::engine::Labels;
use scry_core::keep::{self, KeepAs};
use scry_core::label::printable;
use scry_core::live::Collector;
use scry_core::paths::Paths;
use scry_core::reap::{self, LiveJudge, ReapTarget, Report, StoreAudit};
use scry_core::view::agents::{Leftover, PERSISTENT};
use serde_json::{Value, json};

/// Stop the processes `scry leftovers` lists, children first. Each is checked again right
/// before its signal, with the config and the machine read afresh: stopping still allowed,
/// still a leftover of the same session, still the same process. Each signal is written to
/// the audit table before it is sent. Sends SIGTERM and waits up to 3 s for each. Exits 0
/// when they stopped or there was nothing to reap, 1 when nothing was signalled, 2 when one
/// is still running, and 64 when the arguments are wrong.
#[derive(Args)]
pub struct ReapArgs {
    /// List what would be reaped and signal nothing.
    #[arg(long)]
    dry_run: bool,
    /// Do not ask for confirmation.
    #[arg(long)]
    yes: bool,
    /// Send SIGKILL to any still running after SIGTERM, after the same checks.
    #[arg(long)]
    kill: bool,
    /// Only this session's leftovers: its full id, or the start of one no other session's
    /// id starts with.
    #[arg(long)]
    session: Option<String>,
    /// Print JSON instead of text, refusals included.
    #[arg(long)]
    json: bool,
}

pub fn run(a: ReapArgs) -> Result<()> {
    let paths = Paths::from_env()?;
    let mut collector = Collector::with_paths(Labels::All, Some(paths.clone()));
    // Without the keep list the rule would select what the user asked to keep.
    if let Some(e) = collector.policy().config_error.clone() {
        refuse(
            a.json,
            &[],
            json!({ "refused": "config_unread", "error": e }),
            &format!(
                "{e}\nScry selects no leftovers until the config is fixed; `scry doctor` checks it."
            ),
        );
    }
    let r = collector.read()?;
    let home = collector.home().cloned();
    let list = match reap::plan(&r.snap, &r.lineage, home.as_ref(), a.session.as_deref()) {
        Ok(list) => list,
        // Nothing to reap, as for a session that left nothing; it says which.
        Err(r @ Refusal::NoSession { .. }) => {
            if a.json {
                print_json(&json!({
                    "dry_run": a.dry_run,
                    "targets": [],
                    "report": Report::default(),
                    "message": r.to_string(),
                }));
            } else {
                println!("{r} Nothing to reap.");
            }
            return Ok(());
        }
        Err(r) => refuse(a.json, &[], json!(r), &r.to_string()),
    };
    let allowed = reap::allowed(&paths);

    if list.is_empty() {
        if a.json {
            print_json(
                &json!({ "dry_run": a.dry_run, "targets": list, "report": Report::default() }),
            );
        } else {
            println!("No leftovers to reap.");
        }
        return Ok(());
    }
    if a.dry_run {
        if a.json {
            print_json(&json!({
                "dry_run": true,
                "targets": list,
                "disabled": allowed.as_ref().err().map(|r| r.to_string()),
            }));
        } else {
            print!("{}", crate::agents::leftover_table(&list));
            println!("Dry run: nothing was signalled. `scry reap` stops these, children first.");
            println!("{PERSISTENT}");
            if let Err(r) = &allowed {
                println!("{r}");
            }
        }
        return Ok(());
    }
    if let Err(r) = allowed {
        refuse(a.json, &list, json!(r), &r.to_string());
    }
    let n = list.len();
    let what = if n == 1 {
        "1 leftover".to_string()
    } else {
        format!("{n} leftovers")
    };
    if !a.yes {
        if !std::io::stdin().is_terminal() {
            refuse(
                a.json,
                &list,
                json!({ "refused": "unconfirmed" }),
                &format!("Would reap {what}. Pass --yes to confirm without a terminal."),
            );
        }
        eprint!("{}", crate::agents::leftover_table(&list));
        eprint!("Reap {what}, children first, with SIGTERM? [y/N] ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            refuse(
                a.json,
                &list,
                json!({ "refused": "declined" }),
                "Nothing was reaped.",
            );
        }
    }
    let action = if a.kill { "reap --kill" } else { "reap" };
    let mut audit = match StoreAudit::open(&paths, "cli", action) {
        Ok(s) => s,
        Err(r) => refuse(a.json, &list, json!(r), &r.to_string()),
    };
    let targets: Vec<ReapTarget> = list.iter().map(ReapTarget::from).collect();
    // The judge reads the config again before each signal, so an answer that took minutes,
    // or a `destructive = false` saved meanwhile, is seen.
    let report = reap::run(
        &targets,
        a.kill,
        TERM_GRACE,
        &mut LiveJudge::new(paths.clone()),
        &mut audit,
        &mut |d| {
            if !a.json {
                println!(
                    "pid {} `{}`: {}",
                    d.target.id.pid,
                    printable(&d.target.command),
                    printable(&d.result())
                );
            }
        },
    );
    let unaudited: Vec<String> = report
        .unaudited()
        .iter()
        .map(|d| {
            format!(
                "pid {}: {}",
                d.target.id.pid,
                d.unaudited.as_deref().unwrap_or_default()
            )
        })
        .collect();
    if a.json {
        print_json(&json!({
            "dry_run": false,
            "targets": list,
            "report": report,
            "summary": report.summary(),
            "unaudited": unaudited,
        }));
    } else {
        println!("{}", report.summary());
        if !report.still_running().is_empty() && !a.kill {
            println!("Run `scry reap --kill` to send SIGKILL to those still running.");
        }
        for u in &unaudited {
            eprintln!("The outcome was sent but its audit row was not written for {u}");
        }
    }
    std::process::exit(report.exit_code());
}

/// Says why nothing was signalled, as JSON on stdout or text on stderr, and exits 1.
fn refuse(json: bool, targets: &[Leftover], refusal: Value, message: &str) -> ! {
    if json {
        print_json(&json!({ "targets": targets, "refusal": refusal, "message": message }));
    } else {
        eprintln!("{}", printable(message));
    }
    std::process::exit(1);
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

/// Add a process to the keep list in the config, so no rule selects it again. A program is
/// kept by its executable's path; a script run by an interpreter or shell (node, python,
/// bash, …) by the script's real path, with the working directory when the script lives
/// outside it. Code that is not a file, such as `python3 -c`, has no entry that would keep
/// only it, so nothing is written. Allowed under `destructive = false`, and audited. Prints
/// the change.
#[derive(Args)]
pub struct KeepArgs {
    /// The pid of the process to keep.
    pid: i32,
    /// Show the change and write nothing.
    #[arg(long)]
    dry_run: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
}

pub fn keep(a: KeepArgs) -> Result<()> {
    let paths = Paths::from_env()?;
    let snap = scry_core::engine::Engine::labelling(Labels::Pid(a.pid)).snapshot()?;
    let Some(p) = snap.process(a.pid).filter(|p| p.uid == snap.viewer_uid) else {
        let r = Refusal::NotFound {
            target: format!("pid {}", a.pid),
        };
        if a.json {
            print_json(&json!({ "refusal": r, "message": r.to_string() }));
        } else {
            eprintln!("{r} Scry keeps only your own processes.");
        }
        std::process::exit(1);
    };
    let entry = match keep::entry_for(p, Some(&paths.user_home)) {
        KeepAs::Entry(e) => e,
        KeepAs::Refused(why) => {
            if a.json {
                print_json(&json!({
                    "refusal": { "refused": "no_stable_entry" },
                    "message": why,
                }));
            } else {
                eprintln!("{}", printable(&why));
            }
            std::process::exit(1);
        }
    };
    let edit = keep::plan(&paths, &entry)?;
    if !a.dry_run {
        keep::apply_audited(&edit, &paths, "cli")?;
    }
    if a.json {
        print_json(&json!({ "dry_run": a.dry_run, "edit": edit, "diff": edit.diff() }));
        return Ok(());
    }
    let file = paths.tilde(&edit.config);
    if edit.already {
        println!(
            "`{}` is already on the keep list in {file}.",
            printable(&entry)
        );
        return Ok(());
    }
    let done = if a.dry_run { "Would change" } else { "Changed" };
    println!("{done} {file} line {}:", edit.line);
    for l in edit.diff() {
        println!("  {}", printable(&l));
    }
    println!(
        "No rule selects pid {} or anything else that matches `{}` again.",
        a.pid,
        printable(&entry)
    );
    Ok(())
}
