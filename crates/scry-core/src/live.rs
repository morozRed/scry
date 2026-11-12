//! One live reading for the TUI and the CLI: a snapshot, what the store and the spool know,
//! and the lineage built from both. Clients only read: the daemon alone writes sessions and
//! ancestry, and without it the spool is applied in memory.

use std::path::PathBuf;

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
use crate::view::Board;

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
}

impl Collector {
    /// A config that cannot be read leaves the keep list unknown, so the policy then
    /// selects no leftovers; [`Collector::config_error`] says why.
    pub fn new(labels: Labels) -> Collector {
        let paths = Paths::from_env().ok();
        Collector {
            engine: Engine::labelling(labels),
            policy: policy(paths.as_ref()),
            paths,
            home: Home::from_env(),
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
        if let Some(e) = self.config_error() {
            board.overview.config_unread(e);
        }
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
            keep: c
                .keep
                .iter()
                .map(|k| p.expand(&PathBuf::from(k)).display().to_string())
                .collect(),
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
