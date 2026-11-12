//! `scry doctor`: one verdict per check, each with the fix in plain words.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;

use crate::config::{Config, DEFAULT_SAMPLE_INTERVAL_SECS, Source};
use crate::daemon::{Status, StoreStatus};
use crate::launch_agent::{Launchd, Spec, stable_program};
use crate::paths::Paths;
use crate::sys;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    pub fix: Option<String>,
}

fn ok(name: &'static str, detail: String) -> Check {
    Check {
        name,
        level: Level::Ok,
        detail,
        fix: None,
    }
}

fn warn(name: &'static str, detail: String, fix: impl Into<String>) -> Check {
    Check {
        name,
        level: Level::Warn,
        detail,
        fix: Some(fix.into()),
    }
}

fn fail(name: &'static str, detail: String, fix: impl Into<String>) -> Check {
    Check {
        name,
        level: Level::Fail,
        detail,
        fix: Some(fix.into()),
    }
}

pub fn run(paths: &Paths) -> Vec<Check> {
    let mut checks = vec![check_scan()];
    let config_interval = match Config::load(paths) {
        Ok((config, source)) => {
            checks.push(check_config(paths, &config, &source));
            config.daemon.sample_interval_secs
        }
        Err(e) => {
            checks.push(fail(
                "config",
                format!("{e}; until it is fixed, no process is selected as a leftover"),
                "Fix that line, or move the file away to use the defaults.",
            ));
            DEFAULT_SAMPLE_INTERVAL_SECS
        }
    };
    match Status::gather(paths) {
        Ok(status) => {
            checks.push(check_store(paths, &status.store));
            checks.push(check_daemon(paths, &status));
            if status.installed {
                checks.push(check_plist(paths, &status));
            }
            let interval = sample_interval(&status, config_interval);
            checks.push(check_samples(paths, &status, interval));
        }
        Err(e) => checks.push(fail(
            "daemon",
            format!("cannot read the daemon's state: {e:#}"),
            "Check that launchctl works in this session.",
        )),
    }
    checks
}

/// The period samples should arrive at: a running daemon keeps the interval it started
/// with, whatever the config says now.
fn sample_interval(s: &Status, config_interval: u64) -> Duration {
    Duration::from_secs(
        s.running
            .as_ref()
            .map_or(config_interval, |r| r.interval_secs),
    )
}

/// `Run \`scry daemon install\`<then>.`, unless a newer scry wrote the store, which
/// install refuses.
fn install_fix(s: &Status, then: &str) -> String {
    if s.store.too_new() {
        "Upgrade scry first: a newer scry wrote the store, and `scry daemon install` refuses it."
            .into()
    } else {
        format!("Run `scry daemon install`{then}.")
    }
}

fn check_scan() -> Check {
    const FIX: &str =
        "The kernel's process record changed shape in this macOS release. Update scry.";
    let snap = match sys::scan() {
        Ok(s) => s,
        Err(e) => return fail("scan", format!("{e:#}"), FIX),
    };
    let me = std::process::id() as i32;
    let age_us = |start_us: i64| sys::now_ms() * 1000 - start_us;
    let own_start_ok = snap
        .processes
        .iter()
        .find(|p| p.id.pid == me)
        .is_some_and(|p| (0..3_600_000_000).contains(&age_us(p.id.start_us)));
    let has_launchd = snap
        .processes
        .iter()
        .any(|p| p.id.pid == 1 && p.name == "launchd");
    if own_start_ok && has_launchd {
        ok(
            "scan",
            format!(
                "read {} processes; the kinfo_proc layout matches",
                snap.processes.len()
            ),
        )
    } else {
        fail(
            "scan",
            "pids or start times read from kinfo_proc do not add up".into(),
            FIX,
        )
    }
}

fn check_config(paths: &Paths, config: &Config, source: &Source) -> Check {
    let missing: Vec<String> = config
        .code_roots
        .iter()
        .filter(|r| !r.is_dir())
        .map(|r| paths.tilde(r))
        .collect();
    let origin = match source {
        Source::Defaults => "no config file, using defaults".to_string(),
        Source::File(p) => paths.tilde(p),
    };
    if !missing.is_empty() {
        return warn(
            "config",
            format!("{origin}; code root {} does not exist", missing.join(", ")),
            format!(
                "Remove it from code_roots in {}.",
                paths.tilde(&paths.config)
            ),
        );
    }
    let roots: Vec<String> = config.code_roots.iter().map(|r| paths.tilde(r)).collect();
    let roots = if roots.is_empty() {
        "no code roots".to_string()
    } else {
        format!("code roots {}", roots.join(", "))
    };
    ok("config", format!("{origin}; {roots}"))
}

fn check_store(paths: &Paths, store: &StoreStatus) -> Check {
    match store {
        StoreStatus::Missing { path } => warn(
            "store",
            format!("no store at {} yet", paths.tilde(path)),
            "Run `scry daemon install`, which creates it.",
        ),
        StoreStatus::Outdated { path, schema } => warn(
            "store",
            format!(
                "{} has schema {schema}, older than this scry's",
                paths.tilde(path)
            ),
            "Run `scry daemon install`; the daemon migrates it when it starts.",
        ),
        StoreStatus::Ready { path, schema, .. } => {
            ok("store", format!("{}, schema {schema}", paths.tilde(path)))
        }
        StoreStatus::Unreadable {
            message, too_new, ..
        } => fail(
            "store",
            message.clone(),
            if *too_new {
                "A newer scry wrote this store. Upgrade scry."
            } else {
                "Check the file's owner and mode, or move it away to start a new store."
            },
        ),
    }
}

fn check_daemon(paths: &Paths, s: &Status) -> Check {
    let log = paths.tilde(&paths.log());
    if s.installed && s.disabled {
        return fail(
            "daemon",
            "launchd has the service disabled (by `launchctl disable` or its switch in Login Items), so it does not start".into(),
            install_fix(s, ", which enables it again"),
        );
    }
    match (s.installed, s.launchd, &s.running) {
        (true, Launchd::Loaded { pid: Some(pid) }, Some(r)) if r.pid == pid => {
            if s.restart_needed.is_empty() {
                ok(
                    "daemon",
                    format!(
                        "running as pid {pid} under launchd, sampling every {}",
                        human_duration(Duration::from_secs(r.interval_secs))
                    ),
                )
            } else {
                warn(
                    "daemon",
                    format!(
                        "running as pid {pid}, but restart needed: {}",
                        s.restart_needed.join("; ")
                    ),
                    install_fix(s, ", which restarts it"),
                )
            }
        }
        (true, Launchd::Loaded { .. }, Some(r)) => warn(
            "daemon",
            format!(
                "pid {} holds the lock, so launchd's copy cannot start",
                r.pid
            ),
            format!("Stop pid {} and launchd takes over.", r.pid),
        ),
        (true, Launchd::Loaded { .. }, None) => fail(
            "daemon",
            "installed and loaded but not running".into(),
            format!("Read {log}, then run `scry daemon install` again."),
        ),
        (true, Launchd::NotLoaded, _) => fail(
            "daemon",
            "the LaunchAgent is on disk but launchd has not loaded it".into(),
            install_fix(s, ""),
        ),
        (false, Launchd::Loaded { .. }, _) => warn(
            "daemon",
            "launchd has the job loaded but its plist is gone".into(),
            "Run `scry daemon install` to restore it, or `scry daemon uninstall`.",
        ),
        (false, Launchd::NotLoaded, Some(r)) => warn(
            "daemon",
            format!("running in a terminal as pid {}, not installed", r.pid),
            install_fix(s, " so it survives logout"),
        ),
        (false, Launchd::NotLoaded, None) => warn(
            "daemon",
            "not installed, so disk history is not recorded".into(),
            install_fix(s, ""),
        ),
    }
}

/// `SCRY_HOME=/x`, or `empty`.
fn show_env(env: &BTreeMap<String, String>) -> String {
    if env.is_empty() {
        return "empty".into();
    }
    let pairs: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.join(" ")
}

fn check_plist(paths: &Paths, s: &Status) -> Check {
    let shown = paths.tilde(&s.plist);
    let rewrite = install_fix(s, " to rewrite it");
    let Some(contents) = &s.plist_contents else {
        return fail("plist", format!("plutil cannot read {shown}"), rewrite);
    };
    if contents.label.as_deref() != Some(s.label.as_str()) {
        return fail(
            "plist",
            format!(
                "its Label is {}, not {}",
                contents.label.as_deref().unwrap_or("missing"),
                s.label
            ),
            rewrite,
        );
    }
    let Some(program) = &s.program else {
        return fail("plist", format!("{shown} has no ProgramArguments"), rewrite);
    };
    let want = Spec::args(program);
    if contents.args != want {
        return fail(
            "plist",
            format!(
                "it runs `{}`, not `{}`",
                contents.args.join(" "),
                want.join(" ")
            ),
            rewrite,
        );
    }
    let want = Spec::env(paths.scry_home.as_deref());
    if contents.env != want {
        return fail(
            "plist",
            format!(
                "its environment is {}, but this scry needs {}",
                show_env(&contents.env),
                show_env(&want)
            ),
            rewrite,
        );
    }
    if let Some(mode) = s.plist_mode.filter(|m| m & 0o022 != 0) {
        return warn(
            "plist",
            format!("{shown} has mode {mode:04o}, so others can change what launchd runs as you"),
            install_fix(s, ", which sets it to 0644"),
        );
    }
    let shown = paths.tilde(program);
    if !s.program_exists {
        return fail(
            "plist",
            format!("it runs {shown}, which does not exist"),
            install_fix(s, " from the scry you want to keep"),
        );
    }
    let this = std::env::current_exe()
        .ok()
        .and_then(|e| stable_program(&e).ok());
    match this {
        Some(this) if this != *program => warn(
            "plist",
            format!("it runs {shown}, but this scry is {}", paths.tilde(&this)),
            install_fix(s, " from the scry the daemon should use"),
        ),
        _ => ok("plist", format!("runs {shown} daemon run")),
    }
}

fn check_samples(paths: &Paths, s: &Status, interval: Duration) -> Check {
    let log = paths.tilde(&paths.log());
    // Two missed periods plus a minute of slack for a Mac that just woke.
    let late = interval * 2 + Duration::from_secs(60);
    let (samples, newest) = match &s.store {
        StoreStatus::Ready {
            samples, newest, ..
        } => (*samples, newest),
        StoreStatus::Unreadable { too_new: true, .. } => {
            return warn(
                "samples",
                "cannot read disk samples: a newer scry wrote the store".into(),
                "Upgrade scry to read them.",
            );
        }
        StoreStatus::Unreadable { .. } => {
            return warn(
                "samples",
                "cannot read disk samples, since the store cannot be read".into(),
                "Fix the store first.",
            );
        }
        StoreStatus::Outdated { .. } | StoreStatus::Missing { .. } => {
            return warn(
                "samples",
                "no disk samples, since the store is not set up".into(),
                install_fix(s, ""),
            );
        }
    };
    let running_for = s
        .running
        .as_ref()
        .map(|r| sample_age(r.start_us.div_euclid(1000)));
    let Some(newest) = newest else {
        return match running_for {
            Some(up) if up > late => fail(
                "samples",
                format!(
                    "no disk samples, though the daemon has run for {}",
                    human_duration(up)
                ),
                format!("Read {log}; the sampler is failing."),
            ),
            Some(_) => warn(
                "samples",
                "no disk samples yet".into(),
                "The daemon samples when it starts; check again in a minute.",
            ),
            None => warn("samples", "no disk samples yet".into(), install_fix(s, "")),
        };
    };
    let ahead = Duration::from_millis((newest.at_ms - sys::now_ms()).max(0) as u64);
    if ahead > Duration::from_secs(60) {
        return warn(
            "samples",
            format!(
                "{samples} samples; the newest is dated {} in the future, so the clock moved back",
                human_duration(ahead)
            ),
            if running_for.is_some() {
                format!(
                    "Wait for the next sample, due within {}. If none comes, read {log}.",
                    human_duration(interval)
                )
            } else {
                install_fix(s, "")
            },
        );
    }
    let age = sample_age(newest.at_ms);
    let detail = format!(
        "{samples} samples; the newest is {} old",
        human_duration(age)
    );
    if age <= late {
        ok("samples", detail)
    } else if running_for.is_some() {
        fail(
            "samples",
            format!("{detail}, though the daemon is running"),
            format!("Read {log}; the sampler is failing."),
        )
    } else {
        warn(
            "samples",
            format!("{detail}; nothing is sampling now"),
            install_fix(s, ""),
        )
    }
}

/// How long ago `at_ms` was. Zero for a time in the future.
pub fn sample_age(at_ms: i64) -> Duration {
    Duration::from_millis((sys::now_ms() - at_ms).max(0) as u64)
}

/// `42 s`, `7 min`, `3 h`, `2 d`.
pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..120 => format!("{s} s"),
        120..7200 => format!("{} min", s / 60),
        7200..172_800 => format!("{} h", s / 3600),
        _ => format!("{} d", s / 86_400),
    }
}

/// True when every check passed or only warned.
pub fn healthy(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.level != Level::Fail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::Runtime;
    use crate::launch_agent::{LABEL, PlistContents};
    use crate::model::DiskSample;

    fn runtime(pid: i32, interval_secs: u64, up_secs: i64) -> Runtime {
        Runtime {
            pid,
            start_us: (sys::now_ms() - up_secs * 1000) * 1000,
            version: "0.0.0".into(),
            binary: None,
            config: "c".into(),
            interval_secs,
            scry_home: None,
        }
    }

    fn status(installed: bool, launchd: Launchd, running: Option<i32>) -> Status {
        Status {
            label: LABEL.into(),
            plist: "/p".into(),
            installed,
            plist_contents: None,
            plist_mode: None,
            disabled: false,
            launchd,
            running: running.map(|pid| runtime(pid, 600, 10)),
            program: None,
            program_exists: false,
            restart_needed: vec![],
            store: StoreStatus::Missing { path: "/db".into() },
        }
    }

    fn paths() -> Paths {
        Paths::new("/Users/dev".into(), None)
    }

    #[test]
    fn daemon_verdicts() {
        let loaded = |pid| Launchd::Loaded { pid };
        let level = |s: Status| check_daemon(&paths(), &s).level;
        assert_eq!(level(status(true, loaded(Some(5)), Some(5))), Level::Ok);
        assert_eq!(level(status(true, loaded(None), None)), Level::Fail);
        assert_eq!(level(status(true, Launchd::NotLoaded, None)), Level::Fail);
        assert_eq!(level(status(false, Launchd::NotLoaded, None)), Level::Warn);
        assert_eq!(
            level(status(false, Launchd::NotLoaded, Some(9))),
            Level::Warn
        );
        assert_eq!(level(status(true, loaded(None), Some(9))), Level::Warn);
    }

    #[test]
    fn a_disabled_service_fails_and_install_is_the_fix() {
        let mut s = status(true, Launchd::NotLoaded, None);
        s.disabled = true;
        let c = check_daemon(&paths(), &s);
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("disabled"), "{c:?}");
        assert!(c.fix.unwrap().contains("which enables it"));
    }

    #[test]
    fn a_changed_config_or_binary_asks_for_a_restart() {
        let mut s = status(true, Launchd::Loaded { pid: Some(5) }, Some(5));
        s.restart_needed = vec!["the sample interval changed from 600 s to 5 s".into()];
        let c = check_daemon(&paths(), &s);
        assert_eq!(c.level, Level::Warn);
        assert!(
            c.detail.contains("restart needed: the sample interval"),
            "{c:?}"
        );
        assert!(c.fix.unwrap().contains("which restarts it"));
    }

    fn with_store(mut s: Status, newest_age_s: Option<i64>) -> Status {
        s.store = StoreStatus::Ready {
            path: "/db".into(),
            schema: 1,
            samples: 3,
            newest: newest_age_s.map(|age| DiskSample {
                at_ms: sys::now_ms() - age * 1000,
                volume: "/".into(),
                total_bytes: 10,
                free_bytes: 5,
                avail_bytes: 4,
            }),
        };
        s
    }

    #[test]
    fn sample_freshness_depends_on_the_interval_and_the_daemon() {
        let check = |age_s: i64, running: Option<i32>, interval: u64| {
            let s = with_store(
                status(true, Launchd::Loaded { pid: running }, running),
                Some(age_s),
            );
            check_samples(&paths(), &s, Duration::from_secs(interval)).level
        };
        assert_eq!(check(30, Some(5), 600), Level::Ok);
        assert_eq!(check(1300, Some(5), 600), Level::Fail);
        assert_eq!(check(1300, None, 600), Level::Warn);
        assert_eq!(check(1300, Some(5), 3600), Level::Ok);
    }

    #[test]
    fn freshness_follows_the_running_daemons_interval_not_the_new_config() {
        // The daemon started with 600 s and sampled 15 minutes ago. The config now says
        // 5 s, which would call that sample late and blame the sampler.
        let mut s = with_store(
            status(true, Launchd::Loaded { pid: Some(5) }, Some(5)),
            Some(900),
        );
        s.running = Some(runtime(5, 600, 3600));
        let interval = sample_interval(&s, 5);
        assert_eq!(interval, Duration::from_secs(600));
        assert_eq!(check_samples(&paths(), &s, interval).level, Level::Ok);

        s.running = None;
        assert_eq!(sample_interval(&s, 5), Duration::from_secs(5));
    }

    #[test]
    fn a_daemon_without_samples_fails_after_two_intervals() {
        let check = |up_s: i64| {
            let mut s = with_store(
                status(true, Launchd::Loaded { pid: Some(5) }, Some(5)),
                None,
            );
            s.running = Some(runtime(5, 10, up_s));
            check_samples(&paths(), &s, Duration::from_secs(10))
        };
        assert_eq!(check(5).level, Level::Warn);
        let c = check(600);
        assert_eq!(c.level, Level::Fail, "{c:?}");
        assert!(c.detail.contains("has run for 10 min"), "{c:?}");
        assert!(!healthy(&[c]));
    }

    #[test]
    fn a_sample_from_the_future_is_flagged_not_fresh() {
        let s = with_store(
            status(true, Launchd::Loaded { pid: Some(5) }, Some(5)),
            Some(-3 * 3600),
        );
        let c = check_samples(&paths(), &s, Duration::from_secs(600));
        assert_eq!(c.level, Level::Warn, "{c:?}");
        assert!(c.detail.contains("in the future"), "{c:?}");
    }

    #[test]
    fn a_newer_store_never_suggests_install() {
        let mut s = status(false, Launchd::NotLoaded, None);
        s.store = StoreStatus::Unreadable {
            path: "/db".into(),
            message: "too new".into(),
            too_new: true,
        };
        let samples = check_samples(&paths(), &s, Duration::from_secs(600));
        let daemon = check_daemon(&paths(), &s);
        for c in [samples, daemon] {
            let fix = c.fix.unwrap();
            assert!(fix.starts_with("Upgrade scry"), "{}: {fix}", c.name);
        }
    }

    #[test]
    fn plist_check_reads_arguments_environment_and_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("scry");
        std::fs::write(&exe, "").unwrap();
        let home = Paths::new("/Users/dev".into(), Some("/tmp/h".into()));
        let good = || {
            let mut s = status(true, Launchd::Loaded { pid: Some(5) }, Some(5));
            s.program = Some(exe.clone());
            s.program_exists = true;
            s.plist_mode = Some(0o644);
            s.plist_contents = Some(PlistContents {
                label: Some(LABEL.into()),
                args: Spec::args(&exe),
                env: Spec::env(Some(std::path::Path::new("/tmp/h"))),
            });
            s
        };
        let verdict = |s: Status| check_plist(&home, &s);
        assert_ne!(verdict(good()).level, Level::Fail, "{:?}", verdict(good()));

        let mut s = good();
        s.plist_contents
            .as_mut()
            .unwrap()
            .args
            .push("--once".into());
        let c = verdict(s);
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("daemon run --once"), "{c:?}");

        let mut s = good();
        s.plist_contents.as_mut().unwrap().env = Spec::env(Some("/tmp/other".as_ref()));
        let c = verdict(s);
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("SCRY_HOME=/tmp/other"), "{c:?}");
        assert!(c.detail.contains("needs SCRY_HOME=/tmp/h"), "{c:?}");

        let c = check_plist(&paths(), &good());
        assert_eq!(c.level, Level::Fail, "no SCRY_HOME here");
        assert!(c.detail.contains("needs empty"), "{c:?}");

        let mut s = good();
        s.plist_mode = Some(0o664);
        let c = verdict(s);
        assert_eq!(c.level, Level::Warn);
        assert!(c.detail.contains("0664"), "{c:?}");

        let mut s = good();
        s.plist_contents.as_mut().unwrap().label = Some("other".into());
        assert_eq!(verdict(s).level, Level::Fail);
    }

    #[test]
    fn scan_check_passes_here() {
        let c = check_scan();
        assert_eq!(c.level, Level::Ok, "{c:?}");
    }

    #[test]
    fn human_duration_reads_naturally() {
        assert_eq!(human_duration(Duration::from_secs(42)), "42 s");
        assert_eq!(human_duration(Duration::from_secs(600)), "10 min");
        assert_eq!(human_duration(Duration::from_secs(3 * 3600)), "3 h");
        assert_eq!(human_duration(Duration::from_secs(3 * 86_400)), "3 d");
    }
}
