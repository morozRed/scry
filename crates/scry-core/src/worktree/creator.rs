//! Which agent probably made a worktree, and the evidence for it.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    ClaudeCode,
    Codex,
}

impl Agent {
    pub fn label(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CreatorEvidence {
    /// Files in the worktree's record under `.git/worktrees` that only this agent writes.
    RecordFiles { names: Vec<String> },
    /// The path follows the agent's own layout.
    Path { convention: String },
    /// git's lock reason names the agent, as Claude Code writes when it locks a subagent's
    /// worktree.
    LockReason { reason: String },
}

impl CreatorEvidence {
    pub fn describe(&self) -> String {
        match self {
            CreatorEvidence::RecordFiles { names } if names.len() == 1 => {
                format!("record file {}", names[0])
            }
            CreatorEvidence::RecordFiles { names } => format!("record files {}", names.join(", ")),
            CreatorEvidence::Path { convention } => format!("path {convention}"),
            CreatorEvidence::LockReason { reason } => format!("lock reason \"{reason}\""),
        }
    }
}

/// `agent: None` means unknown, and then `evidence` is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ProbableCreator {
    pub agent: Option<Agent>,
    pub evidence: Vec<CreatorEvidence>,
}

/// What a scan knows about one worktree that can say who made it.
pub struct Clues<'a> {
    pub path: &'a Path,
    pub lock_reason: Option<&'a str>,
    /// Files in its record that only one agent writes, with that agent.
    pub record_files: &'a [(&'a str, Agent)],
}

/// One kind of evidence for who made a worktree. Sources are asked in order: the first to
/// name an agent decides, and later sources that agree add their evidence.
///
/// Record files, path convention and lock reason are the sources today. M2 adds env-stamped
/// processes whose cwd is inside the worktree, then a session's time window labelled as a
/// guess.
pub trait CreatorSource: Send + Sync {
    fn evidence(&self, clues: &Clues) -> Option<(Agent, CreatorEvidence)>;
}

pub fn attribute(sources: &[Box<dyn CreatorSource>], clues: &Clues) -> ProbableCreator {
    let mut found = ProbableCreator::default();
    for source in sources {
        let Some((agent, evidence)) = source.evidence(clues) else {
            continue;
        };
        match found.agent {
            None => found.agent = Some(agent),
            Some(a) if a != agent => continue,
            Some(_) => {}
        }
        found.evidence.push(evidence);
    }
    found
}

/// Files an agent writes into a worktree's record when it makes or tracks the worktree. The
/// agent wrote them itself, so they are asked before the path, which anyone can follow. Files
/// that name different agents say nothing.
pub struct RecordFiles;

impl CreatorSource for RecordFiles {
    fn evidence(&self, clues: &Clues) -> Option<(Agent, CreatorEvidence)> {
        let &(_, agent) = clues.record_files.first()?;
        if clues.record_files.iter().any(|&(_, a)| a != agent) {
            return None;
        }
        let names = clues.record_files.iter().map(|(n, _)| n.to_string());
        let names = names.collect();
        Some((agent, CreatorEvidence::RecordFiles { names }))
    }
}

/// `.claude/worktrees/<name>` inside a repo is Claude Code's; `~/.codex/worktrees` is Codex's.
pub struct PathConvention {
    pub codex_dir: PathBuf,
}

impl CreatorSource for PathConvention {
    fn evidence(&self, clues: &Clues) -> Option<(Agent, CreatorEvidence)> {
        let path = clues.path;
        if path.starts_with(&self.codex_dir) {
            let convention = "~/.codex/worktrees/*".into();
            return Some((Agent::Codex, CreatorEvidence::Path { convention }));
        }
        let parts: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
        let at = parts
            .windows(2)
            .position(|w| w[0] == ".claude" && w[1] == "worktrees")?;
        let name = parts.get(at + 2)?.to_string_lossy();
        let convention = if name.starts_with("agent-") {
            ".claude/worktrees/agent-*"
        } else {
            ".claude/worktrees/*"
        };
        let convention = convention.into();
        Some((Agent::ClaudeCode, CreatorEvidence::Path { convention }))
    }
}

/// Claude Code locks a subagent's worktree with "claude agent <name> (pid …)".
pub struct LockReason;

impl CreatorSource for LockReason {
    fn evidence(&self, clues: &Clues) -> Option<(Agent, CreatorEvidence)> {
        let reason = clues.lock_reason?;
        reason.starts_with("claude agent ").then(|| {
            let reason = reason.to_string();
            (Agent::ClaudeCode, CreatorEvidence::LockReason { reason })
        })
    }
}

pub fn default_sources(home: &Path) -> Vec<Box<dyn CreatorSource>> {
    vec![
        Box::new(RecordFiles),
        Box::new(PathConvention {
            codex_dir: home.join(".codex/worktrees"),
        }),
        Box::new(LockReason),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_files(path: &str, lock: Option<&str>, files: &[(&str, Agent)]) -> ProbableCreator {
        let clues = Clues {
            path: Path::new(path),
            lock_reason: lock,
            record_files: files,
        };
        attribute(&default_sources(Path::new("/Users/dev")), &clues)
    }

    fn who(path: &str, lock: Option<&str>) -> ProbableCreator {
        with_files(path, lock, &[])
    }

    #[test]
    fn a_record_file_an_agent_wrote_outranks_the_path() {
        let c = with_files(
            "/Users/dev/code/app/.claude/worktrees/task",
            None,
            &[
                ("codex-thread.json", Agent::Codex),
                ("codex-synced-branch.json", Agent::Codex),
            ],
        );
        assert_eq!(c.agent, Some(Agent::Codex));
        assert_eq!(
            c.evidence,
            [CreatorEvidence::RecordFiles {
                names: vec![
                    "codex-thread.json".into(),
                    "codex-synced-branch.json".into()
                ]
            }],
            "the path names another agent, so it adds nothing"
        );
        assert_eq!(
            c.evidence[0].describe(),
            "record files codex-thread.json, codex-synced-branch.json"
        );

        let c = with_files(
            "/Users/dev/code/app-wt",
            None,
            &[("CLAUDE_BASE", Agent::ClaudeCode)],
        );
        assert_eq!(c.agent, Some(Agent::ClaudeCode));
        assert_eq!(c.evidence[0].describe(), "record file CLAUDE_BASE");

        let c = with_files(
            "/Users/dev/.codex/worktrees/0f0f/app",
            None,
            &[
                ("CLAUDE_BASE", Agent::ClaudeCode),
                ("codex-thread.json", Agent::Codex),
            ],
        );
        assert_eq!(
            c.agent,
            Some(Agent::Codex),
            "files that disagree say nothing"
        );
        assert_eq!(c.evidence.len(), 1, "{c:?}");
    }

    #[test]
    fn path_convention_comes_first_and_a_lock_reason_adds_evidence() {
        let c = who(
            "/Users/dev/code/app/.claude/worktrees/agent-1234abcd",
            Some("claude agent agent-1234abcd (pid 4242 start Mon Jan  5 09:00:00 2026)"),
        );
        assert_eq!(c.agent, Some(Agent::ClaudeCode));
        assert_eq!(c.evidence.len(), 2);
        assert_eq!(
            c.evidence[0].describe(),
            "path .claude/worktrees/agent-*",
            "{c:?}"
        );

        let c = who("/Users/dev/.codex/worktrees/0f0f/app", None);
        assert_eq!(c.agent, Some(Agent::Codex));

        let c = who("/Users/dev/code/app-wt", Some("manual"));
        assert_eq!(c, ProbableCreator::default(), "unknown carries no evidence");

        let c = who("/Users/dev/code/x/.claude/worktrees", None);
        assert_eq!(c.agent, None, "the folder itself is not a worktree");
    }
}
