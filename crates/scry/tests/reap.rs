//! Reap end to end through the `scry` binary: it stops a leftover tree and nothing else.
//!
//! A fixture agent spawns a child that ignores SIGHUP; the child spawns a grandchild; then
//! the agent exits. Both are leftovers with `stamp` evidence. The fixture agent is this test
//! binary, copied to where Claude Code's native installer puts its binary so the catalog
//! recognises it, as in `scry-core`'s leftover fixture test. Every `scry` run has a Scry home
//! of its own with a zero grace period, and every reap names the fixture's session, so
//! nothing else on the machine is ever selected.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Kills what a test started if it fails halfway.
struct Started(Vec<i32>);

impl Drop for Started {
    fn drop(&mut self) {
        for &pid in &self.0 {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    session: String,
    child: i32,
    grandchild: i32,
    bystander: Child,
    _started: Started,
}

impl Fixture {
    /// The agent has exited and both of its processes have gone to launchd.
    fn new(config: &str) -> Fixture {
        // Unique among the tests running at once in this binary, which a clock reading is
        // not, and across runs.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let session = format!(
            "{:02x}{:06x}-4b5a-4968-8776-a5b4c3d2e1f0",
            n & 0xff,
            std::process::id() & 0xff_ffff
        );
        Fixture::with_session(config, &session)
    }

    fn with_session(config: &str, session: &str) -> Fixture {
        let session = session.to_string();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("scry");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), config).unwrap();
        let dir = tmp.path().join(".local/share/claude/versions");
        std::fs::create_dir_all(&dir).unwrap();
        let agent_exe = dir.join("9.9.9");
        std::fs::copy(std::env::current_exe().unwrap(), &agent_exe).unwrap();

        let mut agent = Command::new(&agent_exe);
        agent
            .args(["fixture_agent", "--exact", "--nocapture", "--quiet"])
            .env("SCRY_FIXTURE_AGENT", std::env::current_exe().unwrap())
            .env("SCRY_FIXTURE_SESSION", &session)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        unsafe {
            agent.pre_exec(|| {
                libc::setsid();
                Ok(())
            })
        };
        let mut agent = agent.spawn().unwrap();
        let mut out = BufReader::new(agent.stdout.take().unwrap());
        let (child, grandchild) = loop {
            let mut line = String::new();
            assert!(
                out.read_line(&mut line).unwrap() > 0,
                "the agent said nothing"
            );
            if let Some(rest) = line.trim().strip_prefix("tree ") {
                let (c, g) = rest.split_once(' ').unwrap();
                break (c.parse().unwrap(), g.parse().unwrap());
            }
        };
        let started = Started(vec![child, grandchild]);
        // Something of the user's that no session owns.
        let bystander = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        writeln!(agent.stdin.take().unwrap(), "go").unwrap();
        agent.wait().unwrap();
        let f = Fixture {
            _tmp: tmp,
            home,
            session,
            child,
            grandchild,
            bystander,
            _started: started,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while f.dry_run()["targets"].as_array().unwrap().len() < 2 {
            assert!(Instant::now() < deadline, "never became leftovers");
            std::thread::sleep(Duration::from_millis(100));
        }
        f
    }

    fn scry(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_scry"))
            .args(args)
            .env("SCRY_HOME", &self.home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> (Option<i32>, Value) {
        let out = self.scry(args);
        let v = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{args:?}: {e}\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code(), v)
    }

    fn dry_run(&self) -> Value {
        let (code, v) = self.json(&["reap", "--dry-run", "--json", "--session", &self.session]);
        assert_eq!(code, Some(0), "{v:#}");
        v
    }

    fn audit(&self) -> Vec<(String, String, String)> {
        let db = self.home.join("state/scry.db");
        if !db.exists() {
            return Vec::new();
        }
        let conn = rusqlite::Connection::open(db).unwrap();
        let mut q = conn
            .prepare("SELECT action, target, result FROM audit ORDER BY id")
            .unwrap();
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.bystander.kill();
        let _ = self.bystander.wait();
    }
}

fn pids(v: &Value) -> Vec<i64> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|t| {
            t["pid"]
                .as_i64()
                .or(t["target"]["id"]["pid"].as_i64())
                .unwrap()
        })
        .collect()
}

const ON: &str = "leftover_grace_secs = 0\n";

#[test]
fn reaping_the_m2_fixture_kills_its_tree_children_first_and_nothing_else() {
    let f = Fixture::new(ON);
    let (c, g) = (f.child as i64, f.grandchild as i64);

    // The dry run lists the grandchild before its parent and signals nothing.
    let v = f.dry_run();
    assert_eq!(pids(&v["targets"]), [g, c], "{v:#}");
    assert_eq!(v["targets"][0]["evidence"], "stamp");
    assert!(alive(f.child) && alive(f.grandchild));
    assert!(f.audit().is_empty(), "a dry run writes no audit row");

    // Without a terminal or --yes, nothing happens.
    let (code, v) = f.json(&["reap", "--json", "--session", &f.session]);
    assert_eq!(code, Some(1));
    assert_eq!(v["refusal"]["refused"], "unconfirmed", "{v:#}");
    assert!(alive(f.child) && alive(f.grandchild));

    let (code, v) = f.json(&["reap", "--yes", "--json", "--session", &f.session]);
    assert_eq!(code, Some(0), "{v:#}");
    let done = &v["report"]["done"];
    assert_eq!(pids(done), [g, c], "children first: {v:#}");
    for d in done.as_array().unwrap() {
        assert_eq!(d["outcome"]["result"], "exited", "{v:#}");
        assert_eq!(d["outcome"]["signal"], "SIGTERM", "{v:#}");
    }
    assert_eq!(v["summary"], "Stopped 2.");
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(f.child) || alive(f.grandchild) {
        assert!(Instant::now() < deadline, "still running");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(alive(f.bystander.id() as i32), "the bystander still runs");

    // Each signal is audited before it is sent, and its outcome after, in order.
    let audit = f.audit();
    assert_eq!(audit.len(), 4, "{audit:?}");
    for (row, pid, said) in [
        (&audit[0], g, "sending SIGTERM"),
        (&audit[1], g, "exited after SIGTERM"),
        (&audit[2], c, "sending SIGTERM"),
        (&audit[3], c, "exited after SIGTERM"),
    ] {
        assert_eq!(row.0, "reap", "{audit:?}");
        assert!(
            row.1.starts_with(&format!("pid {pid} started ")),
            "{audit:?}"
        );
        assert!(row.2.starts_with(said), "{audit:?}");
    }

    // Nothing left to reap.
    let (code, v) = f.json(&["reap", "--yes", "--json", "--session", &f.session]);
    assert_eq!(code, Some(0));
    assert_eq!(v["targets"], Value::Array(vec![]));
}

#[test]
fn destructive_false_refuses_and_names_its_line() {
    let f = Fixture::new("# agents read only\nleftover_grace_secs = 0\ndestructive = false\n");
    let (code, v) = f.json(&["reap", "--yes", "--json", "--session", &f.session]);
    assert_eq!(code, Some(1));
    assert_eq!(v["refusal"]["refused"], "disabled", "{v:#}");
    assert_eq!(v["refusal"]["line"], 3);
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains("`destructive = false`") && message.contains("config.toml line 3"),
        "{message}"
    );
    // `scry stop` refuses as well.
    let (code, v) = f.json(&["stop", &f.child.to_string(), "--yes", "--json"]);
    assert_eq!(code, Some(1));
    assert_eq!(v["refusal"]["refused"], "disabled", "{v:#}");
    // The dry run still lists, and says it would be refused.
    let v = f.dry_run();
    assert_eq!(pids(&v["targets"]).len(), 2, "{v:#}");
    assert!(
        v["disabled"]
            .as_str()
            .unwrap()
            .contains("destructive = false")
    );
    assert!(alive(f.child) && alive(f.grandchild));
    assert!(f.audit().is_empty());

    // Keeping only spares, so it is allowed, and it is audited before and after.
    let (code, v) = f.json(&["keep", &f.child.to_string(), "--json"]);
    assert_eq!(code, Some(0), "{v:#}");
    let audit = f.audit();
    let said: Vec<(&str, &str)> = audit.iter().map(|a| (a.0.as_str(), a.2.as_str())).collect();
    assert_eq!(
        said,
        [("keep", "adding at line 4"), ("keep", "added at line 4")],
        "{audit:?}"
    );
}

#[test]
fn nothing_is_signalled_when_the_audit_table_cannot_be_written() {
    let f = Fixture::new(ON);
    // A directory where the store would be: it cannot be opened, so no row can be written.
    std::fs::create_dir_all(f.home.join("state/scry.db")).unwrap();
    let (code, v) = f.json(&["reap", "--yes", "--json", "--session", &f.session]);
    assert_eq!(code, Some(1), "{v:#}");
    assert_eq!(v["refusal"]["refused"], "unaudited", "{v:#}");
    let (code, v) = f.json(&["stop", &f.child.to_string(), "--yes", "--json"]);
    assert_eq!(code, Some(1), "{v:#}");
    assert_eq!(v["refusal"]["refused"], "unaudited", "{v:#}");
    let out = f.scry(&["keep", &f.child.to_string()]);
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("audit row could not be written"), "{said}");
    assert!(alive(f.child) && alive(f.grandchild));
    assert_eq!(
        std::fs::read_to_string(f.home.join("config.toml")).unwrap(),
        ON,
        "the keep list is not written either"
    );
}

#[test]
fn a_kept_process_is_not_listed_again() {
    let f = Fixture::new(ON);
    let child = f.child.to_string();
    let (code, v) = f.json(&["keep", &child, "--json"]);
    assert_eq!(code, Some(0), "{v:#}");
    let entry = v["edit"]["entry"].as_str().unwrap().to_string();
    assert!(
        entry.ends_with("/9.9.9") || entry.contains("reap-"),
        "{entry}"
    );
    let config = std::fs::read_to_string(f.home.join("config.toml")).unwrap();
    assert!(
        config.starts_with("leftover_grace_secs = 0\nkeep = ["),
        "{config}"
    );
    assert_eq!(
        v["diff"][0].as_str().unwrap(),
        format!("+ {}", config.lines().nth(1).unwrap())
    );
    // The same program runs as the grandchild, so the entry keeps both.
    let v = f.dry_run();
    assert_eq!(v["targets"], Value::Array(vec![]), "{v:#}");
    let (_, v) = f.json(&["agents", "--tree", "--json"]);
    let row = v
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["pid"].as_i64() == Some(f.child as i64))
        .unwrap();
    assert_eq!(row["state"], "kept", "{row:#}");
    let (code, v) = f.json(&["keep", &child, "--json"]);
    assert_eq!(code, Some(0));
    assert_eq!(v["edit"]["already"], true);
}

#[test]
fn session_takes_a_full_id_exactly_and_a_prefix_only_when_one_session_has_it() {
    // Two sessions whose ids share their first 8 characters, and more.
    let shared = format!(
        "{:08x}",
        0xe000_0000u32 | (std::process::id() & 0x0fff_ffff)
    );
    let a = Fixture::with_session(ON, &format!("{shared}-aaaa-4968-8776-a5b4c3d2e1f0"));
    let b = Fixture::with_session(ON, &format!("{shared}-bbbb-4968-8776-a5b4c3d2e1f0"));
    let dry = |session: &str| a.json(&["reap", "--dry-run", "--json", "--session", session]);

    let (code, v) = dry(&a.session);
    assert_eq!(code, Some(0), "{v:#}");
    let (ac, ag) = (a.child as i64, a.grandchild as i64);
    assert_eq!(pids(&v["targets"]), [ag, ac], "only a's: {v:#}");
    let (code, v) = dry(&format!("{shared}-bbbb"));
    assert_eq!(code, Some(0), "{v:#}");
    assert_eq!(
        pids(&v["targets"]),
        [b.grandchild as i64, b.child as i64],
        "{v:#}"
    );

    // A prefix both share, or an id neither has, selects nothing.
    for session in [shared.clone(), format!("{shared}-")] {
        let (code, v) = dry(&session);
        assert_eq!(code, Some(1), "{session}: {v:#}");
        assert_eq!(v["refusal"]["refused"], "ambiguous_session", "{v:#}");
        let listed = v["message"].as_str().unwrap();
        assert!(
            listed.contains(&a.session) && listed.contains(&b.session),
            "{listed}"
        );
    }
    let (code, v) = dry(&format!("{}-0000", a.session));
    assert_eq!(code, Some(0), "{v:#}");
    assert_eq!(v["targets"], Value::Array(vec![]), "{v:#}");
    assert!(
        v["message"].as_str().unwrap().starts_with("No session"),
        "{v:#}"
    );
    let (code, v) = a.json(&["reap", "--yes", "--json", "--session", &shared]);
    assert_eq!(code, Some(1), "{v:#}");
    for f in [&a, &b] {
        assert!(alive(f.child) && alive(f.grandchild));
    }
    assert!(a.audit().is_empty());
}

/// The fixture agent's body, run only inside the copied binary: like Claude Code, it hands
/// its child `CLAUDE_CODE_SESSION_ID` and `CLAUDE_PID`.
#[test]
fn fixture_agent() {
    let Some(me) = std::env::var_os("SCRY_FIXTURE_AGENT") else {
        return;
    };
    // The child is the test binary from where cargo built it, not the agent's copy.
    let mut child = Command::new(me)
        .args(["fixture_child", "--exact", "--nocapture", "--quiet"])
        .env_remove("SCRY_FIXTURE_AGENT")
        .env("SCRY_FIXTURE_CHILD", "1")
        .env(
            "CLAUDE_CODE_SESSION_ID",
            std::env::var("SCRY_FIXTURE_SESSION").unwrap(),
        )
        .env("CLAUDE_PID", std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // The test harness prints its own lines first.
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let grandchild = loop {
        let mut line = String::new();
        assert!(
            out.read_line(&mut line).unwrap() > 0,
            "the child said nothing"
        );
        if let Some(pid) = line.trim().strip_prefix("grandchild ") {
            break pid.to_string();
        }
    };
    println!("tree {} {grandchild}", child.id());
    std::io::stdout().flush().unwrap();
    let mut go = String::new();
    std::io::stdin().read_line(&mut go).unwrap();
    // Exits without waiting, so launchd adopts the child.
    std::process::exit(0);
}

/// The fixture agent's child: ignores SIGHUP, starts a grandchild, and waits to be killed.
#[test]
fn fixture_child() {
    if std::env::var_os("SCRY_FIXTURE_CHILD").is_none() {
        return;
    }
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
    let mut grandchild = Command::new(std::env::current_exe().unwrap())
        .args(["fixture_grandchild", "--exact", "--nocapture", "--quiet"])
        .env_remove("SCRY_FIXTURE_CHILD")
        .env("SCRY_FIXTURE_GRANDCHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    println!("grandchild {}", grandchild.id());
    std::io::stdout().flush().unwrap();
    // The agent that reads this pipe exits, and libtest writes a "running for over 60
    // seconds" line to stdout, which would end this process on a closed pipe.
    let null = std::fs::File::create("/dev/null").unwrap();
    unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&null), 1) };
    // Outlives its grandchild: reaped first, the grandchild must not end this one.
    std::thread::sleep(Duration::from_secs(600));
    let _ = grandchild.wait();
}

#[test]
fn fixture_grandchild() {
    if std::env::var_os("SCRY_FIXTURE_GRANDCHILD").is_none() {
        return;
    }
    std::thread::sleep(Duration::from_secs(600));
}
