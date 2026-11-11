//! The Ports view as plain data: one row per listening process and port, each with the
//! sentence that explains it. Skins only draw these.

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde::Serialize;

use crate::fixture::tilde;
use crate::model::{Agent, IpFamily, Process, ProjectKind, Proto, Snapshot, Socket, TcpState};
use crate::redact;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PortsView {
    pub rows: Vec<PortRow>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PortRow {
    pub proto: Proto,
    pub port: u16,
    /// Bound addresses, `*` for any.
    pub addresses: Vec<String>,
    pub pid: i32,
    /// `None` when the pid is not in the process list.
    pub start_us: Option<i64>,
    /// A short name such as `next dev`.
    pub process: String,
    /// The redacted command line, for the viewer's own processes.
    pub command: Option<String>,
    /// The process's uid, or the socket's when the process is not in the list.
    pub uid: u32,
    /// "you", or the account `uid` belongs to.
    pub owner: String,
    pub cwd: Option<String>,
    pub project: Option<String>,
    pub project_kind: Option<ProjectKind>,
    pub session: Option<SessionRef>,
    pub age_secs: Option<i64>,
    pub sentence: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionRef {
    pub agent: Agent,
    pub id: String,
    /// The environment key the id came from.
    pub key: String,
}

impl PortRow {
    pub fn yours(&self, snap: &Snapshot) -> bool {
        self.uid == snap.viewer_uid
    }

    /// `:3000`, or `:5353/udp`.
    pub fn label(&self) -> String {
        port_label(self.proto, self.port)
    }
}

pub fn port_label(proto: Proto, port: u16) -> String {
    match proto {
        Proto::Tcp => format!(":{port}"),
        Proto::Udp => format!(":{port}/udp"),
    }
}

impl PortsView {
    /// `home` is written as `~` in paths.
    pub fn build(snap: &Snapshot, home: Option<&str>) -> PortsView {
        let mut groups: BTreeMap<(u16, Proto, i32), Vec<&Socket>> = BTreeMap::new();
        for s in snap.sockets.iter().filter(|s| s.is_listening()) {
            groups
                .entry((s.local_port, s.proto, s.pid))
                .or_default()
                .push(s);
        }
        let rows = groups
            .into_iter()
            .map(|((port, proto, pid), sockets)| row(snap, home, port, proto, pid, &sockets))
            .collect();
        PortsView { rows }
    }
}

fn row(
    snap: &Snapshot,
    home: Option<&str>,
    port: u16,
    proto: Proto,
    pid: i32,
    sockets: &[&Socket],
) -> PortRow {
    let mut addresses: Vec<String> = sockets.iter().map(|s| address(s.local_ip)).collect();
    addresses.sort_by_key(|a| (a != "*", a.clone()));
    addresses.dedup();
    let p = snap.process(pid);
    // A daemon can open its socket as root and then switch users, so the socket's uid is not
    // the process's.
    let uid = p.map_or(sockets[0].uid, |p| p.uid);
    let shown = |path: &str| {
        let mut path = path.to_string();
        if let Some(h) = home {
            tilde(&mut path, h.trim_end_matches('/'));
        }
        path
    };
    let project = p.and_then(|p| p.project.as_ref());
    let mut r = PortRow {
        proto,
        port,
        addresses,
        pid,
        start_us: p.map(|p| p.id.start_us),
        process: p.map_or_else(|| format!("pid {pid}"), short_command),
        command: p.and_then(|p| p.argv.as_deref()).map(|a| {
            let line = redact::join(a);
            match home {
                Some(h) => line.replace(&format!("{}/", h.trim_end_matches('/')), "~/"),
                None => line,
            }
        }),
        uid,
        owner: snap.owner_name(uid),
        cwd: p.and_then(|p| p.cwd.as_deref()).map(shown),
        project: project.map(|r| shown(&r.root)),
        project_kind: project.map(|r| r.kind),
        session: p
            .and_then(|p| p.stamps.session())
            .map(|(agent, id, key)| SessionRef {
                agent,
                id: id.to_string(),
                key: key.to_string(),
            }),
        age_secs: p.map(|p| (snap.taken_at_ms * 1000 - p.id.start_us).max(0) / 1_000_000),
        sentence: String::new(),
    };
    r.sentence = sentence(&r, snap, &r.label());
    r
}

fn address(ip: IpAddr) -> String {
    if ip.is_unspecified() {
        "*".into()
    } else {
        ip.to_string()
    }
}

/// ":3000 is held by `next dev` in ~/acme/wt-auth, started 2 h ago." `port` is how the
/// sentence names the port, such as `:3000` or `:5353 (TCP and UDP)`.
fn sentence(r: &PortRow, snap: &Snapshot, port: &str) -> String {
    if r.start_us.is_none() {
        return format!(
            "{port} is held by pid {}, which is not in the process list, owned by {}.",
            r.pid, r.owner
        );
    }
    let mut s = format!("{port} is held by `{}`", r.process);
    if let Some(project) = &r.project {
        s.push_str(&format!(" in {project}"));
    }
    if !r.yours(snap) {
        s.push_str(&format!(", owned by {}", r.owner));
    }
    if let Some(age) = r.age_secs {
        s.push_str(&format!(", started {} ago", human_age(age)));
    }
    s.push('.');
    s
}

/// Cuts long text to `max` characters, ending in an ellipsis.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

pub fn human_age(secs: i64) -> String {
    match secs {
        ..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs / 60),
        3600..86400 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    }
}

const INTERPRETERS: [&str; 9] = [
    "node", "bun", "deno", "ruby", "python", "java", "perl", "php", "Python",
];

/// Generic entry-point names that say less than the directory holding them.
const GENERIC_SCRIPTS: [&str; 8] = [
    "index.js",
    "index.mjs",
    "index.cjs",
    "main.js",
    "cli.js",
    "server.js",
    "main.py",
    "app.py",
];
const BUILD_DIRS: [&str; 5] = ["dist", "build", "bin", "lib", "src"];

/// Interpreter flags whose next argument is code to run: `python3 -c`, `node -e`.
const INLINE_CODE: [&str; 6] = ["-c", "-e", "-E", "--eval", "-p", "--print"];

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// What a person would call the process: `next dev`, `python3 -m http.server`, `cargo run`.
pub fn short_command(p: &Process) -> String {
    let Some(argv) = p.argv.as_ref().filter(|a| !a.is_empty()) else {
        return p.name.clone();
    };
    // Servers such as nginx and puma rewrite argv[0] into a title with spaces.
    if argv[0].contains(' ') && !argv[0].starts_with('/') {
        return argv[0].trim().to_string();
    }
    let first = base(&argv[0]);
    let rest = &argv[1..];
    let word = |a: &&String| !a.starts_with('-') && !a.contains('/') && !a.contains('=');
    if !INTERPRETERS.iter().any(|i| first.starts_with(i)) {
        return match rest.first().filter(word) {
            Some(next) => format!("{first} {next}"),
            None => first.to_string(),
        };
    }
    if let Some(i) = rest.iter().position(|a| a == "-m")
        && let Some(module) = rest.get(i + 1)
    {
        return format!("{first} -m {module}");
    }
    // `python3 -c '…'` runs inline code, which says nothing as a name.
    if let Some(flag) = rest
        .iter()
        .take_while(|a| a.starts_with('-'))
        .find(|a| INLINE_CODE.contains(&a.as_str()))
    {
        return format!("{first} {flag}");
    }
    let Some(i) = rest.iter().position(|a| !a.starts_with('-')) else {
        return first.to_string();
    };
    if word(&&rest[i]) && !rest[i].contains('.') {
        // A subcommand of the runtime, as in `bun run dev`.
        return match rest.get(i + 1).filter(word) {
            Some(next) => format!("{first} {} {next}", rest[i]),
            None => format!("{first} {}", rest[i]),
        };
    }
    let script = script_name(&rest[i]);
    match rest.get(i + 1).filter(word) {
        Some(next) => format!("{script} {next}"),
        None => script,
    }
}

/// `~/x/node_modules/.bin/next` is `next`; `~/mcp-server-github/dist/index.js` is
/// `mcp-server-github`.
fn script_name(path: &str) -> String {
    let name = base(path);
    if !GENERIC_SCRIPTS.contains(&name) {
        let bare = [".js", ".mjs", ".cjs"]
            .iter()
            .find_map(|ext| name.strip_suffix(ext));
        return bare.unwrap_or(name).to_string();
    }
    path.rsplit('/')
        .skip(1)
        .find(|d| !d.is_empty() && !BUILD_DIRS.contains(d))
        .unwrap_or(name)
        .to_string()
}

/// Why a port is held: the sentence first, then the evidence for each holder.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Why {
    pub port: u16,
    pub sentence: String,
    pub holders: Vec<Holder>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Holder {
    #[serde(flatten)]
    pub row: PortRow,
    pub evidence: Vec<Evidence>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Evidence {
    pub strength: Strength,
    pub detail: String,
}

/// `exact` facts come from the process itself; `observed` ones from a table that can lag it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    Exact,
    Observed,
}

impl Why {
    pub fn build(snap: &Snapshot, port: u16, home: Option<&str>) -> Why {
        let holders: Vec<Holder> = PortsView::build(snap, home)
            .rows
            .into_iter()
            .filter(|r| r.port == port)
            .map(|row| Holder {
                evidence: evidence(snap, &row),
                row,
            })
            .collect();
        // One process can hold the port over TCP and UDP; it is still one process.
        let mut processes: Vec<&PortRow> = Vec::new();
        for h in &holders {
            if !processes.iter().any(|r| r.pid == h.row.pid) {
                processes.push(&h.row);
            }
        }
        let sentence = match processes.as_slice() {
            [] => format!("Nothing listens on :{port}."),
            [one] => {
                let label = if holders.len() > 1 {
                    format!(":{port} (TCP and UDP)")
                } else {
                    one.label()
                };
                let mut s = sentence(one, snap, &label);
                if let Some(session) = &one.session {
                    s.push_str(&format!(
                        " It runs under {} session {}.",
                        session.agent.title(),
                        session.id
                    ));
                }
                s
            }
            many => format!(
                ":{port} is held by {} processes: {}.",
                many.len(),
                many.iter()
                    .map(|r| format!("`{}` (pid {}, {})", r.process, r.pid, r.owner))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        Why {
            port,
            sentence,
            holders,
        }
    }

    /// Records a live check of whether the pid has a `proto` socket on the port open.
    pub fn confirm(&mut self, pid: i32, proto: Proto, holds: Option<bool>) {
        for h in self
            .holders
            .iter_mut()
            .filter(|h| h.row.pid == pid && h.row.proto == proto)
        {
            let detail = match holds {
                Some(true) => format!("pid {pid} has a socket on {} open.", h.row.label()),
                Some(false) => format!(
                    "pid {pid} has no socket on {} open. The table's pid is stale.",
                    h.row.label()
                ),
                None => continue,
            };
            h.evidence.push(Evidence {
                strength: Strength::Exact,
                detail,
            });
        }
    }
}

fn evidence(snap: &Snapshot, r: &PortRow) -> Vec<Evidence> {
    let exact = |detail: String| Evidence {
        strength: Strength::Exact,
        detail,
    };
    let mut ev = Vec::new();
    let mut seen: Vec<(String, usize)> = Vec::new();
    for s in snap.sockets.iter().filter(|s| {
        s.is_listening() && s.local_port == r.port && s.proto == r.proto && s.pid == r.pid
    }) {
        let fam = match s.family {
            IpFamily::V4 => "4",
            IpFamily::V6 => "6",
            IpFamily::Dual => "46",
        };
        let state = match s.state {
            Some(TcpState::Listen) => " LISTEN",
            _ => "",
        };
        let line = format!(
            "{}{fam} {}.{}{state}",
            s.proto.name(),
            address(s.local_ip),
            s.local_port
        );
        match seen.iter_mut().find(|(l, _)| *l == line) {
            Some((_, n)) => *n += 1,
            None => seen.push((line, 1)),
        }
    }
    for (line, n) in seen {
        let times = if n > 1 {
            format!(" ({n} sockets)")
        } else {
            String::new()
        };
        ev.push(Evidence {
            strength: Strength::Observed,
            detail: format!(
                "{line}{times} in the kernel socket table, last used by pid {}.",
                r.pid
            ),
        });
    }
    let Some(p) = snap.process(r.pid) else {
        ev.push(Evidence {
            strength: Strength::Observed,
            detail: format!(
                "pid {} is not in the process list. The table keeps the last pid that used a socket, which can outlive it.",
                r.pid
            ),
        });
        return ev;
    };
    let what = clip(r.command.as_deref().unwrap_or(&p.name), 160);
    ev.push(exact(format!(
        "pid {} is `{what}`, started {} ago.",
        r.pid,
        human_age(r.age_secs.unwrap_or(0))
    )));
    if !r.yours(snap) {
        ev.push(exact(format!(
            "Owned by {}. Without root Scry cannot read its command line or directory.",
            r.owner
        )));
        return ev;
    }
    if let Some(cwd) = &r.cwd {
        ev.push(exact(format!("Working directory {cwd}.")));
    }
    if let (Some(root), Some(kind)) = (&r.project, r.project_kind) {
        ev.push(exact(format!(
            "{root} is the project root: it holds {}.",
            kind.marker()
        )));
    }
    if let Some(s) = &r.session {
        ev.push(exact(format!(
            "{}={} in the process environment.",
            s.key, s.id
        )));
    }
    ev
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    fn typical() -> Snapshot {
        fixture::parse(include_str!("../../../fixtures/typical.json")).unwrap()
    }

    fn argv_process(argv: &[&str]) -> Process {
        Process {
            id: crate::ProcId {
                pid: 2,
                start_us: 0,
            },
            ppid: 1,
            uid: 501,
            name: "fallback".into(),
            exe: None,
            footprint: None,
            argv: Some(argv.iter().map(|s| s.to_string()).collect()),
            cwd: None,
            project: None,
            stamps: Default::default(),
        }
    }

    #[test]
    fn short_commands_name_what_a_person_would_type() {
        let cases: [(&[&str], &str); 15] = [
            (
                &["python3", "-u", "-c", "import time\ntime.sleep(9)"],
                "python3 -c",
            ),
            (&["node", "-e", "require('http').createServer()"], "node -e"),
            (
                &["/Users/dev/acme/target/debug/devServer2", "--port", "8080"],
                "devServer2",
            ),
            (
                &["/var/folders/ab/Xyz0123456789AbCdEf/T/go-build123/b001/exe/devserver"],
                "devserver",
            ),
            (
                &["puma 6.4.0 (tcp://localhost:3002) [backend]"],
                "puma 6.4.0 (tcp://localhost:3002) [backend]",
            ),
            (
                &["node", "/a/node_modules/vite/bin/vite.js", "--port", "3001"],
                "vite",
            ),
            (&["bun", "run", "dev"], "bun run dev"),
            (
                &["python3", "manage.py", "runserver"],
                "manage.py runserver",
            ),
            (
                &["node", "/Users/dev/acme/node_modules/.bin/next", "dev"],
                "next dev",
            ),
            (
                &[
                    "/opt/homebrew/bin/node",
                    "/Users/dev/mcp-server-github/dist/index.js",
                ],
                "mcp-server-github",
            ),
            (
                &["python3", "-m", "http.server", "8000"],
                "python3 -m http.server",
            ),
            (
                &["/usr/bin/ruby", "bin/rails", "server", "-p", "3002"],
                "rails server",
            ),
            (&["cargo", "run", "--release"], "cargo run"),
            (&["/opt/homebrew/bin/nginx", "-g", "daemon off;"], "nginx"),
            (&["node"], "node"),
        ];
        for (argv, want) in cases {
            assert_eq!(short_command(&argv_process(argv)), want, "{argv:?}");
        }
        let mut no_args = argv_process(&[]);
        no_args.argv = None;
        assert_eq!(short_command(&no_args), "fallback");
    }

    #[test]
    fn the_fixture_rows_explain_themselves() {
        let snap = typical();
        let view = PortsView::build(&snap, None);
        let s: Vec<&str> = view.rows.iter().map(|r| r.sentence.as_str()).collect();
        assert!(
            s.contains(&":3000 is held by `next dev` in ~/acme/wt-auth, started 2 h ago."),
            "{s:#?}"
        );
        assert!(
            s.contains(&":22 is held by `launchd`, owned by root, started 9 d ago."),
            "{s:#?}"
        );
    }

    #[test]
    fn ipv4_and_ipv6_listeners_of_one_process_are_one_row() {
        let snap = typical();
        let view = PortsView::build(&snap, None);
        let r: Vec<&PortRow> = view.rows.iter().filter(|r| r.port == 3000).collect();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].addresses, ["127.0.0.1", "::1"]);
        let ssh: Vec<&PortRow> = view.rows.iter().filter(|r| r.port == 22).collect();
        assert_eq!(ssh[0].addresses, ["*"]);
        assert_eq!(ssh[0].command, None, "root's command line is unreadable");
    }

    #[test]
    fn connected_and_unbound_sockets_are_not_rows() {
        let snap = typical();
        let view = PortsView::build(&snap, None);
        assert!(view.rows.iter().all(|r| r.port != 0 && r.port != 54012));
    }

    #[test]
    fn rows_are_sorted_by_port_and_udp_says_so() {
        let snap = typical();
        let view = PortsView::build(&snap, None);
        let ports: Vec<u16> = view.rows.iter().map(|r| r.port).collect();
        let mut sorted = ports.clone();
        sorted.sort();
        assert_eq!(ports, sorted);
        let mdns = view.rows.iter().find(|r| r.proto == Proto::Udp).unwrap();
        assert!(
            mdns.sentence.starts_with(":5353/udp is held by"),
            "{}",
            mdns.sentence
        );
    }

    #[test]
    fn why_gives_the_sentence_session_and_evidence() {
        let snap = typical();
        let mut why = Why::build(&snap, 3000, None);
        assert_eq!(
            why.sentence,
            ":3000 is held by `next dev` in ~/acme/wt-auth, started 2 h ago. It runs under Claude Code session 7f3a9c2e."
        );
        why.confirm(1320, Proto::Tcp, Some(true));
        let ev: Vec<&str> = why.holders[0]
            .evidence
            .iter()
            .map(|e| e.detail.as_str())
            .collect();
        assert!(
            ev.iter()
                .any(|e| e.contains("LISTEN in the kernel socket table, last used by pid 1320")),
            "{ev:#?}"
        );
        assert!(
            ev.contains(&"~/acme/wt-auth is the project root: it holds .git."),
            "{ev:#?}"
        );
        assert!(
            ev.contains(&"CLAUDE_CODE_SESSION_ID=7f3a9c2e in the process environment."),
            "{ev:#?}"
        );
        assert!(
            ev.contains(&"pid 1320 has a socket on :3000 open."),
            "{ev:#?}"
        );
    }

    #[test]
    fn why_for_a_free_port_and_a_vanished_pid() {
        let snap = typical();
        assert_eq!(
            Why::build(&snap, 9, None).sentence,
            "Nothing listens on :9."
        );
        let gone = Why::build(&snap, 8080, None);
        assert_eq!(
            gone.sentence,
            ":8080 is held by pid 4999, which is not in the process list, owned by you."
        );
    }

    #[test]
    fn why_counts_a_process_on_tcp_and_udp_once_and_confirms_each_protocol() {
        let mut snap = typical();
        let mut udp = snap.sockets[2].clone();
        udp.proto = Proto::Udp;
        udp.state = None;
        snap.sockets.push(udp);
        let mut why = Why::build(&snap, 3000, None);
        assert_eq!(why.holders.len(), 2);
        assert_eq!(
            why.sentence,
            ":3000 (TCP and UDP) is held by `next dev` in ~/acme/wt-auth, started 2 h ago. It runs under Claude Code session 7f3a9c2e."
        );
        why.confirm(1320, Proto::Udp, Some(false));
        let stale = |h: &Holder| h.evidence.iter().any(|e| e.detail.contains("stale"));
        let by = |proto| why.holders.iter().find(|h| h.row.proto == proto).unwrap();
        assert!(stale(by(Proto::Udp)));
        assert!(!stale(by(Proto::Tcp)), "the TCP row keeps its own result");

        let mut other = snap.sockets[2].clone();
        other.pid = 1311;
        snap.sockets.push(other);
        let why = Why::build(&snap, 3000, None);
        assert_eq!(
            why.sentence,
            ":3000 is held by 2 processes: `claude` (pid 1311, you), `next dev` (pid 1320, you)."
        );
    }

    #[test]
    fn the_owner_is_the_process_owner_not_the_socket_owner() {
        let mut snap = typical();
        let mut p = snap.process(725).unwrap().clone();
        p.id.pid = 300;
        p.uid = 222;
        p.name = "netbiosd".into();
        snap.processes.push(p);
        let mut s = snap.sockets[0].clone();
        s.local_port = 139;
        s.pid = 300;
        snap.sockets.push(s);
        snap.user_names.insert(222, "_netbios".into());
        let view = PortsView::build(&snap, None);
        let row = view.rows.iter().find(|r| r.port == 139).unwrap();
        assert_eq!((row.uid, row.owner.as_str()), (222, "_netbios"));
        assert!(
            row.sentence.contains("owned by _netbios"),
            "{}",
            row.sentence
        );
    }

    #[test]
    fn home_is_written_as_tilde() {
        let mut snap = typical();
        for p in &mut snap.processes {
            if let Some(proj) = &mut p.project {
                proj.root = proj.root.replace('~', "/Users/dev");
            }
        }
        let view = PortsView::build(&snap, Some("/Users/dev/"));
        assert!(
            view.rows
                .iter()
                .any(|r| r.project.as_deref() == Some("~/acme/wt-auth"))
        );
    }

    #[test]
    fn ages_read_in_one_unit() {
        assert_eq!(human_age(42), "42 s");
        assert_eq!(human_age(150), "2 min");
        assert_eq!(human_age(7300), "2 h");
        assert_eq!(human_age(3 * 86400 + 5), "3 d");
    }
}
