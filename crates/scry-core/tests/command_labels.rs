//! Command labels hold no secret from the command line, whatever its shape, in the label or
//! in any output built from it: the snapshot, `ports --json`, `why --json` and `stop --json`.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::time::{Duration, Instant};

use scry_core::control::{self, Target};
use scry_core::fixture;
use scry_core::home::Home;
use scry_core::label::{CommandLabel, Facts, Files, WORDS};
use scry_core::model::{
    IpFamily, ProcId, Process, Proto, SNAPSHOT_SCHEMA, Snapshot, Socket, TcpState,
};
use scry_core::view::ports::{PortsView, Why};
use serde::Serialize;

const PORT: u16 = 3000;
const PID: i32 = 4321;
const HOME: &str = "/Users/dev";
const CWD: &str = "/Users/dev/acme";
const PYTHON: &str =
    "/Library/Frameworks/Python.framework/Versions/3.12/Resources/Python.app/Contents/MacOS/Python";

const FILES: [&str; 11] = [
    "/Users/dev",
    "/System/Volumes/Data/Users/dev",
    "/USERS/DEV",
    "/Volumes/Backup/USERS/Dev",
    "/Users/dev/acme",
    "/Users/dev/acme/server.js",
    "/Users/dev/acme/node_modules/.bin/next",
    "/Users/dev/acme/node_modules/vite/bin/vite.js",
    "/opt/homebrew/bin/npm",
    "/mnt",
    "/usr/bin/env",
];

const PROGRAMS: [&str; 22] = [
    "aws",
    "curl",
    "docker",
    "env",
    "git",
    "helm",
    "kubectl",
    "mount",
    "mysql",
    "nice",
    "node",
    "npm",
    "op",
    "openssl",
    "psql",
    "python3",
    "redis-cli",
    "sh",
    "ssh",
    "sshpass",
    "timeout",
    "watch",
];

struct Disk;

impl Files for Disk {
    fn exists(&self, path: &Path) -> bool {
        FILES.iter().any(|f| Path::new(f) == path)
    }

    fn on_path(&self, name: &str) -> bool {
        PROGRAMS.contains(&name)
    }
}

fn home() -> Home {
    Home::new(Path::new(HOME)).unwrap()
}

fn build(exe: &str, argv: &[String], ports: &[u16]) -> CommandLabel {
    let home = home();
    let facts = Facts {
        exe: Some(exe),
        name: "app",
        cwd: Some(Path::new(CWD)),
        home: Some(&home),
        ports,
        files: &Disk,
    };
    CommandLabel::build(argv, &facts)
}

fn strings(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|s| s.to_string()).collect()
}

/// The executable a corpus command line runs: its first word, from a bin directory.
fn exe_of(argv: &[&str]) -> String {
    match argv[0] {
        "python3" => PYTHON.to_string(),
        "sh" => "/bin/sh".to_string(),
        first => format!("/usr/local/bin/{first}"),
    }
}

/// Every output that shows a process holding :3000 with this label, by name.
fn outputs(command: &CommandLabel) -> Vec<(&'static str, String)> {
    let snap = Snapshot {
        schema: SNAPSHOT_SCHEMA,
        taken_at_ms: 1_762_853_000_000,
        viewer_uid: 501,
        processes: vec![Process {
            id: ProcId {
                pid: PID,
                start_us: 1_762_845_500_000_000,
            },
            ppid: 1,
            uid: 501,
            name: "app".into(),
            exe: Some("/usr/local/bin/app".into()),
            footprint: None,
            command: Some(command.clone()),
            cwd: Some(CWD.into()),
            project: None,
            stamps: Default::default(),
            worker: false,
            tty: None,
            role: None,
            script: None,
            file_args: false,
        }],
        sockets: vec![Socket {
            proto: Proto::Tcp,
            family: IpFamily::V4,
            local_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            local_port: PORT,
            remote_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            remote_port: 0,
            state: Some(TcpState::Listen),
            pid: PID,
            epid: PID,
            uid: 501,
        }],
        ..Snapshot::default()
    };
    let home = home();
    let ports = PortsView::build(&snap, Some(&home));
    let why = Why::build(&snap, PORT, Some(&home));
    let target = control::resolve(&snap, Target::Port(PORT)).expect("a stoppable holder");
    let text: Vec<String> = ports
        .rows
        .iter()
        .map(|r| format!("{} {}", r.process, r.sentence))
        .chain(
            why.holders
                .iter()
                .flat_map(|h| h.evidence.iter().map(|e| e.detail.clone())),
        )
        .collect();
    vec![
        ("label", command.to_string()),
        ("snapshot", json(&fixture::redact(snap.clone(), &home))),
        ("ports --json", json(&ports)),
        ("ports and why text", text.join("\n") + &why.sentence),
        ("why --json", json(&why)),
        ("stop --json", json(&target)),
    ]
}

fn json(v: &impl Serialize) -> String {
    serde_json::to_string(v).unwrap()
}

fn assert_hidden(argv: &[&str], secret: &str) {
    let label = build(&exe_of(argv), &strings(argv), &[PORT]);
    for (name, out) in outputs(&label) {
        assert!(
            !out.contains(secret),
            "{secret:?} leaks into {name} for {argv:?}: {out}"
        );
    }
}

/// Every leak found in review of the rule-based redaction, each with its secret.
const CORPUS: &[(&[&str], &str)] = &[
    // One-letter password flags behind wrappers.
    (&["docker", "exec", "db", "mysql", "-phunter2"], "hunter2"),
    (
        &["kubectl", "exec", "pod", "--", "mysql", "-phunter2"],
        "hunter2",
    ),
    (&["timeout", "60", "mysql", "-phunter2"], "hunter2"),
    (&["watch", "-n1", "redis-cli", "-a", "hunter2"], "hunter2"),
    (&["sh", "-c", "nice -n 10 mysql -phunter2"], "hunter2"),
    (&["sh", "-c", "/usr/bin/env mysql -phunter2"], "hunter2"),
    (&["env", "MYSQL_PWD=hunter2", "mysql"], "hunter2"),
    // Scripts.
    (&["sh", "-c", "MYSQL_HOST=db mysql -phunter2"], "hunter2"),
    (&["sh", "-c", "PASSWORD=hunter2 npm run dev"], "hunter2"),
    (&["sh", "-c", "true;mysql -phunter2"], "hunter2"),
    (&["sh", "-c", "true\nmysql -phunter2"], "hunter2"),
    (&["sh", "-c", "mysql --pass\"\"word=hunter2"], "hunter2"),
    (&["node", "-e", "({password:'hunter2'})"], "hunter2"),
    (
        &["python3", "-c", "import os; os.environ['PW']='hunter2'"],
        "hunter2",
    ),
    // Later pairs in key=value lists.
    (&["curl", "-d", "user=alice&password=hunter2"], "hunter2"),
    (
        &[
            "app",
            "--connection",
            "Server=db;Database=app;User Id=sa;Password=hunter2;",
        ],
        "hunter2",
    ),
    (
        &[
            "app",
            "DefaultEndpointsProtocol=https;AccountName=acct;AccountKey=hunter2==;EndpointSuffix=core.windows.net",
        ],
        "hunter2",
    ),
    (
        &[
            "helm",
            "install",
            "db",
            "--set",
            "a=1,auth.password=hunter2",
        ],
        "hunter2",
    ),
    (
        &[
            "mount",
            "-o",
            "rw,username=alice,password=hunter2",
            "//files/share",
            "/mnt",
        ],
        "hunter2",
    ),
    (
        &["op", "item", "create", "password[password]=hunter2"],
        "hunter2",
    ),
    (&["psql", "password = hunter2"], "hunter2"),
    (
        &["app", "--dsn=root:hunter2=tail@tcp(db:3306)/app"],
        "hunter2",
    ),
    (
        &[
            "java",
            "-Ddb.url=jdbc:oracle:thin:alice/hunter2@db:1521:ORCL",
        ],
        "hunter2",
    ),
    // JSON and YAML.
    (
        &["curl", "-d", r#"{"db":{"auth":{"password":"hunter2"}}}"#],
        "hunter2",
    ),
    (&["curl", "-d", r#"{"password":"hunter2"}"#], "hunter2"),
    (
        &["curl", "-d", r#"{"secret":["first","hunter2"]}"#],
        "hunter2",
    ),
    (
        &["curl", "-d", r#"{"value":"ghp_0123456789abcdefABCDEF"}"#],
        "ghp_0123456789abcdefABCDEF",
    ),
    (
        &["app", "--config", "db:\n  password: |\n    hunter2"],
        "hunter2",
    ),
    // Secrets that start with `/`, `.`, `~` or `-`.
    (&["mysql", "--password=/hunter2"], "hunter2"),
    (&["redis-cli", "-a", ".hunter2"], "hunter2"),
    (&["app", "--token", "~hunter2"], "hunter2"),
    (&["redis-cli", "-a", "--hunter2"], "hunter2"),
    // Positional secrets.
    (&["redis-cli", "AUTH", "hunter2"], "hunter2"),
    (
        &[
            "aws",
            "configure",
            "set",
            "aws_secret_access_key",
            "hunter2",
        ],
        "hunter2",
    ),
    (
        &["npm", "config", "set", "_auth", "YWxpY2U6aHVudGVyMg=="],
        "YWxpY2U6aHVudGVyMg==",
    ),
    (&["sshpass", "-p", "if", "ssh", "host"], "if"),
    (&["sshpass", "-p", "Basic", "ssh", "host"], "Basic"),
    // URLs: user info, escaped names, paths and fragments.
    (
        &["curl", "https://example.com/?%70assword=hunter2"],
        "hunter2",
    ),
    (
        &["curl", "https://example.com/#access_token=hunter2"],
        "hunter2",
    ),
    (
        &["curl", "https://hooks.slack.com/services/T000/B000/hunter2"],
        "hunter2",
    ),
    (
        &["git", "clone", "https://alice:hunter2@example.com/r.git"],
        "hunter2",
    ),
    (
        &["psql", "postgresql://alice:hunter2@example.com/app"],
        "hunter2",
    ),
    // Headers and user:password pairs.
    (
        &["curl", "-H", "aUthorization: Basic YWxpY2U6aHVudGVyMg=="],
        "YWxpY2U6aHVudGVyMg==",
    ),
    (
        &[
            "curl",
            "-HAuthorization: Bearer hunter2",
            "https://example.com",
        ],
        "hunter2",
    ),
    (
        &["curl", "-su", "alice:hunter2", "https://example.com"],
        "hunter2",
    ),
    (
        &["curl", "-ualice:hun=ter2", "https://example.com"],
        "hun=ter2",
    ),
    (&["curl", "-u", "1000:1234", "https://example.com"], "1234"),
    (
        &[
            "curl",
            "-u",
            "ghp_0123456789abcdefABCDEF:x-oauth-basic",
            "https://example.com",
        ],
        "ghp_0123456789abcdefABCDEF",
    ),
    (
        &["docker", "run", "--user", "alice:hunter2", "alpine"],
        "hunter2",
    ),
    // Tokens and one-letter flags with no known shape.
    (
        &[
            "curl",
            "--oauth2-bearer",
            "Zx8Qp2Lm9Rt4Vb7Nc1Kd6Hf3Gj5Ws0Y/",
        ],
        "Zx8Qp2Lm9Rt4Vb7Nc1Kd6Hf3Gj5Ws0Y/",
    ),
    (
        &["openssl", "enc", "-aes-256-cbc", "-k", "hunter2"],
        "hunter2",
    ),
    (
        &["ssh-keygen", "-N", "hunter2", "-f", "id_ed25519"],
        "hunter2",
    ),
    (
        &[
            "tool",
            "eyAiYWxnIjoiSFMyNTYifQ.eyJzdWIiOiIxIn0.c2lnbmF0dXJl",
        ],
        "c2lnbmF0dXJl",
    ),
    (&["app", "--data", "UXg3VmIyS3o_-w"], "UXg3VmIyS3o_-w"),
    (&["app", "-p", "98127634"], "98127634"),
    (&["app", "--pin", "04711"], "04711"),
];

#[test]
fn no_secret_from_the_corpus_reaches_any_output() {
    for (argv, secret) in CORPUS {
        assert_hidden(argv, secret);
    }
}

#[test]
fn the_account_name_reaches_no_output_by_any_spelling_of_home() {
    for path in [
        "/System/Volumes/Data/Users/dev",
        "/USERS/DEV",
        "/Volumes/Backup/USERS/Dev",
    ] {
        let label = build("/usr/local/bin/app", &strings(&["app", path]), &[PORT]);
        for (name, out) in outputs(&label) {
            assert!(
                !out.to_lowercase().contains("dev"),
                "the account name leaks into {name} for {path}: {out}"
            );
        }
    }
}

#[test]
fn a_rewritten_title_is_read_by_the_same_rules() {
    let node = "/opt/homebrew/bin/node";
    let title = "npm exec @scope/server postgresql://alice:hunter2@example.com/app";
    let label = build(node, &strings(&[title]), &[PORT]);
    assert_eq!(label.to_string(), "npm exec …");
    for (name, out) in outputs(&label) {
        assert!(!out.contains("hunter2"), "leaks into {name}: {out}");
    }
    let label = build(node, &strings(&["npm run dev -- --token hunter2"]), &[]);
    assert_eq!(label.to_string(), "npm run dev …");
    let label = build(node, &strings(&["npm run dev --token= --x"]), &[]);
    assert_eq!(
        label.to_string(),
        "npm run dev …",
        "a title's flags never show"
    );
}

#[test]
fn common_servers_keep_recognisable_labels() {
    let node = "/opt/homebrew/Cellar/node/24.4.1/bin/node";
    let cases: &[(&str, &[&str], &[u16], &str)] = &[
        (node, &["npm run dev"], &[], "npm run dev"),
        (
            node,
            &["node", "/opt/homebrew/bin/npm", "run", "dev"],
            &[],
            "npm run dev",
        ),
        (
            node,
            &[
                "node",
                "/Users/dev/acme/node_modules/.bin/next",
                "dev",
                "--port",
                "3000",
            ],
            &[3000],
            "next dev --port 3000",
        ),
        (
            PYTHON,
            &["python3", "-m", "http.server", "8000"],
            &[8000],
            "python3 -m http.server 8000",
        ),
        (node, &["node", "server.js"], &[], "node server.js"),
        (
            node,
            &["node", "/Users/dev/acme/node_modules/vite/bin/vite.js"],
            &[5173],
            "vite",
        ),
        (
            "/bin/sh",
            &["sh", "-c", "MYSQL_HOST=db mysql -phunter2"],
            &[],
            "sh -c …",
        ),
        (
            "/usr/local/bin/docker",
            &["docker", "exec", "db", "mysql", "-phunter2"],
            &[],
            "docker exec … mysql …",
        ),
        // The reverse of a leak: ordinary names that rules used to hide.
        (
            "/Users/dev/acme/target/debug/DevelopmentServerBuild20261111Alpha",
            &["DevelopmentServerBuild20261111Alpha", "--port", "3000"],
            &[3000],
            "DevelopmentServerBuild20261111Alpha --port 3000",
        ),
        (
            "/usr/local/bin/app",
            &["app", "--monkey"],
            &[],
            "app --monkey",
        ),
        (
            "/usr/local/bin/app",
            &["app", "--sort-key", "name", "--watch"],
            &[],
            "app --sort-key … --watch",
        ),
        (
            "/usr/local/bin/git",
            &["git", "-c", "credential.helper=osxkeychain", "fetch"],
            &[],
            "git -c …",
        ),
    ];
    for (exe, argv, ports, want) in cases {
        assert_eq!(
            build(exe, &strings(argv), ports).to_string(),
            *want,
            "{argv:?}"
        );
    }
}

/// xorshift64*, so every run plants the same secrets.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

const SECRET_CHARS: &[u8] =
    b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_./~:@=+%!#$&*,;?[]{}'\"\\^ ";

const FILLER: [&str; 24] = [
    "run",
    "dev",
    "start",
    "--port",
    "-p",
    "--verbose",
    "-v",
    "--",
    "server.js",
    "/Users/dev/acme",
    "mysql",
    "3000",
    "8080",
    "x=1",
    "https://example.com",
    "--sort-key",
    "-m",
    "-c",
    "-e",
    "http.server",
    "db",
    "AUTH",
    "-n1",
    "--set=a",
];

const PROGRAMS_RUN: [(&str, &str); 6] = [
    ("node", "/usr/local/bin/node"),
    ("python3", PYTHON),
    ("sh", "/bin/sh"),
    ("mysql", "/usr/local/bin/mysql"),
    ("docker", "/usr/local/bin/docker"),
    ("app", "/Users/dev/acme/target/debug/app"),
];

/// A secret a label may show anyway: a word from the vocabulary, a file or program that
/// exists, or a listening port.
fn shown_by_design(s: &str) -> bool {
    WORDS.contains(&s)
        || Disk.exists(&Path::new(CWD).join(s))
        || Disk.on_path(s)
        || s.parse::<u16>().is_ok_and(|p| p == PORT)
}

/// A Python module name, which a label shows right after `python3 -m`.
fn module(s: &str) -> bool {
    s.split('.').all(|p| {
        p.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })
}

/// 6 to 24 characters, any of them punctuation. Only a flag's value may start with `-`,
/// because a program reads any other such argument as a flag.
fn secret(rng: &mut Rng, dash_ok: bool) -> String {
    loop {
        let len = 6 + rng.below(19);
        let s: String = (0..len)
            .map(|_| SECRET_CHARS[rng.below(SECRET_CHARS.len())] as char)
            .collect();
        if (dash_ok || !s.starts_with('-')) && !shown_by_design(&s) {
            return s;
        }
    }
}

/// The secret, planted in one of the ways command lines carry one. A flag takes its value as
/// the next argument or after `=`, and a one-letter flag also attached, as getopt reads them.
fn plant(rng: &mut Rng) -> (String, Vec<String>) {
    let flag_value = rng.below(4) == 0;
    let s = secret(rng, flag_value);
    let args = if flag_value {
        let flag = rng.pick(&["-a", "-p", "--password", "--token", "-u"]);
        match rng.below(3) {
            0 => vec![flag.to_string(), s.clone()],
            1 if flag.len() == 2 => vec![format!("{flag}{s}")],
            _ => vec![format!("{flag}={s}")],
        }
    } else {
        match rng.below(11) {
            0 => vec![s.clone()],
            1 => vec!["AUTH".to_string(), s.clone()],
            2 => vec![format!("a=1,b={s},c=2")],
            3 => vec![format!("user=a&password={s}")],
            4 => vec![format!("Server=db;Password={s};")],
            5 => vec![format!("https://alice:{s}@example.com/x")],
            6 => vec![format!("https://example.com/{s}?k={s}#t={s}")],
            7 => vec![format!(r#"{{"a":{{"b":"{s}"}}}}"#)],
            8 => vec![format!("MYSQL_HOST=db mysql -p{s} && echo ok")],
            9 => vec![format!("alice:{s}")],
            _ => vec![format!("{s}/{s}")],
        }
    };
    (s, args)
}

#[test]
fn a_random_planted_secret_is_never_shown() {
    const CASES: usize = 100_000;
    let mut rng = Rng(0x5eed_cafe_f00d_d00d);
    let mut modules = 0;
    for case in 0..CASES {
        let (program, exe) = PROGRAMS_RUN[rng.below(PROGRAMS_RUN.len())];
        let mut argv = vec![program.to_string()];
        let (s, planted) = plant(&mut rng);
        let at = rng.below(4);
        for i in 0..4 {
            if i == at {
                argv.extend(planted.iter().cloned());
            }
            argv.push(rng.pick(&FILLER).to_string());
        }
        let after_m = argv.windows(2).any(|w| w[0] == "-m" && w[1] == s);
        if rng.below(10) == 0 {
            argv = vec![argv.join(" ")];
        }
        let label = build(exe, &argv, &[PORT]);
        if program == "python3" && after_m && module(&s) {
            modules += 1;
            continue;
        }
        assert!(
            !label.to_string().contains(&s),
            "case {case}: {s:?} shows in `{label}` for {argv:?}"
        );
        if case % 100 == 0 {
            for (name, out) in outputs(&label) {
                assert!(!out.contains(&s), "case {case}: {s:?} leaks into {name}");
            }
        }
    }
    assert!(
        modules < CASES / 100,
        "{modules} cases skipped as module names"
    );
}

/// Runs `f` on a thread with a 64 KiB stack, which recursion over a long argument would
/// overflow, and returns how long it took. The time bounds are loose so a busy machine
/// passes; a pass that is quadratic in a megabyte would take hours.
fn timed_on_small_stack(f: impl FnOnce() -> String + Send + 'static) -> (String, Duration) {
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            let start = Instant::now();
            let out = f();
            (out, start.elapsed())
        })
        .unwrap()
        .join()
        .expect("no stack overflow")
}

#[test]
fn megabyte_arguments_take_linear_time_without_recursion() {
    let x = "x=".repeat(30_000);
    let mb = |s: &str| s.repeat((1 << 20) / s.len());
    let cases: Vec<(Vec<String>, &str)> = vec![
        (vec!["app".into(), x.clone()], "app …"),
        (vec!["app".into(), mb("x=a,")], "app …"),
        (vec!["app".into(), mb("{\"a\":[")], "app …"),
        (vec![format!("app {}", mb("x= "))], "app …"),
        (vec![mb("a")], "app"),
        (
            std::iter::repeat_n("x=".to_string(), 100_000).collect(),
            "app …",
        ),
    ];
    for (argv, want) in cases {
        let (label, took) =
            timed_on_small_stack(move || build("/usr/local/bin/app", &argv, &[PORT]).to_string());
        assert_eq!(label, want);
        assert!(took < Duration::from_secs(5), "took {took:?}");
    }
    let big = mb("x=y;");
    let (out, took) = timed_on_small_stack(move || {
        let label = build("/usr/local/bin/app", &["app".into(), big], &[PORT]);
        outputs(&label).into_iter().map(|(_, o)| o).collect()
    });
    assert!(!out.contains("x=y;"));
    assert!(took < Duration::from_secs(5), "took {took:?}");
}
