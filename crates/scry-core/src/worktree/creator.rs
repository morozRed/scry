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
    /// The path follows the agent's own layout.
    Path { convention: String },
    /// git's lock reason names the agent, as Claude Code writes when it locks a subagent's
    /// worktree.
    LockReason { reason: String },
}

impl CreatorEvidence {
    pub fn describe(&self) -> String {
        match self {
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

/// One kind of evidence for who made a worktree. Sources are asked in order: the first to
/// name an agent decides, and later sources that agree add their evidence.
///
/// Path convention and lock reason are the sources today. M2 adds env-stamped processes
/// whose cwd is inside the worktree, then a session's time window labelled as a guess.
pub trait CreatorSource: Send + Sync {
    fn evidence(&self, path: &Path, lock_reason: Option<&str>) -> Option<(Agent, CreatorEvidence)>;
}

pub fn attribute(
    sources: &[Box<dyn CreatorSource>],
    path: &Path,
    lock_reason: Option<&str>,
) -> ProbableCreator {
    let mut found = ProbableCreator::default();
    for source in sources {
        let Some((agent, evidence)) = source.evidence(path, lock_reason) else {
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

/// `.claude/worktrees/<name>` inside a repo is Claude Code's; `~/.codex/worktrees` is Codex's.
pub struct PathConvention {
    pub codex_dir: PathBuf,
}

impl CreatorSource for PathConvention {
    fn evidence(&self, path: &Path, _: Option<&str>) -> Option<(Agent, CreatorEvidence)> {
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
    fn evidence(&self, _: &Path, reason: Option<&str>) -> Option<(Agent, CreatorEvidence)> {
        let reason = reason?;
        reason.starts_with("claude agent ").then(|| {
            let reason = reason.to_string();
            (Agent::ClaudeCode, CreatorEvidence::LockReason { reason })
        })
    }
}

pub fn default_sources(home: &Path) -> Vec<Box<dyn CreatorSource>> {
    vec![
        Box::new(PathConvention {
            codex_dir: home.join(".codex/worktrees"),
        }),
        Box::new(LockReason),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who(path: &str, lock: Option<&str>) -> ProbableCreator {
        attribute(
            &default_sources(Path::new("/Users/dev")),
            Path::new(path),
            lock,
        )
    }

    #[test]
    fn path_convention_comes_first_and_a_lock_reason_adds_evidence() {
        let c = who(
            "/Users/dev/sources/scry/.claude/worktrees/agent-a46",
            Some("claude agent agent-a46 (pid 1654 start Wed Nov 11 16:14:45 2026)"),
        );
        assert_eq!(c.agent, Some(Agent::ClaudeCode));
        assert_eq!(c.evidence.len(), 2);
        assert_eq!(
            c.evidence[0].describe(),
            "path .claude/worktrees/agent-*",
            "{c:?}"
        );

        let c = who("/Users/dev/.codex/worktrees/1c11/app", None);
        assert_eq!(c.agent, Some(Agent::Codex));

        let c = who("/Users/dev/sources/app-wt", Some("manual"));
        assert_eq!(c, ProbableCreator::default(), "unknown carries no evidence");

        let c = who("/Users/dev/sources/x/.claude/worktrees", None);
        assert_eq!(c.agent, None, "the folder itself is not a worktree");
    }
}
