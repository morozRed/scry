//! What every skin shows, as plain data. The TUI draws it now; the menu bar app draws the
//! same values later, so nothing here knows about terminals.

pub mod agents;
pub mod ports;
pub mod worktrees;

use serde::{Deserialize, Serialize};

use crate::model::Snapshot;
pub use agents::AgentRow;
pub use ports::PortRow;
pub use worktrees::{WorktreeRow, WorktreesView};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tab {
    Overview,
    Ports,
    Agents,
    Disk,
    Worktrees,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Overview,
        Tab::Ports,
        Tab::Agents,
        Tab::Disk,
        Tab::Worktrees,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Ports => "Ports",
            Tab::Agents => "Agents",
            Tab::Disk => "Disk",
            Tab::Worktrees => "Worktrees",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

/// Every view model at one moment. The TUI draws one; `scry watch --json` will stream them,
/// and a recorded one is a design fixture (`scry tui --fixture fixtures/design.json`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Board {
    /// `None` until the pulse collector exists.
    #[serde(default)]
    pub pulse: Option<Pulse>,
    pub overview: Overview,
    pub ports: Load<Vec<PortRow>>,
    pub agents: Load<Vec<AgentRow>>,
}

impl Board {
    pub fn from_snapshot(s: &Snapshot) -> Self {
        Board {
            pulse: None,
            overview: Overview::from_snapshot(s),
            ports: Load::Unavailable("Port scanning is not built yet.".into()),
            agents: Load::Unavailable("Agent sessions are not tracked yet.".into()),
        }
    }

    pub fn parse(text: &str) -> serde_json::Result<Self> {
        serde_json::from_str(text)
    }
}

/// Where a view's data stands. Every view draws all four.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Load<T> {
    #[default]
    Loading,
    /// Expected absence, said plainly: not collected yet, needs more samples.
    Unavailable(String),
    Failed(String),
    Ready(T),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_ref(&self) -> Load<&T> {
        match self {
            Load::Loading => Load::Loading,
            Load::Unavailable(s) => Load::Unavailable(s.clone()),
            Load::Failed(s) => Load::Failed(s.clone()),
            Load::Ready(v) => Load::Ready(v),
        }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Load<U> {
        match self {
            Load::Loading => Load::Loading,
            Load::Unavailable(s) => Load::Unavailable(s),
            Load::Failed(s) => Load::Failed(s),
            Load::Ready(v) => Load::Ready(f(v)),
        }
    }
}

/// How much a state asks of the user. Skins pick colour and glyph from it; the word comes
/// from the state, so nothing depends on colour alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Ok,
    Info,
    Warn,
    Danger,
    Quiet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Live,
    Ended,
    Leftover,
    Kept,
    /// No session owns it, or only by `guess`. Never reaped by rule.
    Unowned,
    /// Owned by macOS or another user: shown, never acted on.
    System,
    Stale,
    /// A disk that fills within weeks at the current rate.
    Filling,
    /// A disk that fills within days.
    Full,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Live => "live",
            State::Ended => "ended",
            State::Leftover => "leftover",
            State::Kept => "kept",
            State::Unowned => "unowned",
            State::System => "system",
            State::Stale => "stale",
            State::Filling => "filling",
            State::Full => "full",
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            State::Live => Tone::Ok,
            State::Leftover | State::Stale | State::Filling => Tone::Warn,
            State::Full => Tone::Danger,
            State::Kept => Tone::Info,
            State::Ended | State::Unowned | State::System => Tone::Quiet,
        }
    }
}

/// How a process was tied to a session (PLAN.md section 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    Stamp,
    Tree,
    Guess,
}

impl Evidence {
    pub fn word(self) -> &'static str {
        match self {
            Evidence::Stamp => "stamp",
            Evidence::Tree => "tree",
            Evidence::Guess => "guess",
        }
    }
}

/// What the detail pane says about one row: a sentence first, then the evidence for it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detail {
    pub title: String,
    #[serde(default)]
    pub facts: Vec<Fact>,
    pub sentence: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub label: String,
    pub value: String,
}

/// One process an action would touch, as the confirmation lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub pid: i32,
    pub command: String,
    pub session: Option<String>,
    pub evidence: Option<Evidence>,
    pub age_s: u64,
    pub frees: Option<u64>,
}

/// The header strip. Each metric is `None` while it warms up.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pulse {
    pub cpu: Option<Cpu>,
    pub memory: Option<Pressure>,
    pub swap_used: Option<u64>,
    pub disk: Option<DiskFree>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cpu {
    /// Percent of all cores, now.
    pub now: u8,
    /// Recent percentages, oldest first.
    pub recent: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pressure {
    Normal,
    Warn,
    Critical,
}

impl Pressure {
    pub fn word(self) -> &'static str {
        match self {
            Pressure::Normal => "normal",
            Pressure::Warn => "warn",
            Pressure::Critical => "critical",
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            Pressure::Normal => Tone::Ok,
            Pressure::Warn => Tone::Warn,
            Pressure::Critical => Tone::Danger,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskFree {
    pub free: u64,
    pub full_in_days: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overview {
    pub process_count: usize,
    pub own_count: usize,
    pub own_footprint: u64,
    /// Ranked things that need the user, most urgent first.
    #[serde(default)]
    pub attention: Vec<Attention>,
}

/// One line of the Overview: what needs the user and where to act on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attention {
    pub id: String,
    pub state: State,
    pub title: String,
    pub context: Option<String>,
    pub size: Option<u64>,
    /// The view that acts on it, and the row there.
    pub tab: Tab,
    pub row: Option<String>,
    pub detail: Detail,
}

impl Overview {
    pub fn from_snapshot(s: &Snapshot) -> Self {
        Overview {
            process_count: s.processes.len(),
            own_count: s.own_processes().count(),
            own_footprint: s.own_processes().filter_map(|p| p.footprint).sum(),
            attention: Vec::new(),
        }
    }

    pub fn headline(&self) -> String {
        match self.attention.len() {
            0 => "Nothing needs attention.".into(),
            1 => "1 thing needs attention.".into(),
            n => format!("{n} things need attention."),
        }
    }

    pub fn counts_line(&self) -> String {
        format!(
            "{} processes, {} yours using {}",
            self.process_count,
            self.own_count,
            human_bytes(self.own_footprint)
        )
    }
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 || v >= 10.0 {
        format!("{v:.0}{}", UNITS[i])
    } else {
        format!("{v:.1}{}", UNITS[i])
    }
}

/// The largest whole unit, as `ps` prints elapsed time but shorter: `41m`, `2h`, `3d`.
pub fn human_age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_reads_like_activity_monitor() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1536), "1.5K");
        assert_eq!(human_bytes(412 * 1024 * 1024), "412M");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5G");
        assert_eq!(human_bytes(22 * 1024 * 1024), "22M");
    }

    #[test]
    fn human_age_keeps_the_largest_unit() {
        assert_eq!(human_age(59), "59s");
        assert_eq!(human_age(41 * 60 + 5), "41m");
        assert_eq!(human_age(2 * 3600 + 59 * 60), "2h");
        assert_eq!(human_age(3 * 86_400), "3d");
    }

    #[test]
    fn the_design_fixture_parses() {
        let board = Board::parse(include_str!("../../../fixtures/design.json")).unwrap();
        assert!(!board.overview.attention.is_empty());
        assert!(board.agents.ready().is_some_and(|rows| rows.len() > 8));
        assert!(board.ports.ready().is_some_and(|rows| rows.len() > 5));
    }
}
