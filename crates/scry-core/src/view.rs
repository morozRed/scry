//! What every skin shows, as plain data. The TUI draws it now; the menu bar app draws the
//! same values later, so nothing here knows about terminals.

pub mod agents;
pub mod disk;
pub mod ports;
pub mod worktrees;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::attribution::{AgentState, Known, Lineage, Policy};
use crate::home::Home;
use crate::model::Snapshot;
pub use agents::AgentRow;
pub use disk::{DiskItem, DiskList};
pub use ports::{PortRow, PortsView};
pub use worktrees::{WorktreeItem, WorktreeList, WorktreeRow, WorktreesView};

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
    /// The machine's load, from a live reading; `None` for one built from a snapshot alone.
    #[serde(default)]
    pub pulse: Option<Pulse>,
    pub overview: Overview,
    pub ports: Load<Vec<PortRow>>,
    pub agents: Load<Vec<AgentRow>>,
    /// Read by a scan of its own, slower than a process snapshot, so a skin keeps it across
    /// boards made from snapshots.
    #[serde(default)]
    pub worktrees: Load<WorktreeList>,
    /// Read with the worktrees, by the same reader.
    #[serde(default)]
    pub disk: Load<DiskList>,
}

impl Board {
    /// The overview counts of a snapshot, with the other views not collected. A recorded
    /// snapshot drawn as a fixture looks like this.
    pub fn from_snapshot(s: &Snapshot) -> Self {
        Board {
            pulse: None,
            overview: Overview::from_snapshot(s),
            ports: Load::Ready(PortsView::build(s, Home::from_env().as_ref()).rows),
            agents: Load::Unavailable("Agent sessions are not tracked yet.".into()),
            worktrees: Load::Unavailable("A process snapshot holds no worktrees.".into()),
            disk: Load::Unavailable("A process snapshot holds no disk reading.".into()),
        }
    }

    /// Every view from a recorded snapshot alone, with no store or spool behind it.
    pub fn from_recorded(s: &Snapshot) -> Self {
        let lineage = Lineage::build(s, &Known::default(), &Policy::default());
        Board::build(s, &lineage, None)
    }

    /// Every view from one snapshot and its lineage. Paths are shown as [`Home::redact`]
    /// writes them.
    pub fn build(s: &Snapshot, lineage: &Lineage, home: Option<&Home>) -> Self {
        let mut ports = PortsView::build(s, home).rows;
        for r in ports.iter_mut().filter(|r| r.uid == s.viewer_uid) {
            if lineage.is_leftover(r.pid) {
                r.state = Some(State::Leftover);
            } else if r.state.is_none()
                && lineage.session_of(r.pid).is_some_and(|(session, _)| {
                    !session.ended() && matches!(session.agent, AgentState::Live { .. })
                })
            {
                r.state = Some(State::Live);
            }
        }
        let agents = agents::rows(s, lineage, home);
        Board {
            pulse: None,
            overview: Overview::build(s, lineage, &agents, &ports),
            ports: Load::Ready(ports),
            agents: Load::Ready(agents),
            worktrees: Load::Unavailable("A process snapshot holds no worktrees.".into()),
            disk: Load::Unavailable("A process snapshot holds no disk reading.".into()),
        }
    }

    /// Copies the disk reading's forecast into the pulse, which a process reading cannot
    /// know: the disk is read on its own. Days round as the Disk view writes them ("~19").
    pub fn forecast_pulse(&mut self) {
        use crate::disk::forecast::Forecast;
        let days = match self.disk.ready().and_then(|d| d.forecast.as_ref()) {
            Some(Forecast::FullIn { days, .. }) => Some(days.round().max(1.0) as u32),
            _ => None,
        };
        if let Some(disk) = self.pulse.as_mut().and_then(|p| p.disk.as_mut()) {
            disk.full_in_days = days;
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
    /// Worktree states, one per `worktrees::Status`.
    CanGo,
    CanPrune,
    NeedsLook,
    Dirty,
    Locked,
    Active,
    NotRead,
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
            State::CanGo => "can go",
            State::CanPrune => "can prune",
            State::NeedsLook => "needs a look",
            State::Dirty => "dirty",
            State::Locked => "locked",
            State::Active => "active",
            State::NotRead => "not read",
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            State::Live | State::Active | State::Dirty => Tone::Ok,
            State::Leftover | State::Stale | State::Filling => Tone::Warn,
            State::CanGo | State::CanPrune | State::NeedsLook => Tone::Warn,
            State::Full => Tone::Danger,
            State::Kept | State::Locked => Tone::Info,
            State::Ended | State::Unowned | State::System | State::NotRead => Tone::Quiet,
        }
    }
}

/// How a process was tied to a session (docs/architecture.md).
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

    pub fn parse(s: &str) -> Option<Evidence> {
        [Evidence::Stamp, Evidence::Tree, Evidence::Guess]
            .into_iter()
            .find(|e| e.word() == s)
    }

    /// Exact evidence is what a rule may act on; a guess is only shown.
    pub fn exact(self) -> bool {
        self != Evidence::Guess
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
    /// With `pid`, the identity checked right before each signal. Unknown in a recorded
    /// board, which nothing acts on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_us: Option<i64>,
    pub command: String,
    pub session: Option<String>,
    /// The full id of the session it was listed under; see [`agents::AgentRow`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub evidence: Option<Evidence>,
    pub age_s: u64,
    pub frees: Option<u64>,
    /// The keep-list entry that would spare it, or why there is none; see [`crate::keep`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<crate::keep::KeepAs>,
}

/// The header strip. Each metric is `None` while it warms up.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pulse {
    pub cpu: Option<Cpu>,
    pub memory: Option<Pressure>,
    pub swap_used: Option<u64>,
    pub disk: Option<DiskFree>,
}

impl Pulse {
    /// The header's pulse from the sampler's reading: CPU now and the `recent` readings
    /// before it (oldest first, this one last), memory pressure, swap in use and the free
    /// space Finder shows. Each is `None` while it has no value yet.
    pub fn read(p: &crate::model::Pulse, recent: &[u8], disk_free: Option<u64>) -> Pulse {
        use crate::model::{MemoryPressure, Reading};
        Pulse {
            cpu: match &p.cpu_percent {
                Reading::Ready(v) => Some(Cpu {
                    now: v.round().clamp(0.0, 100.0) as u8,
                    recent: recent.to_vec(),
                }),
                _ => None,
            },
            memory: match &p.memory {
                Reading::Ready(MemoryPressure::Normal) => Some(Pressure::Normal),
                Reading::Ready(MemoryPressure::Warn) => Some(Pressure::Warn),
                Reading::Ready(MemoryPressure::Critical) => Some(Pressure::Critical),
                _ => None,
            },
            swap_used: match &p.swap {
                Reading::Ready(s) => Some(s.used),
                _ => None,
            },
            disk: disk_free.map(|free| DiskFree {
                free,
                full_in_days: None,
            }),
        }
    }
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
    /// What was looked at, so an empty list says what it is clear of.
    #[serde(default)]
    pub checked: Vec<String>,
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
            checked: Vec::new(),
        }
    }

    /// The attention list: leftovers by session first, then ports they hold.
    pub fn build(s: &Snapshot, lineage: &Lineage, agents: &[AgentRow], ports: &[PortRow]) -> Self {
        let mut attention = Vec::new();
        let mut session: Option<&AgentRow> = None;
        let mut groups: Vec<(&AgentRow, Vec<&AgentRow>)> = Vec::new();
        for row in agents {
            if row.depth == 0 {
                session = Some(row);
            } else if row.state == Some(State::Leftover)
                && let Some(head) = session
            {
                match groups.last_mut() {
                    Some((h, v)) if h.id == head.id => v.push(row),
                    _ => groups.push((head, vec![row])),
                }
            }
        }
        for (head, left) in groups {
            let n = left.len();
            attention.push(Attention {
                id: format!("a-left-{}", head.id),
                state: State::Leftover,
                title: format!(
                    "{} left {n} {} running",
                    head.label,
                    if n == 1 { "process" } else { "processes" }
                ),
                context: head.context.clone(),
                size: Some(left.iter().filter_map(|r| r.ram).sum()),
                tab: Tab::Agents,
                row: Some(head.id.clone()),
                detail: Detail {
                    title: match &head.context {
                        Some(c) => format!("{n} left over in {c}"),
                        None => format!("{n} left over"),
                    },
                    facts: head.detail.facts.clone(),
                    sentence: head.detail.sentence.clone(),
                    evidence: left
                        .iter()
                        .map(|r| format!("{}: {}", r.label, r.detail.evidence.join("; ")))
                        .collect(),
                },
            });
        }
        // Ports held by leftovers, one line per session, since an ended test run can leave
        // dozens of servers behind.
        let mut owner: HashMap<i32, &AgentRow> = HashMap::new();
        let mut head: Option<&AgentRow> = None;
        for row in agents {
            if row.depth == 0 {
                head = Some(row);
            } else if let (Some(h), Some(pid)) = (head, row.pid) {
                owner.insert(pid, h);
            }
        }
        let mut held: Vec<(Option<&AgentRow>, Vec<&PortRow>)> = Vec::new();
        for p in ports.iter().filter(|p| p.state == Some(State::Leftover)) {
            let h = owner.get(&p.pid).copied();
            match held
                .iter_mut()
                .find(|(o, _)| o.map(|o| &o.id) == h.map(|h| &h.id))
            {
                Some((_, v)) => v.push(p),
                None => held.push((h, vec![p])),
            }
        }
        for (h, list) in held {
            let p = list[0];
            let more = list.len() - 1;
            let title = match more {
                0 => format!("{} is held by an ended session", p.label()),
                n => format!(
                    "{} and {n} more {} are held by an ended session",
                    p.label(),
                    if n == 1 { "port" } else { "ports" }
                ),
            };
            let mut detail = p.detail();
            if more > 0 {
                detail.evidence.push(format!(
                    "Also held: {}",
                    list[1..]
                        .iter()
                        .map(|p| p.label())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            attention.push(Attention {
                id: format!("a-port-{}-{}-{}", p.proto.name(), p.port, p.pid),
                state: State::Leftover,
                title,
                context: h
                    .and_then(|h| h.context.clone())
                    .or_else(|| Some(p.holder())),
                size: Some({
                    let mut seen = Vec::new();
                    list.iter()
                        .filter(|p| {
                            let new = !seen.contains(&p.pid);
                            seen.push(p.pid);
                            new
                        })
                        .filter_map(|p| p.ram)
                        .sum()
                }),
                tab: Tab::Ports,
                row: Some(format!("{}-{}-{}", p.proto.name(), p.port, p.pid)),
                detail,
            });
        }
        let sessions = lineage.sessions.len();
        let listening = ports.len();
        let own = s.own_processes().count();
        Overview {
            attention,
            checked: vec![
                format!("{own} of your processes against the leftover rule"),
                format!(
                    "{sessions} agent {}",
                    if sessions == 1 { "session" } else { "sessions" }
                ),
                format!(
                    "{listening} listening {} for one held by an ended session",
                    if listening == 1 { "port" } else { "ports" }
                ),
            ],
            ..Overview::from_snapshot(s)
        }
    }

    /// Puts a broken config first: until it is fixed, no process is a leftover, so an empty
    /// list would be a false all-clear.
    pub fn config_unread(&mut self, error: &str) {
        self.attention.insert(
            0,
            Attention {
                id: "a-config".into(),
                state: State::NotRead,
                title: "Your config could not be read, so no leftovers are selected".into(),
                context: None,
                size: None,
                tab: Tab::Agents,
                row: None,
                detail: Detail {
                    title: "Config unreadable".into(),
                    facts: Vec::new(),
                    sentence: format!(
                        "Scry selects no leftovers until the config is fixed, since the keep list in it is unknown: {error}"
                    ),
                    evidence: vec!["Run `scry doctor` for the line to fix.".into()],
                },
            },
        );
    }

    /// "Checked 412 of your processes …, 6 agent sessions and 14 listening ports …."
    pub fn checked_line(&self) -> Option<String> {
        match self.checked.as_slice() {
            [] => None,
            [one] => Some(format!("Checked {one}.")),
            [rest @ .., last] => Some(format!("Checked {} and {last}.", rest.join(", "))),
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

/// Memory in binary units, as Activity Monitor counts it: `1.5G` is 1.5 × 1024³ bytes.
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

/// Disk space in decimal units, as Finder counts it: `412G` is 412 × 1000³ bytes.
pub fn disk_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let step = 1000.0;
    let mut v = b as f64;
    let mut i = 0;
    while v >= step && i < UNITS.len() - 1 {
        v /= step;
        i += 1;
    }
    // Rounding can carry into the next unit: 999.6 MB reads as 1.0 GB, not 1000 MB.
    if i < UNITS.len() - 1 && v.round() >= step {
        v /= step;
        i += 1;
    }
    if i == 0 || v >= 9.95 {
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
    fn disk_bytes_reads_like_finder() {
        assert_eq!(disk_bytes(512), "512B");
        assert_eq!(disk_bytes(1500), "1.5K");
        assert_eq!(disk_bytes(412_000_000_000), "412G");
        assert_eq!(disk_bytes(1_500_000_000), "1.5G");
        // 1 GiB is 1.07 GB on a Mac.
        assert_eq!(disk_bytes(1 << 30), "1.1G");
        assert_eq!(disk_bytes(999_700_000), "1.0G");
        assert_eq!(disk_bytes(9_960_000), "10M");
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
