//! The agent skill `scry setup` installs: one `SKILL.md` that tells a coding agent when to
//! reach for Scry and how to call its CLI safely, and where each agent looks for it.
//!
//! Claude Code, Codex, OpenCode and Gemini CLI all read the same skill format, a folder
//! with a `SKILL.md` whose front matter names the skill and says when to use it. Only the
//! folder each one reads differs. A file Scry wrote carries [`MARK`], so an update or a
//! removal never touches a skill of the same name that someone else wrote.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The skill's folder name, and its `name` in the front matter.
pub const NAME: &str = "scry";

/// Marks a `SKILL.md` that `scry setup` wrote.
pub const MARK: &str = "<!-- Installed by `scry setup`. `scry setup --remove` removes it. -->";

/// The agents that read skills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    OpenCode,
    Gemini,
}

impl Agent {
    pub const ALL: [Agent; 4] = [Agent::Claude, Agent::Codex, Agent::OpenCode, Agent::Gemini];

    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::OpenCode => "OpenCode",
            Agent::Gemini => "Gemini CLI",
        }
    }

    /// The agent's own folder under the home, which says it is installed.
    pub fn home(self, env: &Env) -> PathBuf {
        let home = &env.home;
        match self {
            Agent::Claude => env
                .claude_config_dir
                .clone()
                .unwrap_or_else(|| home.join(".claude")),
            Agent::Codex => env
                .codex_home
                .clone()
                .unwrap_or_else(|| home.join(".codex")),
            Agent::OpenCode => env
                .xdg_config_home
                .clone()
                .unwrap_or_else(|| home.join(".config"))
                .join("opencode"),
            Agent::Gemini => home.join(".gemini"),
        }
    }

    /// The folder the agent reads personal skills from.
    pub fn skills_dir(self, env: &Env) -> PathBuf {
        self.home(env).join("skills")
    }

    /// Where `scry setup` writes the skill for this agent.
    pub fn skill_file(self, env: &Env) -> PathBuf {
        self.skills_dir(env).join(NAME).join("SKILL.md")
    }

    /// Whether the agent looks installed: its folder exists.
    pub fn present(self, env: &Env) -> bool {
        self.home(env).is_dir()
    }
}

/// The environment that decides where each agent keeps its files.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub home: PathBuf,
    /// `CLAUDE_CONFIG_DIR`.
    pub claude_config_dir: Option<PathBuf>,
    /// `CODEX_HOME`.
    pub codex_home: Option<PathBuf>,
    /// `XDG_CONFIG_HOME`, which OpenCode follows.
    pub xdg_config_home: Option<PathBuf>,
}

impl Env {
    pub fn read() -> Result<Env> {
        let home = std::env::home_dir()
            .filter(|h| !h.as_os_str().is_empty())
            .context("cannot tell the home directory")?;
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        Ok(Env {
            home,
            claude_config_dir: var("CLAUDE_CONFIG_DIR"),
            codex_home: var("CODEX_HOME"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
        })
    }
}

/// The `SKILL.md` text.
pub fn text() -> String {
    format!("{FRONT}{MARK}\n{BODY}")
}

const FRONT: &str = "---
name: scry
description: See what coding agents left running on this Mac and clean it up safely. Use when a port is already in use, a dev server or MCP server will not start, something keeps running after an agent session ended, the disk is filling, or old git worktrees pile up; and before stopping processes or deleting caches. Scry traces every process, port, worktree and gigabyte to the project and agent session that made it.
---
";

const BODY: &str = r#"
# Scry

Scry answers "what is running, who started it, and is it safe to remove?" for processes,
listening ports, git worktrees and disk space on macOS. Every command reads the machine
fresh and needs no root.

## Rules

- Always pass `--json` and read the JSON. The text output is for people and may change.
- Look before you act: list with `leftovers`, `why`, `disk` or `worktrees` first, then act
  on exactly what you saw.
- Never stop, reap or clean without the user's go-ahead in this conversation. Show them the
  targets first. Prefer `--dry-run` where a command has it.
- Never act on a process whose evidence is `guess`; Scry does not reap those either.
- Processes of the session you are running in are not leftovers. Do not stop your own
  agent, its shell or its MCP servers.
- A refusal is an answer, not an error to retry around. Report Scry's reason to the user.
- Do not use `kill`, `pkill` or `rm -rf` for what Scry can do: Scry checks that a pid is
  still the same process and that a folder is still the one it showed before it acts.

## Look

| Question | Command |
|---|---|
| Is anything wrong? A ranked list of what needs attention | `scry overview --json` |
| Who holds port 3000, and which session started it? | `scry why 3000 --json` |
| Every listening port with its process, project and session | `scry ports --json` |
| Agent sessions and the processes each started | `scry agents --tree --json` |
| What did ended sessions leave running? | `scry leftovers --json` |
| What fills the disk, and what would deleting each item free? | `scry disk --json` |
| Linked git worktrees, their state and size | `scry worktrees --json` (add `--no-size` to be quick) |
| Is Scry itself set up right? | `scry doctor --json` |

Evidence ties a process to a session: `stamp` (the agent's environment variable, exact),
`tree` (the process tree, exact) or `guess` (it started in the session's folder; shown,
never acted on). A `leftover` is a process an ended session left running.

## Act, after the user agrees

| To | Command |
|---|---|
| Stop one process by pid or by the port it holds | `scry stop 48213 --yes --json` or `scry stop :3000 --yes --json` |
| See what reaping would stop | `scry reap --dry-run --json` |
| Stop every leftover, children first | `scry reap --yes --json` |
| Only one session's leftovers | `scry reap --session <id> --yes --json` |
| Never select a process again | `scry keep <pid> --dry-run`, then `scry keep <pid> --json` |
| Clean up a disk item (Trash, or delete a cache) | `scry clean <item-id> --yes --json`; a delete also needs `--confirm "<item name>"` |

`stop` sends SIGTERM and waits 3 s; add `--kill` only if the user asks for SIGKILL. Each
item in `scry disk --json` says what cleanup it allows: `trash`, `delete`, `prune`, a
`command` for the user to run, or `never`. Do not run a `command` cleanup yourself unless
the user asks.

Exit codes for `stop` and `reap`: 0 done or nothing to do, 1 nothing was signalled, 2 still
running, 64 bad arguments. `destructive = false` in Scry's config (`scry doctor` says where
it is) turns every action off; respect it.
"#;

/// What installing the skill for one agent would do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// No skill there yet.
    Create,
    /// Scry's skill is there with older text.
    Update,
    /// Scry's skill is there and current.
    Current,
    /// A `scry` skill someone else wrote is there; it is left alone.
    Foreign,
}

/// What removing it would do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Remove {
    Remove,
    Missing,
    Foreign,
}

pub fn plan_install(file: &Path) -> Result<Install> {
    Ok(match read(file)? {
        None => Install::Create,
        Some(t) if t == text() => Install::Current,
        Some(t) if t.contains(MARK) => Install::Update,
        Some(_) => Install::Foreign,
    })
}

pub fn plan_remove(file: &Path) -> Result<Remove> {
    Ok(match read(file)? {
        None => Remove::Missing,
        Some(t) if t.contains(MARK) => Remove::Remove,
        Some(_) => Remove::Foreign,
    })
}

/// Writes the skill, through a temporary file so a reader never sees half of it.
pub fn install(file: &Path) -> Result<()> {
    let dir = file.parent().context("the skill file has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(".SKILL.md.scry-tmp");
    std::fs::write(&tmp, text()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, file).with_context(|| format!("writing {}", file.display()))
}

/// Removes Scry's skill file, and its folder when nothing else is in it.
pub fn remove(file: &Path) -> Result<()> {
    std::fs::remove_file(file).with_context(|| format!("removing {}", file.display()))?;
    if let Some(dir) = file.parent() {
        // Fails, and leaves the folder, when it holds anything else.
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

fn read(file: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(file) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &Path) -> Env {
        Env {
            home: home.to_path_buf(),
            ..Env::default()
        }
    }

    #[test]
    fn each_agent_reads_skills_from_its_own_folder() {
        let e = env(Path::new("/Users/me"));
        let file = |a: Agent| a.skill_file(&e).display().to_string();
        assert_eq!(
            file(Agent::Claude),
            "/Users/me/.claude/skills/scry/SKILL.md"
        );
        assert_eq!(file(Agent::Codex), "/Users/me/.codex/skills/scry/SKILL.md");
        assert_eq!(
            file(Agent::OpenCode),
            "/Users/me/.config/opencode/skills/scry/SKILL.md"
        );
        assert_eq!(
            file(Agent::Gemini),
            "/Users/me/.gemini/skills/scry/SKILL.md"
        );
        // Each agent's own setting moves its folder.
        let moved = Env {
            claude_config_dir: Some("/c".into()),
            codex_home: Some("/x".into()),
            xdg_config_home: Some("/cfg".into()),
            ..e
        };
        assert_eq!(
            Agent::Claude.skill_file(&moved),
            Path::new("/c/skills/scry/SKILL.md")
        );
        assert_eq!(
            Agent::Codex.skill_file(&moved),
            Path::new("/x/skills/scry/SKILL.md")
        );
        assert_eq!(
            Agent::OpenCode.skill_file(&moved),
            Path::new("/cfg/opencode/skills/scry/SKILL.md")
        );
    }

    #[test]
    fn the_skill_has_front_matter_the_agents_read() {
        let t = text();
        assert!(t.starts_with("---\nname: scry\ndescription: "));
        let front = t.split("---").nth(1).unwrap();
        // One line of description, as every agent's loader expects.
        assert_eq!(front.trim().lines().count(), 2, "{front}");
        assert!(t.contains(MARK));
    }

    #[test]
    fn install_updates_only_scrys_own_skill_and_remove_leaves_others() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("skills/scry/SKILL.md");
        assert_eq!(plan_install(&file).unwrap(), Install::Create);
        install(&file).unwrap();
        assert_eq!(plan_install(&file).unwrap(), Install::Current);
        std::fs::write(&file, format!("{MARK}\nold text")).unwrap();
        assert_eq!(plan_install(&file).unwrap(), Install::Update);
        assert_eq!(plan_remove(&file).unwrap(), Remove::Remove);
        remove(&file).unwrap();
        assert!(
            !file.parent().unwrap().exists(),
            "the empty folder goes too"
        );
        assert_eq!(plan_remove(&file).unwrap(), Remove::Missing);
        // Someone else's skill of the same name is never touched.
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "---\nname: scry\n---\nmine").unwrap();
        assert_eq!(plan_install(&file).unwrap(), Install::Foreign);
        assert_eq!(plan_remove(&file).unwrap(), Remove::Foreign);
    }
}
