//! The daemon's lineage recorder: takes hook events from the spool into the store, and
//! records which session each live process belongs to while its agent lives, so a child
//! re-parented to launchd after its agent dies keeps its `tree` evidence.
//!
//! It rescans when a watched agent process forks or exits (kqueue `NOTE_FORK`,
//! `NOTE_EXIT`), at most once per [`MIN_GAP`], and otherwise every [`IDLE_RESCAN`] to catch
//! agents that started since.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::attribution::{HISTORY, Known, Lineage, Policy};
use crate::daemon_log;
use crate::engine::{Engine, Labels};
use crate::paths::Paths;
use crate::session::{EventKind, Part, SessionProcess};
use crate::spool;
use crate::store::Store;
use crate::sys::kqueue::{ProcWatch, Stopper, Wake};
use crate::sys::now_ms;
use crate::view::Evidence;

pub const IDLE_RESCAN: Duration = Duration::from_secs(30);
/// Forks come in bursts, one per shell command an agent runs; one scan answers a burst.
pub const MIN_GAP: Duration = Duration::from_secs(1);

/// What one pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    pub events: usize,
    /// The agent processes to watch until the next pass.
    pub agents: Vec<i32>,
}

pub struct Recorder {
    engine: Engine,
    policy: Policy,
    passes: u64,
}

impl Recorder {
    pub fn new(policy: Policy) -> Recorder {
        Recorder {
            engine: Engine::labelling(Labels::None),
            policy,
            passes: 0,
        }
    }

    /// Ingests the spool, scans, and writes what the scan learned.
    pub fn pass(&mut self, store: &Store, paths: &Paths) -> Result<Pass> {
        let now = now_ms();
        let mut events = 0;
        spool::drain(&paths.spool(), now, |batch| {
            store.batch(|s| {
                for e in batch {
                    if !s.take_event(&e.id, now)? {
                        continue;
                    }
                    let mut k = Known::default();
                    k.apply(std::slice::from_ref(e));
                    if matches!(e.kind, EventKind::Start { .. }) {
                        s.reopen_session(&e.key(), e.at_ms)?;
                    }
                    for session in &k.sessions {
                        s.upsert_session(session)?;
                    }
                    for p in &k.processes {
                        s.record_process(p)?;
                    }
                    events += 1;
                }
                Ok(())
            })
        })?;

        let snap = self.engine.snapshot()?;
        let since = now - HISTORY.as_millis() as i64;
        let known = Known {
            sessions: store.sessions(since)?,
            processes: store.session_processes(since)?,
        };
        let lineage = Lineage::build(&snap, &known, &self.policy);
        let (sessions, procs) = lineage.records(&snap, now);
        store.batch(|s| {
            for session in &sessions {
                s.upsert_session(session)?;
            }
            for p in &procs {
                s.record_process(p)?;
            }
            Ok(())
        })?;
        self.passes += 1;
        if self.passes % 1000 == 1 {
            store.prune(since)?;
        }
        let agents = procs
            .iter()
            .filter(|p: &&SessionProcess| p.part == Part::Agent && p.evidence == Evidence::Tree)
            .map(|p| p.proc.pid)
            .collect();
        Ok(Pass { events, agents })
    }
}

/// Runs passes until `stop` fires. Errors are logged and the loop goes on.
pub fn run(paths: Paths, mut watch: ProcWatch) {
    let store = match Store::open(&paths.db()) {
        Ok(s) => s,
        Err(e) => {
            daemon_log::line(&format!("recorder cannot open the store: {e:#}"));
            return;
        }
    };
    let keep = crate::config::Config::load(&paths)
        .map(|(c, _)| {
            c.keep
                .iter()
                .map(|k| crate::keep::expand(k, &paths))
                .collect()
        })
        .unwrap_or_default();
    let mut rec = Recorder::new(Policy {
        keep,
        ..Policy::default()
    });
    let mut last = Instant::now() - MIN_GAP;
    loop {
        let gap = last.elapsed();
        if gap < MIN_GAP {
            std::thread::sleep(MIN_GAP - gap);
        }
        last = Instant::now();
        match rec.pass(&store, &paths) {
            Ok(p) => {
                if p.events > 0 {
                    daemon_log::line(&format!("took {} hook events", p.events));
                }
                watch.watch(&p.agents);
            }
            Err(e) => daemon_log::line(&format!("recorder pass failed: {e:#}")),
        }
        match watch.wait(IDLE_RESCAN) {
            Ok(Wake::Stop) => return,
            Ok(_) => {}
            Err(e) => {
                daemon_log::line(&format!("kqueue wait failed: {e:#}"));
                std::thread::sleep(IDLE_RESCAN);
            }
        }
    }
}

/// Starts the recorder on its own thread. The stopper ends it; join the handle after.
pub fn spawn(paths: Paths) -> Result<(Stopper, std::thread::JoinHandle<()>)> {
    let watch = ProcWatch::new()?;
    let stopper = watch.stopper();
    let handle = std::thread::Builder::new()
        .name("recorder".into())
        .spawn(move || run(paths, watch))?;
    Ok((stopper, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Agent;
    use crate::session::{EventKind, SessionEvent};

    /// A minute ago, so a pass's pruning of old sessions keeps these.
    fn t0() -> i64 {
        static T0: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
        *T0.get_or_init(|| now_ms() - 60_000)
    }

    fn event(id: &str, kind: EventKind) -> SessionEvent {
        SessionEvent {
            id: id.into(),
            at_ms: t0(),
            agent: Agent::ClaudeCode,
            session_id: "s1".into(),
            kind,
            cwd: Some("/Users/dev/sources/acme".into()),
            agent_proc: None,
        }
    }

    #[test]
    fn ingest_takes_each_event_once_however_often_it_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().into(), Some(tmp.path().join("scry")));
        paths.ensure_state().unwrap();
        let store = Store::open(&paths.db()).unwrap();
        let start = event("e1", EventKind::Start { source: None });
        let end = event(
            "e2",
            EventKind::End {
                reason: Some("other".into()),
            },
        );
        spool::append(&paths.spool(), &start).unwrap();
        spool::append(&paths.spool(), &end).unwrap();
        // The same line again, as after a crash between reading and deleting a file.
        let again = paths.spool().join("taken-0.jsonl");
        std::fs::write(&again, serde_json::to_string(&start).unwrap() + "\n").unwrap();

        let mut rec = Recorder::new(Policy {
            scry: None,
            ..Policy::default()
        });
        let first = rec.pass(&store, &paths).unwrap();
        assert_eq!(
            first.events, 2,
            "e1 once, though it is in both files, and e2"
        );
        assert_eq!(rec.pass(&store, &paths).unwrap().events, 0);

        // The pass also records whatever agents run on this machine; s1 is the hook's.
        let sessions = store.sessions(0).unwrap();
        let s = sessions.iter().find(|s| s.key.id.short() == "s1").unwrap();
        assert_eq!(s.source, crate::session::Source::Hook);
        assert_eq!(s.started_at_ms, Some(t0()));
        assert_eq!(s.ended_at_ms, Some(t0()));
        assert_eq!(s.end_reason.as_deref(), Some("other"));
        assert_eq!(s.cwd.as_deref(), Some("/Users/dev/sources/acme"));
        assert!(std::fs::read_dir(paths.spool()).unwrap().next().is_none());
    }

    #[test]
    fn a_later_session_start_reopens_an_ended_session_in_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().into(), Some(tmp.path().join("scry")));
        paths.ensure_state().unwrap();
        let store = Store::open(&paths.db()).unwrap();
        let mut rec = Recorder::new(Policy {
            scry: None,
            ..Policy::default()
        });
        let mut take = |events: &[SessionEvent]| {
            for e in events {
                spool::append(&paths.spool(), e).unwrap();
            }
            rec.pass(&store, &paths).unwrap();
        };
        let mut start = event("e1", EventKind::Start { source: None });
        let mut end = event(
            "e2",
            EventKind::End {
                reason: Some("prompt_input_exit".into()),
            },
        );
        end.at_ms = t0() + 1_000;
        take(&[start.clone(), end]);
        let s1 = |store: &Store| {
            store
                .sessions(0)
                .unwrap()
                .into_iter()
                .find(|s| s.key.id.short() == "s1")
                .unwrap()
        };
        assert_eq!(s1(&store).ended_at_ms, Some(t0() + 1_000));
        start.id = "e3".into();
        start.at_ms = t0() + 2_000;
        start.kind = EventKind::Start {
            source: Some("resume".into()),
        };
        take(&[start]);
        let s = s1(&store);
        assert_eq!((s.ended_at_ms, s.end_reason), (None, None), "resumed");
    }

    #[test]
    fn a_pass_records_this_process_tree_under_its_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().into(), Some(tmp.path().join("scry")));
        paths.ensure_state().unwrap();
        let store = Store::open(&paths.db()).unwrap();
        let mut rec = Recorder::new(Policy {
            scry: None,
            ..Policy::default()
        });
        let pass = rec.pass(&store, &paths).unwrap();
        // Whatever agents run on this machine, each one watched is recorded as an agent.
        let recorded = store.session_processes(0).unwrap();
        for pid in &pass.agents {
            assert!(
                recorded
                    .iter()
                    .any(|m| m.proc.pid == *pid && m.part == Part::Agent)
            );
        }
    }
}
