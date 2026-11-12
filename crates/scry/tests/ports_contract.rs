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
    let mut set = BTreeSet::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() < 8 || !t[0].starts_with(proto) {
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
        let pid = t[5..]
            .iter()
            .find_map(|w| w.rsplit_once(':').and_then(|(_, p)| p.parse().ok()))
            .expect("process:pid column");
        set.insert((proto.to_string(), port, pid));
    }
    set
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
