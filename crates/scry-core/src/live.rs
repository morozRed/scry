//! One live reading for the TUI and the CLI: a snapshot, what the store and the spool know,
//! and the lineage built from both. Clients only read: the daemon alone writes sessions and
//! ancestry, and without it the spool is applied in memory.

use std::collections::VecDeque;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::attribution::{HISTORY, Known, Lineage, Policy};
use crate::config::Config;
use crate::engine::{Engine, Labels};
use crate::home::Home;
use crate::model::Snapshot;
use crate::paths::Paths;
use crate::spool;
use crate::store::{SCHEMA_VERSION, Store};
use crate::view::usage::{self, CpuMeter};
use crate::view::{Board, Load, Pulse};

/// CPU readings the pulse keeps, oldest first: enough for the Overview's graph.
const CPU_RECENT: usize = 30;

/// Where the lineage's history came from, so a view can say what it is missing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum History {
    /// The store, plus spool events the daemon has not taken yet.
    Store,
    /// No store yet: hook events from the spool only, and no ancestry recorded before a
    /// parent died.
    SpoolOnly,
    /// Neither could be read.
    Unreadable(String),
}

pub struct Reading {
    pub snap: Snapshot,
    pub lineage: Lineage,
    pub history: History,
}

pub struct Collector {
    engine: Engine,
    paths: Option<Paths>,
    policy: Policy,
    home: Option<Home>,
    /// The last CPU readings, for the pulse's graph.
    cpu: VecDeque<u8>,
    /// Each process's CPU time at the last reading, for the Top view's shares.
    meter: CpuMeter,
    cores: u32,
}

impl Collector {
    /// A config that cannot be read leaves the keep list unknown, so the policy then
    /// selects no leftovers; [`Collector::config_error`] says why.
    pub fn new(labels: Labels) -> Collector {
        Collector::with_paths(labels, Paths::from_env().ok())
    }

    /// A collector for the config and store at `paths`.
    pub fn with_paths(labels: Labels, paths: Option<Paths>) -> Collector {
        Collector {
            engine: Engine::labelling(labels),
            policy: policy(paths.as_ref()),
            paths,
            home: Home::from_env(),
            cpu: VecDeque::with_capacity(CPU_RECENT),
            meter: CpuMeter::new(),
            cores: crate::sys::logical_cores(),
        }
    }

    /// The rule's settings in effect: the keep list and grace period from the config, which
    /// is read again on every reading, so a `K` takes effect on the next one.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn paths(&self) -> Option<&Paths> {
        self.paths.as_ref()
    }

    /// Why the config could not be read. While it is broken no process is a leftover.
    pub fn config_error(&self) -> Option<&str> {
        self.policy.config_error.as_deref()
    }

    pub fn read(&mut self) -> Result<Reading> {
        self.policy = policy(self.paths.as_ref());
        let snap = self.engine.snapshot()?;
        let (known, history) = self.known(snap.taken_at_ms);
        let lineage = Lineage::build(&snap, &known, &self.policy);
        Ok(Reading {
            snap,
            lineage,
            history,
        })
    }

    pub fn board(&mut self) -> Result<Board> {
        let r = self.read()?;
        let mut board = Board::build(&r.snap, &r.lineage, self.home.as_ref());
        let shares = self.meter.shares(&r.snap, self.cores);
        board.usage = Load::Ready(usage::build(
            &r.snap,
            &r.lineage,
            self.home.as_ref(),
            self.cores,
            shares.as_ref(),
        ));
        if let Some(e) = self.config_error() {
            board.overview.config_unread(e);
        }
        if let crate::model::Reading::Ready(v) = r.snap.pulse.cpu_percent {
            if self.cpu.len() == CPU_RECENT {
                self.cpu.pop_front();
            }
            self.cpu.push_back(v.round().clamp(0.0, 100.0) as u8);
        }
        let free = crate::disk::space::important(Path::new(crate::daemon::SAMPLED_VOLUME));
        let recent: Vec<u8> = self.cpu.iter().copied().collect();
        board.pulse = Some(Pulse::read(&r.snap.pulse, &recent, free));
        Ok(board)
    }

    pub fn home(&self) -> Option<&Home> {
        self.home.as_ref()
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    fn known(&self, now_ms: i64) -> (Known, History) {
        let Some(paths) = &self.paths else {
            return (
                Known::default(),
                History::Unreadable("no home directory".into()),
            );
        };
        let since = now_ms - HISTORY.as_millis() as i64;
        let db = paths.db();
        let (mut known, history) = if db.exists() {
            // A store the daemon has not migrated yet has no session tables.
            match Store::open_read_only(&db).and_then(|s| {
                if s.schema_version()? < SCHEMA_VERSION {
                    return Ok(None);
                }
                Ok(Some(Known {
                    sessions: s.sessions(since)?,
                    processes: s.session_processes(since)?,
                }))
            }) {
                Ok(Some(k)) => (k, History::Store),
                Ok(None) => (Known::default(), History::SpoolOnly),
                Err(e) => (Known::default(), History::Unreadable(format!("{e:#}"))),
            }
        } else {
            (Known::default(), History::SpoolOnly)
        };
        known.apply(&spool::read_all(&paths.spool()));
        (known, history)
    }
}

/// The leftover rule's settings from the config. A config that cannot be read leaves the
/// keep list unknown, so no process is a leftover until it is fixed.
fn policy(paths: Option<&Paths>) -> Policy {
    match paths.map(|p| (Config::load(p), p)) {
        Some((Ok((c, _)), p)) => Policy {
            keep: c.keep.iter().map(|k| crate::keep::expand(k, p)).collect(),
            grace: c.leftover_grace,
            ..Policy::default()
        },
        Some((Err(e), _)) => Policy {
            config_error: Some(e.to_string()),
            ..Policy::default()
        },
        None => Policy::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_board_carries_the_pulse_and_cpu_once_two_readings_exist() {
        let mut c = Collector::with_paths(crate::engine::Labels::All, None);
        let first = c.board().unwrap();
        let pulse = first.pulse.expect("a live board has a pulse");
        assert!(pulse.memory.is_some(), "memory pressure reads at once");
        assert!(pulse.swap_used.is_some());
        assert!(pulse.disk.is_some_and(|d| d.free > 0));
        // The tick counters can stand still for a moment under a parallel test run.
        let mut cpu = None;
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            cpu = c.board().unwrap().pulse.and_then(|p| p.cpu);
            if cpu.is_some() {
                break;
            }
        }
        let cpu = cpu.expect("a second reading has the CPU load");
        assert!(cpu.now <= 100);
        assert_eq!(cpu.recent.last(), Some(&cpu.now));
    }
}
