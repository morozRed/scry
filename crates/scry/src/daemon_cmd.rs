//! `scry daemon install|uninstall|status|run`: argument parsing and printing. The work is in
//! `scry_core::daemon`.

use std::time::Duration;

use anyhow::Result;
use clap::Subcommand;
use scry_core::daemon::{self, Status, StoreStatus};
use scry_core::daemon_log;
use scry_core::doctor::{human_duration, sample_age};
use scry_core::launch_agent::{LaunchAgent, Launchd, Step};
use scry_core::paths::Paths;
use scry_core::sys;
use scry_core::view::disk_bytes;

#[derive(Subcommand)]
pub enum Action {
    /// Install the LaunchAgent and start the daemon, or restart it if its binary or config
    /// changed. Running it again converges.
    Install,
    /// Stop the daemon and remove the LaunchAgent. The store stays.
    Uninstall,
    /// Say whether the daemon is installed, loaded and running, and how fresh its samples are.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Run the daemon in the foreground. The LaunchAgent runs this.
    Run,
}

pub fn run(action: Action) -> Result<()> {
    let paths = Paths::from_env()?;
    match action {
        Action::Install => install(&paths),
        Action::Uninstall => uninstall(&paths),
        Action::Status { json } => status(&paths, json),
        Action::Run => {
            // Under launchd stderr is the log, so the error gets a timestamp like every line.
            if let Err(e) = daemon::run(&paths) {
                daemon_log::line(&format!("error: {e:#}"));
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

fn install(paths: &Paths) -> Result<()> {
    let done = daemon::install(paths, &std::env::current_exe()?)?;
    if done.steps.is_empty() {
        println!(
            "Already installed and running as pid {}. Nothing changed.",
            done.pid
        );
    } else {
        println!("{}.", describe(&done.steps, &done.restarted_because, paths));
        println!(
            "Running as pid {}: {} daemon run",
            done.pid,
            paths.tilde(&done.program)
        );
    }
    for w in &done.warnings {
        eprintln!("warning: {w}");
    }
    Ok(())
}

fn uninstall(paths: &Paths) -> Result<()> {
    let done = daemon::uninstall(paths)?;
    if done.steps.is_empty() {
        println!("Not installed. Nothing changed.");
    } else {
        println!("{}.", describe(&done.steps, &[], paths));
        println!("The store stays in {}.", paths.tilde(&paths.state));
    }
    for w in &done.warnings {
        eprintln!("warning: {w}");
    }
    Ok(())
}

fn describe(steps: &[Step], restarted_because: &[String], paths: &Paths) -> String {
    let plist = paths.tilde(&LaunchAgent::new(paths).plist);
    let words: Vec<String> = steps
        .iter()
        .map(|s| match s {
            Step::Enable => "enabled the service in launchd, which had it disabled".to_string(),
            Step::Bootout => "stopped and unloaded the job".to_string(),
            Step::WritePlist => format!("wrote {plist}"),
            Step::FixPlistMode => format!("set {plist} to mode 0644"),
            Step::RemovePlist => format!("removed {plist}"),
            Step::Bootstrap => "loaded it into launchd".to_string(),
            Step::Kickstart => "started the loaded job".to_string(),
            Step::Restart => format!(
                "restarted the job because {}",
                restarted_because.join(" and ")
            ),
        })
        .collect();
    let text = words.join(", ");
    match text.get(..1) {
        Some(first) => first.to_uppercase() + &text[1..],
        None => text,
    }
}

fn status(paths: &Paths, json: bool) -> Result<()> {
    let s = Status::gather(paths)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }
    let yes_no = |b: bool| if b { "yes" } else { "no " };
    println!(
        "installed  {}  {}",
        yes_no(s.installed),
        paths.tilde(&s.plist)
    );
    if s.disabled {
        println!("enabled    no   launchd will not load it; `scry daemon install` enables it");
    }
    let loaded = matches!(s.launchd, Launchd::Loaded { .. });
    println!(
        "loaded     {}  gui/{}/{}",
        yes_no(loaded),
        unsafe { libc::getuid() },
        s.label
    );
    let running = match (&s.running, s.launchd) {
        (Some(r), Launchd::Loaded { pid: Some(p) }) if r.pid == p => format!(
            "yes  pid {p}, under launchd, every {}",
            human_duration(Duration::from_secs(r.interval_secs))
        ),
        (Some(r), _) => format!("yes  pid {}, not started by launchd", r.pid),
        (None, _) => "no".into(),
    };
    println!("running    {running}");
    if s.running.is_some() {
        if s.restart_needed.is_empty() {
            println!("restart    not needed");
        } else {
            println!(
                "restart    needed: {}. Run `scry daemon install`.",
                s.restart_needed.join("; ")
            );
        }
    }
    match &s.program {
        Some(p) if s.program_exists => println!("binary     {}", paths.tilde(p)),
        Some(p) => println!("binary     {} (missing)", paths.tilde(p)),
        None => println!("binary     -"),
    }
    match &s.store {
        StoreStatus::Missing { path } => println!("store      none yet at {}", paths.tilde(path)),
        StoreStatus::Outdated { path, schema } => println!(
            "store      {}, schema {schema}; the daemon migrates it when it starts",
            paths.tilde(path)
        ),
        StoreStatus::Unreadable { message, .. } => println!("store      {message}"),
        StoreStatus::Ready {
            path,
            schema,
            samples,
            newest,
        } => {
            println!("store      {}, schema {schema}", paths.tilde(path));
            match newest {
                Some(n) => {
                    let ahead_ms = n.at_ms - sys::now_ms();
                    let when = if ahead_ms > 60_000 {
                        let ahead = Duration::from_millis(ahead_ms as u64);
                        format!("dated {} in the future", human_duration(ahead))
                    } else {
                        format!("{} ago", human_duration(sample_age(n.at_ms)))
                    };
                    println!(
                        "samples    {samples}, newest {when} ({} {} available of {})",
                        n.volume,
                        disk_bytes(n.important_bytes.unwrap_or(n.avail_bytes)),
                        disk_bytes(n.total_bytes)
                    )
                }
                None => println!("samples    0"),
            }
        }
    }
    Ok(())
}
