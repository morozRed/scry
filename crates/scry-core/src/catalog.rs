//! The agent catalog: which executables are agents, and which processes Scry never reaps.
//!
//! Both are reviewed tables in [`RULES`], matched against the executable path the kernel
//! resolved and the process's arguments. The first rule that matches wins, so the order is
//! part of the review: the never-reap entries that run an agent's own binary (`claude
//! daemon`, `codex app-server`) come before the agents, and the broad entries (anything
//! inside an app bundle) come last. `fixtures/agents/install-styles.json` holds one case
//! per install style seen on a real Mac, and a test runs every case through the table.

use serde::{Deserialize, Serialize};

use crate::model::Agent;

/// What a process is to Scry, when the catalog knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// An agent process. It hosts sessions, and what it starts belongs to them.
    Agent(Agent),
    /// On the never-reap list: no rule ever selects it, whatever else is true.
    NeverReap(Infra),
}

/// The kinds of process on the never-reap list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Infra {
    /// `claude daemon`, which runs Claude Code's background sessions.
    ClaudeDaemon,
    /// `claude --bg-pty-host` and its pre-started `--bg-spare`, which hold a background
    /// session's terminal.
    ClaudePtyHost,
    /// `codex app-server`, which the Codex app and editors talk to. It hosts sessions too.
    CodexAppServer,
    /// A helper inside an application bundle (`*.app/Contents/`). The app manages it.
    AppHelper,
    /// A `tmux`, `screen` or `zellij` server. One first started inside an agent's shell
    /// carries that agent's stamp into every later pane.
    Multiplexer,
    /// `gpg-agent`, `keyboxd`, `scdaemon`, `dirmngr` or `ssh-agent`. The first `git commit
    /// -S` or `ssh` of a session starts one, and every later session uses it.
    KeyAgent,
    /// An `ssh` control master or background tunnel: `-M`, `ControlMaster`, `-N`, `-f`, a
    /// `[mux]` title, or a listening Unix socket. Other `ssh` connections share a master, and
    /// `ssh_config` can make one of a plain `ssh -fN host`.
    SshMaster,
}

impl Infra {
    /// Why it is never reaped, as the detail pane says it.
    pub fn why(self) -> &'static str {
        match self {
            Infra::ClaudeDaemon => "it is Claude Code's background daemon",
            Infra::ClaudePtyHost => "it holds a Claude Code background session's terminal",
            Infra::CodexAppServer => "it is the Codex app-server",
            Infra::AppHelper => "it runs inside an application bundle, which manages it",
            Infra::Multiplexer => "it is a terminal multiplexer server",
            Infra::KeyAgent => "it is a key agent that every later session shares",
            Infra::SshMaster => {
                "it is an ssh control master or background tunnel that other connections share"
            }
        }
    }

    /// The agent whose sessions it hosts, for the ones that host sessions as well.
    pub fn hosts(self) -> Option<Agent> {
        match self {
            Infra::CodexAppServer => Some(Agent::Codex),
            _ => None,
        }
    }
}

impl Role {
    /// The agent whose sessions this process hosts: an agent, or a host on the never-reap list.
    pub fn hosts(self) -> Option<Agent> {
        match self {
            Role::Agent(a) => Some(a),
            Role::NeverReap(i) => i.hosts(),
        }
    }

    pub fn never_reap(self) -> Option<Infra> {
        match self {
            Role::NeverReap(i) => Some(i),
            Role::Agent(_) => None,
        }
    }
}

/// One reviewed entry.
pub struct Rule {
    /// The install style or list entry, as fixtures and `doctor` name it.
    pub name: &'static str,
    pub role: Role,
    test: fn(&Exe) -> bool,
}

/// The facts a rule may look at.
struct Exe<'a> {
    path: &'a str,
    file: &'a str,
    argv: &'a [String],
    /// For a script runtime (`node`, `bun`), the script it runs, as it was invoked.
    script: Option<&'a str>,
    facts: Facts,
}

/// What a rule may know of a process beyond its executable and arguments. The engine reads
/// these only for the processes a rule asks about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// It holds a listening Unix-domain socket, as an ssh control master holds its control
    /// socket.
    pub unix_listener: bool,
}

impl Exe<'_> {
    fn arg(&self, a: &str) -> bool {
        self.argv.iter().skip(1).any(|x| x == a)
    }

    fn subcommand(&self, a: &str) -> bool {
        self.argv.get(1).is_some_and(|x| x == a)
    }

    fn script_in(&self, package: &str) -> bool {
        self.script.is_some_and(|s| s.contains(package))
    }

    fn script_named(&self, name: &str) -> bool {
        self.script.is_some_and(|s| file_name(s) == name)
    }

    fn claude(&self) -> bool {
        self.file == "claude" || self.path.contains("/claude/versions/")
    }

    /// Framework builds of Python run every script through `Python.app/Contents/MacOS/Python`,
    /// so that bundle is an interpreter, not an app's helper.
    fn python_launcher(&self) -> bool {
        self.path.contains("/Python.framework/") && self.file.starts_with("Python")
    }

    /// An `ssh` that serves other connections or runs in the background rather than for one
    /// command: `-M` or `-o ControlMaster=…` other than `no`; `-N` (no command, so it is there
    /// for its forwards or its control socket) or `-f` (it backgrounds itself), which is how
    /// a master configured through `ssh_config` is started; or a listening Unix socket. A
    /// master that daemonised itself retitles to `ssh: <socket> [mux]`.
    fn ssh_master(&self) -> bool {
        let retitled = self.argv.first().is_some_and(|a| a.ends_with("[mux]"));
        retitled || self.facts.unix_listener || ssh_serves(self.argv)
    }

    fn codex(&self) -> bool {
        self.file == "codex"
            || self.file.starts_with("codex-aarch64-")
            || self.file.starts_with("codex-x86_64-")
    }
}

use Agent::{ClaudeCode, Codex, Gemini, OpenCode};
use Infra::{
    AppHelper, ClaudeDaemon, ClaudePtyHost, CodexAppServer, KeyAgent, Multiplexer, SshMaster,
};

/// The table, in match order.
pub static RULES: &[Rule] = &[
    // Agent binaries running as infrastructure, before the agents themselves.
    Rule {
        name: "claude daemon",
        role: Role::NeverReap(ClaudeDaemon),
        test: |x| x.claude() && x.subcommand("daemon"),
    },
    Rule {
        name: "claude --bg-pty-host",
        role: Role::NeverReap(ClaudePtyHost),
        test: |x| x.claude() && (x.arg("--bg-pty-host") || x.arg("--bg-spare")),
    },
    Rule {
        name: "codex app-server",
        role: Role::NeverReap(CodexAppServer),
        test: |x| x.codex() && x.arg("app-server"),
    },
    // Claude Code.
    Rule {
        name: "Claude Code, native installer",
        role: Role::Agent(ClaudeCode),
        test: |x| x.path.contains("/.local/share/claude/versions/"),
    },
    Rule {
        name: "Claude Code, inside the Claude desktop app",
        role: Role::Agent(ClaudeCode),
        test: |x| {
            x.path.contains("/claude-code/")
                && x.path.ends_with("/claude.app/Contents/MacOS/claude")
        },
    },
    Rule {
        name: "Claude Code, npm",
        role: Role::Agent(ClaudeCode),
        test: |x| {
            x.path.contains("/node_modules/@anthropic-ai/claude-code")
                || x.script_in("/node_modules/@anthropic-ai/claude-code/")
                || x.script_named("claude")
        },
    },
    Rule {
        name: "Claude Code, other",
        role: Role::Agent(ClaudeCode),
        test: |x| x.file == "claude",
    },
    // Codex.
    Rule {
        name: "Codex, standalone installer",
        role: Role::Agent(Codex),
        test: |x| x.path.contains("/.codex/packages/standalone/") && x.codex(),
    },
    Rule {
        name: "Codex, npm",
        role: Role::Agent(Codex),
        test: |x| {
            x.path.contains("/node_modules/@openai/codex")
                || x.script_in("/node_modules/@openai/codex/")
                || x.script_named("codex")
        },
    },
    Rule {
        name: "Codex, other",
        role: Role::Agent(Codex),
        // Inside an app bundle, a `codex` that is not the app-server is the app's helper,
        // such as the ChatGPT app's `exec-server`.
        test: |x| x.codex() && !x.path.contains(".app/Contents/"),
    },
    // OpenCode.
    Rule {
        name: "OpenCode",
        role: Role::Agent(OpenCode),
        test: |x| {
            x.file == "opencode"
                || x.script_in("/node_modules/opencode-ai/")
                || x.script_named("opencode")
        },
    },
    // Gemini CLI.
    Rule {
        name: "Gemini CLI, npm",
        role: Role::Agent(Gemini),
        test: |x| x.script_in("/node_modules/@google/gemini-cli/") || x.script_named("gemini"),
    },
    Rule {
        name: "Gemini CLI, other",
        role: Role::Agent(Gemini),
        test: |x| x.file == "gemini",
    },
    // The rest of the never-reap list.
    Rule {
        name: "tmux, screen or zellij",
        role: Role::NeverReap(Multiplexer),
        test: |x| matches!(x.file, "tmux" | "screen" | "zellij"),
    },
    Rule {
        name: "gpg-agent, keyboxd, scdaemon, dirmngr or ssh-agent",
        role: Role::NeverReap(KeyAgent),
        test: |x| {
            matches!(
                x.file,
                "gpg-agent" | "keyboxd" | "scdaemon" | "dirmngr" | "ssh-agent"
            )
        },
    },
    Rule {
        name: "an ssh control master",
        role: Role::NeverReap(SshMaster),
        test: |x| x.file == "ssh" && x.ssh_master(),
    },
    Rule {
        name: "a helper inside *.app/Contents/",
        role: Role::NeverReap(AppHelper),
        test: |x| x.path.contains(".app/Contents/") && !x.python_launcher(),
    },
];

const SCRIPT_RUNTIMES: [&str; 2] = ["node", "bun"];

/// The first rule matching an executable path and its arguments.
pub fn classify(exe: &str, argv: &[String]) -> Option<&'static Rule> {
    classify_with(exe, argv, Facts::default())
}

/// [`classify`], with what Scry read of the process besides its arguments.
pub fn classify_with(exe: &str, argv: &[String], facts: Facts) -> Option<&'static Rule> {
    let file = file_name(exe);
    let script = SCRIPT_RUNTIMES
        .contains(&file)
        .then(|| argv.iter().skip(1).find(|a| !a.starts_with('-')))
        .flatten()
        .map(String::as_str);
    let x = Exe {
        path: exe,
        file,
        argv,
        script,
        facts,
    };
    RULES.iter().find(|r| (r.test)(&x))
}

/// The `ssh` options that take a value, from ssh(1).
const SSH_VALUED: &str = "BbcDEeFIiJLlmOoPpQRSWw";


/// Reads `ssh`'s options as ssh does: clusters such as `-fNL`, values glued on (`-p2222`) or
/// in the next argument, and nothing after the destination, which starts the remote command.
fn ssh_serves(argv: &[String]) -> bool {
    let control_master = |o: &str| {
        let o = o.to_ascii_lowercase();
        o.strip_prefix("controlmaster")
            .is_some_and(|v| !matches!(v.trim_start_matches([' ', '=']).trim(), "no"))
    };
    let mut master = false;
    let mut args = argv.iter().skip(1).map(String::as_str);
    while let Some(a) = args.next() {
        if a == "--" {
            break;
        }
        let Some(cluster) = a.strip_prefix('-').filter(|c| !c.is_empty()) else {
            break;
        };
        for (i, c) in cluster.char_indices() {
            if SSH_VALUED.contains(c) {
                let glued = &cluster[i + c.len_utf8()..];
                let value = if glued.is_empty() {
                    args.next().unwrap_or("")
                } else {
                    glued
                };
                master |= c == 'o' && control_master(value);
                break;
            }
            master |= matches!(c, 'M' | 'N' | 'f');
        }
    }
    master
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Case {
        style: String,
        exe: String,
        argv: Vec<String>,
        /// The rule name expected, or null for a process the catalog must not know.
        rule: Option<String>,
        #[serde(default)]
        unix_listener: bool,
    }

    #[test]
    fn every_install_style_fixture_matches_its_rule() {
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../../../fixtures/agents/install-styles.json"))
                .unwrap();
        assert!(cases.len() >= 12);
        for c in &cases {
            let facts = Facts {
                unix_listener: c.unix_listener,
            };
            let got = classify_with(&c.exe, &c.argv, facts).map(|r| r.name);
            assert_eq!(
                got,
                c.rule.as_deref(),
                "{}: {} {:?}",
                c.style,
                c.exe,
                c.argv
            );
        }
        for agent in Agent::ALL {
            assert!(
                cases.iter().any(
                    |c| classify(&c.exe, &c.argv).is_some_and(|r| r.role == Role::Agent(agent))
                ),
                "a fixture for {agent:?}"
            );
        }
    }

    #[test]
    fn every_rule_has_a_fixture_and_a_distinct_name() {
        let text = include_str!("../../../fixtures/agents/install-styles.json");
        for (i, r) in RULES.iter().enumerate() {
            assert!(
                text.contains(&format!("\"rule\": \"{}\"", r.name)),
                "{}",
                r.name
            );
            assert!(RULES[..i].iter().all(|o| o.name != r.name), "{}", r.name);
        }
    }

    #[test]
    fn only_the_app_server_among_the_never_reap_list_hosts_sessions() {
        for r in RULES {
            if let Role::NeverReap(i) = r.role {
                assert_eq!(i.hosts().is_some(), i == CodexAppServer, "{}", r.name);
            }
        }
    }
}
