//! `scry leftovers` and `scry overview` with a config that cannot be read: no leftover is
//! selected, and both say why. They read this machine's processes and change nothing.

use std::process::Command;

fn scry(home: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_scry"));
    c.env("HOME", home).env("SCRY_HOME", home.join("scry"));
    c
}

#[test]
fn a_broken_config_stops_leftover_selection_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("scry");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), "keep = [\"postgres\"\n").unwrap();

    let out = scry(tmp.path())
        .args(["leftovers", "--json"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "no list at all");
    assert!(err.contains("selects no leftovers"), "{err}");

    let out = scry(tmp.path())
        .args(["overview", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["attention"][0]["id"], "a-config", "{v}");
}
