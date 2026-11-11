//! The Agents view model and the reap rule's target selection.

use serde::{Deserialize, Serialize};

use super::{Detail, Evidence, State, Target};

/// One line of the agent tree, flattened depth first. Sessions sit at depth 0 and the
/// processes they own below them, so columns align across depths in every skin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRow {
    pub id: String,
    pub depth: u8,
    /// The agent for a session (`claude`), the command for a process (`next dev`).
    pub label: String,
    /// The project for a session.
    pub context: Option<String>,
    pub pid: Option<i32>,
    pub ram: Option<u64>,
    #[serde(default)]
    pub ports: Vec<u16>,
    pub age_s: u64,
    pub evidence: Option<Evidence>,
    pub state: Option<State>,
    pub detail: Detail,
}

impl AgentRow {
    pub fn session_name(&self) -> String {
        match &self.context {
            Some(c) => format!("{} {c}", self.label),
            None => self.label.clone(),
        }
    }
}

/// What `X` would reap: leftovers tied by `stamp` or `tree`, never a `guess`, children
/// before their parents so nothing is re-parented to launchd mid-way.
pub fn reap_targets(rows: &[AgentRow]) -> Vec<Target> {
    let mut picked: Vec<(u8, usize, Target)> = Vec::new();
    let mut session: Option<String> = None;
    for (i, row) in rows.iter().enumerate() {
        if row.depth == 0 {
            session = Some(row.session_name());
        }
        let exact = matches!(row.evidence, Some(Evidence::Stamp | Evidence::Tree));
        if let (Some(State::Leftover), true, Some(pid)) = (row.state, exact, row.pid) {
            picked.push((
                row.depth,
                i,
                Target {
                    pid,
                    command: row.label.clone(),
                    session: session.clone(),
                    evidence: row.evidence,
                    age_s: row.age_s,
                    frees: row.ram,
                },
            ));
        }
    }
    picked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    picked.into_iter().map(|(_, _, t)| t).collect()
}

/// The stop target for one process row, with the session it belongs to.
pub fn stop_target(rows: &[AgentRow], index: usize) -> Option<Target> {
    let row = rows.get(index)?;
    let pid = row.pid?;
    let session = rows[..=index]
        .iter()
        .rev()
        .find(|r| r.depth == 0)
        .filter(|s| s.pid != Some(pid))
        .map(AgentRow::session_name);
    Some(Target {
        pid,
        command: row.label.clone(),
        session,
        evidence: row.evidence,
        age_s: row.age_s,
        frees: row.ram,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, depth: u8, state: Option<State>, evidence: Option<Evidence>) -> AgentRow {
        AgentRow {
            id: id.into(),
            depth,
            label: id.into(),
            context: (depth == 0).then(|| "proj".into()),
            pid: Some(id.len() as i32 * 100 + depth as i32),
            ram: Some(1),
            ports: vec![],
            age_s: 60,
            evidence,
            state,
            detail: Detail::default(),
        }
    }

    #[test]
    fn reap_takes_exact_leftovers_children_first_and_never_a_guess() {
        let rows = vec![
            row("claude", 0, Some(State::Ended), None),
            row("npm", 1, Some(State::Leftover), Some(Evidence::Stamp)),
            row("next", 2, Some(State::Leftover), Some(Evidence::Tree)),
            row("chrome", 1, Some(State::Leftover), Some(Evidence::Guess)),
            row("mcp", 1, Some(State::Live), Some(Evidence::Stamp)),
        ];
        let picked: Vec<_> = reap_targets(&rows).into_iter().map(|t| t.command).collect();
        assert_eq!(picked, ["next", "npm"]);
        assert_eq!(
            reap_targets(&rows)[0].session.as_deref(),
            Some("claude proj")
        );
    }

    #[test]
    fn stop_target_names_the_owning_session() {
        let rows = vec![
            row("claude", 0, Some(State::Ended), None),
            row("next", 1, Some(State::Leftover), Some(Evidence::Stamp)),
        ];
        let t = stop_target(&rows, 1).unwrap();
        assert_eq!(t.session.as_deref(), Some("claude proj"));
        assert_eq!(stop_target(&rows, 0).unwrap().session, None);
    }
}
