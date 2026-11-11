//! A real server end to end: `scry why` names it and its project, `scry stop` ends it.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn scry(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_scry"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("scry runs")
}

struct Server {
    child: Child,
    dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `python3 -m http.server` on a free loopback port, run from a fresh project directory.
fn server() -> (Server, u16) {
    let dir = std::env::temp_dir().join(format!("scry-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname = \"fixture\"\n",
    )
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new("python3")
        .args([
            "-m",
            "http.server",
            "--bind",
            "127.0.0.1",
            &port.to_string(),
        ])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("python3");
    let s = Server { child, dir };
    let until = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < until, "http.server never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
    (s, port)
}

#[test]
fn why_names_the_server_and_its_project_and_stop_ends_it() {
    let (mut s, port) = server();
    let label = format!(":{port}");

    let out = scry(&["why", &label, "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let why: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let holder = &why["holders"][0];
    assert_eq!(holder["pid"].as_i64(), Some(s.child.id() as i64), "{why:#}");
    let root = s.dir.canonicalize().unwrap();
    assert_eq!(holder["project"].as_str(), root.to_str(), "{why:#}");
    assert_eq!(holder["project_kind"], "python");
    let sentence = why["sentence"].as_str().unwrap();
    assert!(
        sentence.starts_with(&format!("{label} is held by `"))
            && sentence.contains(&format!("`python3 -m http.server --bind … {port}`"))
            && sentence.contains(root.to_str().unwrap()),
        "{sentence}"
    );
    assert!(
        !why.to_string().contains("127.0.0.1 "),
        "the bound address is an argument"
    );

    let out = scry(&["why", &label, "--show-command"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(&format!(" -m http.server --bind 127.0.0.1 {port}\n")),
        "{text}"
    );
    let out = scry(&["why", &label, "--json", "--show-command"]);
    assert_eq!(
        out.status.code(),
        Some(64),
        "raw arguments never go into JSON"
    );
    assert!(
        holder["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["detail"] == format!("pid {} has a socket on {label} open.", s.child.id())),
        "{why:#}"
    );

    let out = scry(&["stop", &label]);
    assert!(
        !out.status.success(),
        "no terminal and no --yes must refuse"
    );
    let out = scry(&["stop", &label, "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let refused: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(refused["refusal"]["refused"], "unconfirmed", "{refused:#}");
    assert_eq!(
        refused["target"]["id"]["pid"].as_i64(),
        Some(s.child.id() as i64)
    );
    assert!(s.child.try_wait().unwrap().is_none());

    let out = scry(&["stop", &label, "--yes"]);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("with SIGTERM"));
    use std::os::unix::process::ExitStatusExt;
    let status = s.child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM));
}

#[test]
fn wrong_arguments_exit_64_not_the_still_running_2() {
    for args in [
        &["stop", "abc"][..],
        &["stop"],
        &["why", "port"],
        &["ports", "--nope"],
    ] {
        let out = scry(args);
        assert_eq!(out.status.code(), Some(64), "{args:?}");
        assert!(!out.stderr.is_empty());
    }
    assert_eq!(scry(&["stop", "--help"]).status.code(), Some(0));
    assert_eq!(scry(&["--version"]).status.code(), Some(0));
}

#[test]
fn refusals_are_json_with_json() {
    let refusal = |target: &str| {
        let out = scry(&["stop", target, "--yes", "--json"]);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).expect("JSON on stdout")
    };
    let launchd = refusal("1");
    assert_eq!(launchd["refusal"]["refused"], "protected", "{launchd:#}");
    assert_eq!(launchd["refusal"]["pid"], 1);
    assert!(launchd["message"].as_str().unwrap().contains("protected"));
    assert!(launchd["target"].is_null());

    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let nothing = refusal(&format!(":{free}"));
    assert_eq!(nothing["refusal"]["refused"], "not_found", "{nothing:#}");
}
