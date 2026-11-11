//! `scry doctor`: prints the checks from `scry_core::doctor`, one line each.

use anyhow::Result;
use scry_core::doctor::{self, Level};
use scry_core::paths::Paths;

/// Prints one line per check. Exits 1 when any check fails.
pub fn run(json: bool) -> Result<()> {
    let checks = doctor::run(&Paths::from_env()?);
    let healthy = doctor::healthy(&checks);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "ok": healthy, "checks": checks }))?
        );
    } else {
        for c in &checks {
            let (glyph, word) = match c.level {
                Level::Ok => ("✓", "ok"),
                Level::Warn => ("!", "warn"),
                Level::Fail => ("✗", "fail"),
            };
            let fix = c
                .fix
                .as_deref()
                .map(|f| format!(" Fix: {f}"))
                .unwrap_or_default();
            println!("{glyph} {word:<4}  {:<7}  {}.{fix}", c.name, c.detail);
        }
    }
    if !healthy {
        std::process::exit(1);
    }
    Ok(())
}
