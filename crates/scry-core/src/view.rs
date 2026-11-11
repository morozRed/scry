//! What every skin shows, as plain data. The TUI draws it now; the menu bar app draws the
//! same values later, so nothing here knows about terminals.

use serde::Serialize;

use crate::model::Snapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Overview {
    pub process_count: usize,
    pub own_count: usize,
    pub own_footprint: u64,
    /// Ranked things that need the user. Empty until port and agent checks exist.
    pub attention: Vec<String>,
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
        if self.attention.is_empty() {
            "Nothing needs attention.".into()
        } else {
            format!("{} things need attention.", self.attention.len())
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
    if i == 0 || v >= 100.0 {
        format!("{v:.0}{}", UNITS[i])
    } else {
        format!("{v:.1}{}", UNITS[i])
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
    }
}
