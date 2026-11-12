//! `scry hook install claude`, `uninstall claude`, and `run`, which is what the installed
//! hooks execute.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Subcommand, ValueEnum};
use scry_core::catalog::{self, Role};
use scry_core::claude_hooks::{self, ManagedSources, Plan};
use scry_core::model::{Agent, ProcId};
use scry_core::paths::Paths;
use scry_core::session::{EventKind, SessionEvent, valid_id};
use scry_core::spool;
use scry_core::sys::{self, procinfo::ArgsReader};

#[derive(Subcommand)]
pub enum Action {
    /// Add Scry's SessionStart and SessionEnd hooks to the agent's settings, after showing
    /// the diff and backing the file up.
    Install {
        agent: HookAgent,
        /// Show the diff and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove Scry's hooks and nothing else, after showing the diff and backing the file up.
    Uninstall {
        agent: HookAgent,
        #[arg(long)]
        dry_run: bool,
    },
    /// What an installed hook runs. Reads the hook's JSON on stdin, appends one line to the
    /// spool, prints nothing and always exits 0.
    #[command(hide = true)]
    Run { agent: HookAgent, event: String },
}

/// Agents whose hook payloads Scry has checked. Codex and Gemini CLI follow once theirs are.
#[derive(Clone, Copy, ValueEnum)]
pub enum HookAgent {
    Claude,
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::Run { event, .. } => {
            // Whatever happens, the agent sees silence and success.
            let _ = record(&event);
            Ok(())
        }
        Action::Install { dry_run, .. } => change(true, dry_run),
        Action::Uninstall { dry_run, .. } => change(false, dry_run),
    }
}

fn settings() -> Result<PathBuf> {
    let home = std::env::home_dir()
        .filter(|h| !h.as_os_str().is_empty())
        .context("cannot tell the home directory")?;
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    Ok(claude_hooks::settings_path(&home, config_dir.as_deref()))
}

fn change(install: bool, dry_run: bool) -> Result<()> {
    let path = settings()?;
    let before = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let plan = if install {
        let user = std::env::var("USER").ok();
        let blockers = claude_hooks::managed_blockers(&ManagedSources::macos(user.as_deref()))
            .context("reading Claude Code's managed settings")?;
        if !blockers.is_empty() {
            for b in &blockers {
                eprintln!("{}", b.sentence());
            }
            bail!("not installed: managed settings would stop the hooks from running");
        }
        if let Err(why) = claude_hooks::check_exec_form(&claude_version()) {
            bail!("not installed: {why}");
        }
        // The Homebrew link rather than the Cellar path, so an upgrade keeps the hook working.
        let exe = std::env::current_exe()?;
        let exe = scry_core::launch_agent::stable_program(&exe)?;
        claude_hooks::plan_install(&path, before, &exe)?
    } else {
        claude_hooks::plan_uninstall(&path, before)?
    };
    show(&plan);
    if !plan.changes() {
        println!(
            "{} already {}.",
            path.display(),
            if install {
                "has Scry's hooks"
            } else {
                "has none of Scry's hooks"
            }
        );
        return Ok(());
    }
    if dry_run {
        println!("Dry run: {} is unchanged.", path.display());
        return Ok(());
    }
    let backup = plan.apply(sys::now_ms())?;
    if let Some(b) = backup {
        println!("Backed up the old file to {}.", b.display());
    }
    println!(
        "{} {}. Claude Code reloads its settings, so new sessions pick this up.",
        if install {
            "Installed hooks in"
        } else {
            "Removed Scry's hooks from"
        },
        path.display()
    );
    Ok(())
}

/// What `claude --version` prints, or why it printed nothing. `SCRY_CLAUDE` names another
/// `claude` to ask.
fn claude_version() -> String {
    let bin = std::env::var_os("SCRY_CLAUDE")
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "claude".into());
    let shown = PathBuf::from(&bin).display().to_string();
    let child = std::process::Command::new(&bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return format!("{shown} --version: {e}"),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return format!("{shown} --version did not answer");
            }
        }
    }
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    out
}

fn show(plan: &Plan) {
    if plan.changes() {
        println!("--- {}", plan.path.display());
        print!("{}", plan.diff());
    }
}

/// The hook body: one spool line, and for SessionStart the `SCRY_SESSION` stamp.
fn record(event: &str) -> Result<()> {
    let input = stdin_within(Duration::from_millis(500))?;
    let v: serde_json::Value = serde_json::from_slice(&input)?;
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let session_id = field("session_id")
        .filter(|s| valid_id(s))
        .context("session_id")?;
    let kind = match event {
        "SessionStart" => EventKind::Start {
            source: field("source").filter(|s| valid_id(s)),
        },
        "SessionEnd" => EventKind::End {
            reason: field("reason").filter(|s| valid_id(s)),
        },
        _ => bail!("not a hooked event"),
    };
    let e = SessionEvent {
        id: random_id()?,
        at_ms: sys::now_ms(),
        agent: Agent::ClaudeCode,
        session_id: session_id.clone(),
        kind,
        cwd: field("cwd").filter(|c| c.starts_with('/') && c.len() < 1024),
        agent_proc: agent_above(),
    };
    let paths = Paths::from_env()?;
    paths.ensure_state()?;
    spool::append(&paths.spool(), &e)?;
    if event == "SessionStart"
        && let Some(file) = std::env::var_os("CLAUDE_ENV_FILE")
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)?;
        writeln!(f, "export SCRY_SESSION={session_id}")?;
    }
    Ok(())
}

/// Reads stdin to its end, at most 1 MB, giving up after `limit`.
fn stdin_within(limit: Duration) -> Result<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let r = std::io::stdin().take(1 << 20).read_to_end(&mut buf);
        let _ = tx.send(r.map(|_| buf));
    });
    Ok(rx.recv_timeout(limit)??)
}

/// The Claude Code process the hook runs under: the nearest ancestor the catalog names as
/// Claude Code.
fn agent_above() -> Option<ProcId> {
    let mut reader = ArgsReader::new();
    let mut pid = std::os::unix::process::parent_id() as i32;
    for _ in 0..8 {
        if pid <= 1 {
            return None;
        }
        let exe = sys::exe_path(pid)?;
        let argv = reader.read(pid).map(|a| a.argv).unwrap_or_default();
        if catalog::classify(&exe, &argv).map(|r| r.role) == Some(Role::Agent(Agent::ClaudeCode)) {
            return sys::proc_id(pid);
        }
        pid = sys::ppid(pid)?;
    }
    None
}

fn random_id() -> Result<String> {
    let mut b = [0u8; 16];
    if unsafe { libc::getentropy(b.as_mut_ptr().cast(), b.len()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}
