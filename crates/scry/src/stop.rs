//! `scry stop`.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::Result;
use clap::Args;
use scry_core::control::{self, Outcome, StopTarget, TERM_GRACE, Target};
use scry_core::engine::Engine;
use scry_core::label::printable;
use scry_core::view::ports::clip;
use serde_json::{Value, json};

/// Stop one of your processes by pid or by the port it holds. Sends SIGTERM and waits 3 s.
/// Exits 0 when the process stopped, 1 when nothing was signalled, 2 when it is still
/// running, and 64 when the arguments are wrong.
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
    let snap = Engine::new().snapshot()?;
    let t = match control::resolve(&snap, a.target) {
        Ok(t) => t,
        Err(r) => refuse(a.json, None, json!(r), &r.to_string()),
    };
    let what = clip(&printable(&t.process), 120);
    let pid = t.id.pid;
    if !a.yes {
        if !std::io::stdin().is_terminal() {
            refuse(
                a.json,
                Some(&t),
                json!({ "refused": "unconfirmed", "pid": pid }),
                &format!(
                    "Would stop pid {pid} (`{what}`). Pass --yes to confirm without a terminal."
                ),
            );
        }
        eprint!("Stop pid {pid} (`{what}`) with SIGTERM? [y/N] ");
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
    let outcome = match control::stop(&t, a.kill, TERM_GRACE) {
        Ok(o) => o,
        Err(r) => refuse(a.json, Some(&t), json!(r), &r.to_string()),
    };
    if a.json {
        let out = json!({ "target": t, "outcome": outcome });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        match &outcome {
            Outcome::Exited { signal, after_ms } => println!(
                "Stopped pid {pid} (`{what}`) with {signal} after {:.1} s.",
                *after_ms as f64 / 1000.0
            ),
            Outcome::StillRunning { after_ms } => println!(
                "pid {pid} (`{what}`) is still running {:.1} s after SIGTERM. Run `scry stop {pid} --kill` to send SIGKILL.",
                *after_ms as f64 / 1000.0,
            ),
        }
    }
    if matches!(outcome, Outcome::StillRunning { .. }) {
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
