//! `scry ports` and `scry why`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use scry_core::Snapshot;
use scry_core::engine::Engine;
use scry_core::ports::{PortRow, PortsView, Why, clip, human_age};
use scry_core::sys::sockets::pid_holds_port;
use scry_core::{fixture, model::Proto};

/// List listening ports with their process, project and owner.
#[derive(Args)]
pub struct PortsArgs {
    /// Print JSON instead of a table.
    #[arg(long)]
    json: bool,
    /// Read a recorded snapshot instead of this machine.
    #[arg(long)]
    fixture: Option<PathBuf>,
}

/// Explain who holds a port, with the evidence.
#[derive(Args)]
pub struct WhyArgs {
    /// The port, as `3000` or `:3000`.
    #[arg(value_parser = parse_port)]
    port: u16,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    /// Read a recorded snapshot instead of this machine.
    #[arg(long)]
    fixture: Option<PathBuf>,
}

fn parse_port(s: &str) -> Result<u16, String> {
    s.trim_start_matches(':')
        .parse()
        .map_err(|_| format!("`{s}` is not a port"))
}

pub(crate) fn snapshot(fixture: Option<PathBuf>) -> Result<Snapshot> {
    match fixture {
        Some(path) => fixture::load(&path),
        None => Engine::new().snapshot(),
    }
}

pub(crate) fn home() -> Option<String> {
    fixture::home_dir().ok()
}

pub fn ports(a: PortsArgs) -> Result<()> {
    let snap = snapshot(a.fixture)?;
    let view = PortsView::build(&snap, home().as_deref());
    if a.json {
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }
    if view.rows.is_empty() {
        println!("Nothing is listening.");
        return Ok(());
    }
    print!("{}", table(&view.rows));
    Ok(())
}

fn table(rows: &[PortRow]) -> String {
    let head = [
        "PORT", "ADDRESS", "PID", "PROCESS", "OWNER", "PROJECT", "AGE",
    ];
    let cells: Vec<[String; 7]> = rows
        .iter()
        .map(|r| {
            [
                r.label(),
                r.addresses.join(", "),
                r.pid.to_string(),
                clip(&r.process, 28),
                r.owner.clone(),
                r.project.clone().unwrap_or_default(),
                r.age_secs.map(human_age).unwrap_or_default(),
            ]
        })
        .collect();
    let mut width = head.map(str::len);
    for c in &cells {
        for (w, s) in width.iter_mut().zip(c) {
            *w = (*w).max(s.chars().count());
        }
    }
    let line = |c: &[String]| {
        let parts: Vec<String> = c
            .iter()
            .zip(width)
            .map(|(s, w)| format!("{s:<w$}"))
            .collect();
        parts.join("  ").trim_end().to_string() + "\n"
    };
    let mut out = line(&head.map(String::from));
    for c in &cells {
        out.push_str(&line(c));
    }
    out
}

pub fn why(a: WhyArgs) -> Result<()> {
    let live = a.fixture.is_none();
    let snap = snapshot(a.fixture)?;
    let mut why = Why::build(&snap, a.port, home().as_deref());
    if live {
        let checks: Vec<(i32, Proto)> = why
            .holders
            .iter()
            .filter(|h| h.row.uid == snap.viewer_uid)
            .map(|h| (h.row.pid, h.row.proto))
            .collect();
        for (pid, proto) in checks {
            why.confirm(pid, proto, pid_holds_port(pid, proto, a.port));
        }
    }
    if a.json {
        println!("{}", serde_json::to_string_pretty(&why)?);
        return Ok(());
    }
    println!("{}", why.sentence);
    for h in &why.holders {
        println!();
        if why.holders.len() > 1 {
            println!("{} (pid {}, {}):", h.row.process, h.row.pid, h.row.label());
        }
        println!("Evidence:");
        for e in &h.evidence {
            println!("  - {}", e.detail);
        }
    }
    Ok(())
}
