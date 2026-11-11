//! `scry daemon run`, `status`, `uninstall` and `scry doctor` as a user runs them, under a
//! throwaway `SCRY_HOME`. Nothing here installs a LaunchAgent.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use scry_core::launch_agent::LABEL;
use scry_core::store::{SCHEMA_VERSION, Store};

fn scry(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_scry"));
    c.env("SCRY_HOME", home);
    c
}

fn wait_until<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn exit_within(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    wait_until("exit", timeout, || child.try_wait().unwrap())
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(out)))
}

fn check<'a>(doctor: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no {name} check in {doctor}"))
}

/// `2026-11-11T17:04:05Z scry daemon: ...`
fn stamped(line: &str) -> bool {
    line.split_at_checked(20).is_some_and(|(t, rest)| {
        t.as_bytes()[4] == b'-'
            && t.as_bytes()[10] == b'T'
            && t.ends_with('Z')
            && rest.starts_with(" scry daemon: ")
    })
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn daemon_samples_refuses_a_second_copy_and_stops_on_sigterm() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let config = home.join("config.toml");
    std::fs::write(&config, "[daemon]\nsample_interval_secs = 1\n").unwrap();
    let db = home.join("state/scry.db");

    let mut first = scry(home)
        .args(["daemon", "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let count = || -> Option<u64> {
        let n = Store::open_read_only(&db).ok()?.sample_count().ok()?;
        (n >= 3).then_some(n)
    };
    wait_until("three samples", Duration::from_secs(10), count);

    let second = scry(home).args(["daemon", "run"]).output().unwrap();
    assert_eq!(second.status.code(), Some(1), "{}", text(&second));
    let err = String::from_utf8_lossy(&second.stderr).into_owned();
    assert!(stamped(err.trim_end()), "{err}");
    assert!(
        err.contains(&format!(
            "error: another scry daemon is already running as pid {}",
            first.id()
        )),
        "{err}"
    );

    // A scratch SCRY_HOME has its own label, and the lock records what the daemon runs with.
    let status = json(
        &scry(home)
            .args(["daemon", "status", "--json"])
            .output()
            .unwrap(),
    );
    let label = status["label"].as_str().unwrap();
    assert!(label.starts_with(&format!("{LABEL}.")), "{label}");
    assert!(
        status["plist"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/Library/LaunchAgents/{label}.plist"))
    );
    assert_eq!(status["installed"], false);
    assert_eq!(status["running"]["pid"], first.id());
    assert_eq!(status["running"]["interval_secs"], 1);
    assert_eq!(status["running"]["scry_home"], home.to_str().unwrap());
    assert_eq!(
        status["running"]["binary"]["path"],
        std::fs::canonicalize(env!("CARGO_BIN_EXE_scry"))
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(status["restart_needed"], serde_json::json!([]));

    // A new interval is not applied until a restart, and status says so.
    std::fs::write(&config, "[daemon]\nsample_interval_secs = 2\n").unwrap();
    let status = scry(home).args(["daemon", "status"]).output().unwrap();
    assert!(
        text(&status).contains(&format!(
            "running    yes  pid {}, not started by launchd",
            first.id()
        )),
        "{}",
        text(&status)
    );
    assert!(
        text(&status).contains(
            "restart    needed: the sample interval changed from 1 s to 2 s. Run `scry daemon install`."
        ),
        "{}",
        text(&status)
    );
    std::fs::write(
        &config,
        "# same settings\n[daemon]\nsample_interval_secs = 1\n",
    )
    .unwrap();
    let status = scry(home).args(["daemon", "status"]).output().unwrap();
    assert!(
        text(&status).contains("restart    not needed"),
        "{}",
        text(&status)
    );

    let doctor = json(&scry(home).args(["doctor", "--json"]).output().unwrap());
    assert_eq!(check(&doctor, "samples")["level"], "ok", "{doctor}");

    unsafe { libc::kill(first.id() as i32, libc::SIGTERM) };
    let exit = exit_within(&mut first, Duration::from_secs(5));
    assert!(exit.success(), "clean exit on SIGTERM: {exit}");
    let mut log = String::new();
    std::io::Read::read_to_string(&mut first.stderr.take().unwrap(), &mut log).unwrap();
    assert!(log.contains("started as pid"), "{log}");
    assert!(log.contains("stopped by signal 15"), "{log}");
    assert!(
        log.lines().all(stamped),
        "every line has a timestamp:\n{log}"
    );

    let lock = std::fs::read_to_string(home.join("state/daemon.lock")).unwrap();
    assert_eq!(lock, "", "a clean exit leaves no holder behind");
    let status = scry(home).args(["daemon", "status"]).output().unwrap();
    assert!(text(&status).contains("running    no"), "{}", text(&status));

    assert_eq!(mode(&home.join("state")), 0o700);
    assert_eq!(mode(&home.join("state/spool")), 0o700);
    assert_eq!(mode(&db), 0o600);
    assert_eq!(mode(&home.join("state/daemon.lock")), 0o600);
}

#[test]
fn a_newer_store_is_refused_untouched_and_install_is_never_the_fix() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let db = home.join("state/scry.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch("CREATE TABLE later (x);").unwrap();
    c.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(c);
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
    let before = std::fs::read(&db).unwrap();

    let run = scry(home).args(["daemon", "run"]).output().unwrap();
    assert_eq!(run.status.code(), Some(1), "{}", text(&run));
    let err = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(err.lines().all(stamped), "{err}");
    assert!(
        err.contains("error: ") && err.contains("upgrade scry"),
        "{err}"
    );

    let status = scry(home).args(["daemon", "status"]).output().unwrap();
    assert!(status.status.success(), "{}", text(&status));
    assert!(
        text(&status).contains("knows only up to"),
        "{}",
        text(&status)
    );

    let out = scry(home).args(["doctor", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let doctor = json(&out);
    assert_eq!(check(&doctor, "store")["level"], "fail");
    for name in ["daemon", "samples"] {
        let fix = check(&doctor, name)["fix"].as_str().unwrap().to_string();
        assert!(fix.starts_with("Upgrade scry"), "{name}: {fix}");
    }

    let out = scry(home).args(["daemon", "uninstall"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("Not installed. Nothing changed."),
        "{}",
        text(&out)
    );

    assert_eq!(std::fs::read(&db).unwrap(), before, "same bytes");
    assert_eq!(mode(&db), 0o644, "same mode");
}

#[test]
fn status_and_doctor_leave_an_empty_store_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let db = home.join("state/scry.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&db, "").unwrap();

    let status = scry(home).args(["daemon", "status"]).output().unwrap();
    assert!(
        text(&status).contains("schema 0; the daemon migrates it when it starts"),
        "{}",
        text(&status)
    );
    let doctor = json(&scry(home).args(["doctor", "--json"]).output().unwrap());
    assert_eq!(check(&doctor, "store")["level"], "warn", "{doctor}");
    assert_eq!(std::fs::metadata(&db).unwrap().len(), 0, "no tables made");
}

#[test]
fn doctor_fails_on_a_broken_config_and_names_the_line() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("config.toml"),
        "keep = []\ndestructive = 1\n",
    )
    .unwrap();
    let out = scry(tmp.path()).args(["doctor"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let line = text(&out)
        .lines()
        .find(|l| l.contains("config"))
        .map(String::from)
        .unwrap();
    assert!(line.starts_with("✗ fail"), "{line}");
    assert!(line.contains("config.toml line 2:"), "{line}");
    assert!(line.contains("Fix: "), "{line}");

    let out = scry(tmp.path())
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v = json(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(check(&v, "config")["level"], "fail");
    assert!(!tmp.path().join("state").exists(), "doctor creates nothing");
}
