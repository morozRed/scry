//! `scry hook install claude`, the hook it installs, and `uninstall`, under a throwaway HOME
//! and SCRY_HOME. Nothing here reads or writes the real `~/.claude`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn scry(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_scry"));
    c.env("HOME", home)
        .env("SCRY_HOME", home.join("scry"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDE_ENV_FILE");
    c
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

const EXISTING: &str = r#"{
  "theme": "dark",
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup",
        "hooks": [{ "type": "command", "command": "echo hello" }]
      }
    ],
    "Stop": [{ "hooks": [{ "type": "command", "command": "say done" }] }]
  }
}
"#;

fn value(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

/// Runs the installed hook as Claude Code would: JSON on stdin, nothing expected back.
fn hook(home: &Path, event: &str, stdin: &str, env_file: Option<&Path>) -> (Output, Duration) {
    let mut c = scry(home);
    c.args(["hook", "run", "claude", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(f) = env_file {
        c.env("CLAUDE_ENV_FILE", f);
    }
    let t = Instant::now();
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (out, t.elapsed())
}

#[test]
fn install_run_and_uninstall_round_trip_keeping_other_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let settings = home.join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, EXISTING).unwrap();

    let dry = scry(home)
        .args(["hook", "install", "claude", "--dry-run"])
        .output()
        .unwrap();
    assert!(dry.status.success(), "{}", text(&dry));
    let shown = text(&dry);
    assert!(
        shown.contains("+ ") && shown.contains("\"SessionEnd\""),
        "{shown}"
    );
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), EXISTING);

    let out = scry(home)
        .args(["hook", "install", "claude"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let after = value(&std::fs::read_to_string(&settings).unwrap());
    let start = after["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(start.len(), 2);
    assert_eq!(start[0]["hooks"][0]["command"], "echo hello");
    assert_eq!(after["hooks"]["Stop"], value(EXISTING)["hooks"]["Stop"]);
    let ours = &start[1]["hooks"][0];
    assert!(ours["command"].as_str().unwrap().ends_with("/scry"));
    assert_eq!(ours["args"][3], "SessionStart");
    assert!(after["hooks"].get("WorktreeCreate").is_none());
    let backups: Vec<_> = std::fs::read_dir(home.join(".claude"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("scry-backup"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(
        std::fs::read_to_string(backups[0].path()).unwrap(),
        EXISTING
    );

    let again = scry(home)
        .args(["hook", "install", "claude"])
        .output()
        .unwrap();
    assert!(
        text(&again).contains("already has Scry's hooks"),
        "{}",
        text(&again)
    );

    // The hook: silent, exit 0, one spool line, and the SCRY_SESSION stamp for later commands.
    let env_file = home.join("claude-env");
    let payload = r#"{"session_id":"4f2c9a1e-0000-4000-8000-000000000001","cwd":"/Users/dev/acme","hook_event_name":"SessionStart","source":"startup","transcript_path":"/Users/dev/.claude/x.jsonl"}"#;
    let (out, took) = hook(home, "SessionStart", payload, Some(&env_file));
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty() && out.stderr.is_empty(),
        "{}",
        text(&out)
    );
    assert!(took < Duration::from_secs(1), "{took:?}");
    eprintln!("SessionStart hook took {took:?} (debug build)");
    assert_eq!(
        std::fs::read_to_string(&env_file).unwrap(),
        "export SCRY_SESSION=4f2c9a1e-0000-4000-8000-000000000001\n"
    );
    let end = r#"{"session_id":"4f2c9a1e-0000-4000-8000-000000000001","hook_event_name":"SessionEnd","reason":"prompt_input_exit"}"#;
    let (out, _) = hook(home, "SessionEnd", end, None);
    assert!(out.status.success() && out.stdout.is_empty());
    let spool = std::fs::read_to_string(home.join("scry/state/spool/events.jsonl")).unwrap();
    let lines: Vec<serde_json::Value> = spool.lines().map(value).collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["kind"]["event"], "start");
    assert_eq!(lines[0]["kind"]["source"], "startup");
    assert_eq!(lines[1]["kind"]["reason"], "prompt_input_exit");
    assert!(
        !spool.contains("transcript"),
        "only the fields Scry needs are kept"
    );

    // Garbage and hostile input still exit 0 silently, and write nothing.
    for bad in ["", "not json", r#"{"session_id":"../../etc/passwd"}"#] {
        let (out, _) = hook(home, "SessionStart", bad, Some(&env_file));
        assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty());
    }
    assert_eq!(
        std::fs::read_to_string(home.join("scry/state/spool/events.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );

    let out = scry(home)
        .args(["hook", "uninstall", "claude"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        value(&std::fs::read_to_string(&settings).unwrap()),
        value(EXISTING)
    );
}

#[test]
fn install_creates_settings_when_there_are_none() {
    let tmp = tempfile::tempdir().unwrap();
    let out = scry(tmp.path())
        .args(["hook", "install", "claude"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let settings = tmp.path().join(".claude/settings.json");
    let v = value(&std::fs::read_to_string(&settings).unwrap());
    assert_eq!(
        v["hooks"]["SessionEnd"][0]["hooks"][0]["args"][3],
        "SessionEnd"
    );
    let out = scry(tmp.path())
        .args(["hook", "uninstall", "claude"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(std::fs::read_to_string(&settings).unwrap().trim(), "{}");
}

#[test]
fn a_broken_settings_file_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = tmp.path().join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "{ \"a\": 1, }").unwrap();
    let out = scry(tmp.path())
        .args(["hook", "install", "claude"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(text(&out).contains("not valid JSON"), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{ \"a\": 1, }");
}
