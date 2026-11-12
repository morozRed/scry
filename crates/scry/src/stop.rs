//! `scry stop`.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::Result;
use clap::Args;
use scry_core::control::{self, Outcome, StopTarget, TERM_GRACE, Target};
use scry_core::engine::{Engine, Labels};
use scry_core::label::printable;
use scry_core::paths::Paths;
use scry_core::reap;
use scry_core::reap::{StopJudge, StoreAudit};
use serde_json::{Value, json};

/// Stop one of your processes by pid or by the port it holds. Sends SIGTERM and waits 3 s.
/// Exits 0 when the process stopped, 1 when nothing was signalled, 2 when it is still
/// running, and 64 when the arguments are wrong. `destructive = false` in the config refuses.
#[derive(Args)]
pub struct StopArgs {
    /// A pid, or `:port`.
    target: Target,
    /// Send SIGKILL if the process is still running after SIGTERM.
    #[arg(long)]
    kill: bool,
    /// Do not ask for confirmation.
    #[arg(long)]
    yes: bool,
    /// Print JSON instead of text, refusals included.
    #[arg(long)]
    json: bool,
}

pub fn run(a: StopArgs) -> Result<()> {
    let labels = match a.target {
        Target::Pid(pid) => Labels::Pid(pid),
        Target::Port(_) => Labels::Listeners,
    };
    let snap = Engine::labelling(labels).snapshot()?;
    let t = match control::resolve(&snap, a.target) {
        Ok(t) => t,
        Err(r) => refuse(a.json, None, json!(r), &r.to_string()),
    };
    let what = printable(&t.described());
    let pid = t.id.pid;
    let paths = Paths::from_env()?;
    if let Err(r) = reap::allowed(&paths) {
        refuse(a.json, Some(&t), json!(r), &r.to_string());
    }
    if !a.yes {
        if !std::io::stdin().is_terminal() {
            refuse(
                a.json,
                Some(&t),
                json!({ "refused": "unconfirmed", "pid": pid }),
                &format!(
                    "Would stop pid {pid} ({what}). Pass --yes to confirm without a terminal."
                ),
            );
        }
        eprint!("Stop pid {pid} ({what}) with SIGTERM? [y/N] ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            refuse(
                a.json,
                Some(&t),
                json!({ "refused": "declined", "pid": pid }),
                "Nothing was stopped.",
            );
        }
    }
    let action = if a.kill { "stop --kill" } else { "stop" };
    let mut audit = match StoreAudit::open(&paths, "cli", action) {
        Ok(s) => s,
        Err(r) => refuse(a.json, Some(&t), json!(r), &r.to_string()),
    };
    // The judge reads the config and the process again before each signal, so an answer
    // that took minutes, or a `destructive = false` saved meanwhile, is seen.
    let done = reap::stop_one(
        &t,
        a.kill,
        TERM_GRACE,
        &mut StopJudge::new(paths.clone()),
        &mut audit,
    );
    if let Some(e) = &done.unaudited {
        eprintln!("The outcome was sent but its audit row was not written: {e}");
    }
    let outcome = match (done.outcome, done.refusal) {
        (Some(o), _) => o,
        (None, Some(r)) => refuse(a.json, Some(&t), json!(r), &r.to_string()),
        (None, None) => unreachable!("a stop either acts or says why not"),
    };
    if a.json {
        let out = json!({ "target": t, "outcome": outcome });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        match &outcome {
            Outcome::Exited { signal, after_ms } => println!(
                "Stopped pid {pid} ({what}) with {signal} after {:.1} s.",
                *after_ms as f64 / 1000.0
            ),
            Outcome::StillRunning { after_ms } => println!(
                "pid {pid} ({what}) is still running {:.1} s after SIGTERM. Run `scry stop {pid} --kill` to send SIGKILL.",
                *after_ms as f64 / 1000.0,
            ),
            Outcome::KillRefused { why, .. } => {
                println!("pid {pid} ({what}) is still running; SIGKILL was not sent: {why}")
            }
        }
    }
    if matches!(
        outcome,
        Outcome::StillRunning { .. } | Outcome::KillRefused { .. }
    ) {
        std::process::exit(2);
    }
    Ok(())
}

/// Says why nothing was signalled, as JSON on stdout or text on stderr, and exits 1.
fn refuse(json: bool, target: Option<&StopTarget>, refusal: Value, message: &str) -> ! {
    if json {
        let out = json!({ "target": target, "refusal": refusal, "message": message });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        eprintln!("{}", printable(message));
    }
    std::process::exit(1);
}
