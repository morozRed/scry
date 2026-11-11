//! `scry ports` and `scry why`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use scry_core::engine::Engine;
use scry_core::label::{printable, unprintable};
use scry_core::sys::sockets::pid_holds_port;
use scry_core::view::human_age;
use scry_core::view::ports::{PortRow, PortsView, Why, clip};
use scry_core::{ProcId, Snapshot};
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
    /// Also print each of your holders' full command line, which can hold secrets. Text only:
    /// it is never part of `--json` or a snapshot.
    #[arg(long, conflicts_with_all = ["json", "fixture"])]
    show_command: bool,
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
                clip(&r.process, 40),
                r.owner.clone(),
                r.project.clone().unwrap_or_default(),
                r.age_secs
                    .map(|s| human_age(s.max(0) as u64))
                    .unwrap_or_default(),
            ]
            .map(|cell| printable(&cell))
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
    let mut engine = Engine::new();
    let snap = match a.fixture {
        Some(path) => fixture::load(&path)?,
        None => engine.snapshot()?,
    };
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
    println!("{}", printable(&why.sentence));
    for h in &why.holders {
        println!();
        if why.holders.len() > 1 {
            let process = printable(&h.row.process);
            println!("{process} (pid {}, {}):", h.row.pid, h.row.label());
        }
        println!("Evidence:");
        for e in &h.evidence {
            println!("  - {}", printable(&e.detail));
        }
        if a.show_command {
            let raw = h.row.start_us.and_then(|start_us| {
                engine.command_line(ProcId {
                    pid: h.row.pid,
                    start_us,
                })
            });
            match raw {
                Some(argv) => println!("Command line: {}", shell_line(argv)),
                None => println!("Command line: not readable."),
            }
        }
    }
    Ok(())
}

/// One line a person can read: arguments with spaces, quotes or shell characters are
/// single-quoted, and control and text-direction characters are escaped so they cannot
/// drive the terminal or disguise the line.
fn shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let escaped: String = a
                .chars()
                .map(|c| {
                    if unprintable(c) {
                        c.escape_default().to_string()
                    } else {
                        c.to_string()
                    }
                })
                .collect();
            let plain = !a.is_empty()
                && a.chars()
                    .all(|c| c.is_alphanumeric() || "-_./=:,@%+".contains(c));
            if plain {
                escaped
            } else {
                format!("'{}'", escaped.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::{shell_line, table};
    use scry_core::fixture;
    use scry_core::view::ports::PortsView;

    #[test]
    fn names_from_the_system_cannot_drive_the_terminal() {
        let mut snap = fixture::parse(include_str!("../../../fixtures/typical.json")).unwrap();
        for p in &mut snap.processes {
            p.name = format!("{}\u{1b}]0;x\u{7}\u{202e}", p.name);
            p.command = None;
        }
        let out = table(&PortsView::build(&snap, None).rows);
        assert!(out.contains("launchd?]0;x??"), "{out}");
        assert!(!out.contains(['\u{1b}', '\u{7}', '\u{202e}']), "{out}");
    }

    #[test]
    fn a_command_line_reads_back_with_quotes_and_no_terminal_controls() {
        let argv = [
            "node",
            "a b",
            "it's",
            "",
            "--x=1",
            "\u{1b}[2J\n",
            "a\u{202e}b",
        ]
        .map(String::from);
        assert_eq!(
            shell_line(&argv),
            r"node 'a b' 'it'\''s' '' --x=1 '\u{1b}[2J\n' 'a\u{202e}b'"
        );
    }
}
