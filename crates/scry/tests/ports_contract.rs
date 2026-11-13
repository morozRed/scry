//! `scry ports --json` lists every listening socket `netstat -anv` shows, with the same pid.

use std::collections::BTreeSet;
use std::process::Command;

type Key = (String, u16, i32);

/// TCP listeners, or UDP sockets bound to a port with no peer, as `(proto, port, pid)`.
fn netstat(proto: &str) -> BTreeSet<Key> {
    let out = Command::new("netstat")
        .args(["-anv", "-p", proto])
        .output()
        .expect("netstat");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let (pid_at, named) = pid_column(&text, proto);
    let mut set = BTreeSet::new();
    for line in text.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() <= pid_at || !t[0].starts_with(proto) {
            continue;
        }
        let listening = match proto {
            "tcp" => t[5] == "LISTEN",
            _ => t[4] == "*.*",
        };
        let Some(port) = t[3].rsplit_once('.').and_then(|(_, p)| p.parse().ok()) else {
            continue;
        };
        if !listening {
            continue;
        }
        let pid = if named {
            // `process:pid`, where the name may hold spaces.
            t[pid_at..]
                .iter()
                .find_map(|w| w.rsplit_once(':').and_then(|(_, p)| p.parse().ok()))
        } else {
            t[pid_at].parse().ok()
        };
        let pid = pid.unwrap_or_else(|| panic!("no pid in {line:?}"));
        set.insert((proto.to_string(), port, pid));
    }
    set
}

/// Where the pid sits in a row, from netstat's header, and whether it comes as
/// `process:pid` (macOS 26) or bare (earlier releases). UDP rows leave `(state)` empty, so
/// their fields sit one to the left.
fn pid_column(text: &str, proto: &str) -> (usize, bool) {
    let header = text
        .lines()
        .find(|l| l.starts_with("Proto"))
        .expect("netstat header");
    let names: Vec<String> = header
        .replace("Local Address", "Local-Address")
        .replace("Foreign Address", "Foreign-Address")
        .split_whitespace()
        .map(String::from)
        .collect();
    let (at, named) = names
        .iter()
        .enumerate()
        .find_map(|(i, n)| match n.as_str() {
            "process:pid" => Some((i, true)),
            "pid" => Some((i, false)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no pid column in {header:?}"));
    (if proto == "udp" { at - 1 } else { at }, named)
}

fn scry_ports() -> BTreeSet<Key> {
    let out = Command::new(env!("CARGO_BIN_EXE_scry"))
        .args(["ports", "--json"])
        .output()
        .expect("scry ports");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["proto"].as_str().unwrap().to_string(),
                r["port"].as_u64().unwrap() as u16,
                r["pid"].as_i64().unwrap() as i32,
            )
        })
        .collect()
}

#[test]
fn every_netstat_listener_is_in_scry_ports_with_the_same_pid() {
    // Sockets come and go between the three reads, so only those netstat shows both
    // before and after the scry run must appear in it.
    let mut before = netstat("tcp");
    before.extend(netstat("udp"));
    let ours = scry_ports();
    let mut after = netstat("tcp");
    after.extend(netstat("udp"));
    let stable: BTreeSet<&Key> = before.intersection(&after).collect();
    assert!(
        stable.iter().any(|k| k.0 == "tcp") && stable.iter().any(|k| k.0 == "udp"),
        "netstat listed no tcp or no udp listeners: {stable:?}"
    );
    let missing: Vec<&&Key> = stable.iter().filter(|k| !ours.contains(**k)).collect();
    assert!(missing.is_empty(), "missing from scry ports: {missing:?}");
}
