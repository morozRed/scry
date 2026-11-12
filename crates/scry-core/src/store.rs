//! The SQLite store. One writer per table: the daemon writes samples, clients append audit
//! rows. `PRAGMA user_version` is the schema version, and a client refuses a store that is
//! newer than it knows before it changes anything in it.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

use crate::model::{Agent, DiskSample, ProcId};
use crate::paths::private_file;
use crate::session::{Part, Session, SessionId, SessionKey, SessionProcess, Source};
use crate::view::Evidence;

/// Append only. Each entry moves the schema up by one version, so version N means the
/// first N entries ran.
const MIGRATIONS: &[&str] = &[
    "CREATE TABLE disk_sample (
        id          INTEGER PRIMARY KEY,
        at_ms       INTEGER NOT NULL,
        volume      TEXT    NOT NULL,
        total_bytes INTEGER NOT NULL,
        free_bytes  INTEGER NOT NULL,
        avail_bytes INTEGER NOT NULL
    );
    CREATE INDEX disk_sample_volume_at ON disk_sample (volume, at_ms);
    CREATE TABLE audit (
        id     INTEGER PRIMARY KEY,
        at_ms  INTEGER NOT NULL,
        actor  TEXT    NOT NULL,
        action TEXT    NOT NULL,
        target TEXT    NOT NULL,
        result TEXT    NOT NULL
    );",
    // Sessions; the processes in them, which records an agent tree's ancestry while the
    // agent lives; and the spool events already taken.
    "CREATE TABLE session (
        agent         TEXT    NOT NULL,
        session_id    TEXT    NOT NULL,
        cwd           TEXT,
        started_at_ms INTEGER,
        ended_at_ms   INTEGER,
        end_reason    TEXT,
        source        TEXT    NOT NULL,
        PRIMARY KEY (agent, session_id)
    );
    CREATE TABLE session_process (
        agent         TEXT    NOT NULL,
        session_id    TEXT    NOT NULL,
        pid           INTEGER NOT NULL,
        start_us      INTEGER NOT NULL,
        role          TEXT    NOT NULL,
        first_seen_ms INTEGER NOT NULL,
        last_seen_ms  INTEGER NOT NULL,
        evidence      TEXT    NOT NULL,
        PRIMARY KEY (agent, session_id, pid, start_us)
    );
    CREATE INDEX session_process_proc ON session_process (pid, start_us);
    CREATE TABLE spool_event (
        id       TEXT    PRIMARY KEY,
        taken_ms INTEGER NOT NULL
    );",
];

pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// A store written by a newer Scry. Opening it would risk misreading or damaging it.
#[derive(Debug, PartialEq, Eq)]
pub struct SchemaTooNew {
    pub found: u32,
    pub known: u32,
}

impl fmt::Display for SchemaTooNew {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the store has schema {} but this scry knows only up to {}; upgrade scry",
            self.found, self.known
        )
    }
}

impl std::error::Error for SchemaTooNew {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRow {
    pub at_ms: i64,
    /// Who acted: `cli`, `tui`, `mcp`, `daemon`.
    pub actor: String,
    pub action: String,
    pub target: String,
    pub result: String,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens or creates the store as a 0600 file, in WAL mode, migrated to `SCHEMA_VERSION`.
    pub fn open(path: &Path) -> Result<Store> {
        Store::refuse_newer(path)?;
        private_file(path)?;
        let mut conn =
            Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&mut conn).with_context(|| format!("migrating {}", path.display()))?;
        Ok(Store { conn })
    }

    /// Opens an existing store without changing it: no file, table, mode or journal mode is
    /// created or altered, and an old schema is left unmigrated. Refuses a newer store.
    pub fn open_read_only(path: &Path) -> Result<Store> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {} read-only", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let store = Store { conn };
        let found = store.schema_version()?;
        if found > SCHEMA_VERSION {
            return Err(SchemaTooNew {
                found,
                known: SCHEMA_VERSION,
            }
            .into());
        }
        Ok(store)
    }

    /// Fails with `SchemaTooNew` if `path` holds a store from a newer scry, without touching
    /// it. A missing store passes.
    pub fn refuse_newer(path: &Path) -> Result<()> {
        if path.exists() {
            Store::open_read_only(path)?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<u32> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    pub fn insert_sample(&self, s: &DiskSample) -> Result<()> {
        self.conn.execute(
            "INSERT INTO disk_sample (at_ms, volume, total_bytes, free_bytes, avail_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                s.at_ms,
                s.volume,
                s.total_bytes as i64,
                s.free_bytes as i64,
                s.avail_bytes as i64
            ],
        )?;
        Ok(())
    }

    pub fn sample_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM disk_sample", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// The sample written last. Insertion order, not `at_ms`, because a clock that moved back
    /// would leave older samples dated later than the newest one.
    pub fn latest_sample(&self) -> Result<Option<DiskSample>> {
        Ok(self
            .conn
            .query_row(
                "SELECT at_ms, volume, total_bytes, free_bytes, avail_bytes
                 FROM disk_sample ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok(DiskSample {
                        at_ms: r.get(0)?,
                        volume: r.get(1)?,
                        total_bytes: r.get::<_, i64>(2)? as u64,
                        free_bytes: r.get::<_, i64>(3)? as u64,
                        avail_bytes: r.get::<_, i64>(4)? as u64,
                    })
                },
            )
            .optional()?)
    }

    pub fn append_audit(&self, a: &AuditRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO audit (at_ms, actor, action, target, result) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![a.at_ms, a.actor, a.action, a.target, a.result],
        )?;
        Ok(())
    }

    pub fn audit_rows(&self) -> Result<Vec<AuditRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT at_ms, actor, action, target, result FROM audit ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(AuditRow {
                at_ms: r.get(0)?,
                actor: r.get(1)?,
                action: r.get(2)?,
                target: r.get(3)?,
                result: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds a session or fills in what it learned. A field already known is kept, so a
    /// SessionEnd arriving after a scan fills the end without losing the start; a hook's
    /// start and directory replace a scan's guess at them.
    pub fn upsert_session(&self, s: &Session) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session (agent, session_id, cwd, started_at_ms, ended_at_ms, end_reason, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (agent, session_id) DO UPDATE SET
               cwd = CASE WHEN excluded.source = 'hook'
                     THEN coalesce(excluded.cwd, cwd) ELSE coalesce(cwd, excluded.cwd) END,
               started_at_ms = CASE WHEN excluded.source = 'hook'
                     THEN coalesce(excluded.started_at_ms, started_at_ms)
                     ELSE coalesce(started_at_ms, excluded.started_at_ms) END,
               ended_at_ms = coalesce(ended_at_ms, excluded.ended_at_ms),
               end_reason = coalesce(end_reason, excluded.end_reason),
               source = CASE WHEN excluded.source = 'hook' THEN 'hook' ELSE source END",
            params![
                s.key.agent.short(),
                s.key.id.encode(),
                s.cwd,
                s.started_at_ms,
                s.ended_at_ms,
                s.end_reason,
                s.source.word()
            ],
        )?;
        Ok(())
    }

    /// Sessions that are open, or ended at or after `since_ms`.
    pub fn sessions(&self, since_ms: i64) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT agent, session_id, cwd, started_at_ms, ended_at_ms, end_reason, source
             FROM session WHERE ended_at_ms IS NULL OR ended_at_ms >= ?1",
        )?;
        let rows = stmt.query_map([since_ms], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (agent, id, cwd, started_at_ms, ended_at_ms, end_reason, source) = row?;
            let (Some(key), Some(source)) = (key(&agent, &id), Source::parse(&source)) else {
                continue;
            };
            out.push(Session {
                key,
                cwd,
                started_at_ms,
                ended_at_ms,
                end_reason,
                source,
            });
        }
        Ok(out)
    }

    /// Records that a process was seen in a session, keeping when it was first seen.
    pub fn record_process(&self, m: &SessionProcess) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_process
               (agent, session_id, pid, start_us, role, first_seen_ms, last_seen_ms, evidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (agent, session_id, pid, start_us) DO UPDATE SET
               last_seen_ms = max(last_seen_ms, excluded.last_seen_ms),
               evidence = excluded.evidence",
            params![
                m.session.agent.short(),
                m.session.id.encode(),
                m.proc.pid,
                m.proc.start_us,
                m.part.word(),
                m.first_seen_ms,
                m.last_seen_ms,
                m.evidence.word()
            ],
        )?;
        Ok(())
    }

    /// Session processes last seen at or after `since_ms`.
    pub fn session_processes(&self, since_ms: i64) -> Result<Vec<SessionProcess>> {
        let mut stmt = self.conn.prepare(
            "SELECT agent, session_id, pid, start_us, role, first_seen_ms, last_seen_ms, evidence
             FROM session_process WHERE last_seen_ms >= ?1",
        )?;
        let rows = stmt.query_map([since_ms], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, String>(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (agent, id, pid, start_us, part, first_seen_ms, last_seen_ms, evidence) = row?;
            let (Some(session), Some(part), Some(evidence)) = (
                key(&agent, &id),
                Part::parse(&part),
                Evidence::parse(&evidence),
            ) else {
                continue;
            };
            out.push(SessionProcess {
                session,
                proc: ProcId { pid, start_us },
                part,
                first_seen_ms,
                last_seen_ms,
                evidence,
            });
        }
        Ok(out)
    }

    /// Marks a spool event as taken. False when it was taken before, which makes ingest
    /// idempotent.
    pub fn take_event(&self, id: &str, now_ms: i64) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO spool_event (id, taken_ms) VALUES (?1, ?2)",
            params![id, now_ms],
        )?;
        Ok(n == 1)
    }

    /// Forgets session processes, ended sessions and taken event ids older than `before_ms`.
    pub fn prune(&self, before_ms: i64) -> Result<()> {
        for sql in [
            "DELETE FROM session_process WHERE last_seen_ms < ?1",
            "DELETE FROM session WHERE ended_at_ms < ?1",
            "DELETE FROM spool_event WHERE taken_ms < ?1",
        ] {
            self.conn.execute(sql, [before_ms])?;
        }
        Ok(())
    }

    /// Runs `f` in one transaction, so the rows of one scan land together.
    pub fn batch<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(self) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

fn key(agent: &str, id: &str) -> Option<SessionKey> {
    Some(SessionKey {
        agent: Agent::from_short(agent)?,
        id: SessionId::decode(id)?,
    })
}

fn migrate(conn: &mut Connection) -> Result<()> {
    // IMMEDIATE takes the write lock before reading the version, so two processes opening
    // an old store cannot both run the same migration.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found: u32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(SchemaTooNew {
            found,
            known: SCHEMA_VERSION,
        }
        .into());
    }
    for sql in &MIGRATIONS[found as usize..] {
        tx.execute_batch(sql)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn schema(path: &Path) -> (u32, Vec<(String, String, Option<String>)>) {
        let c = Connection::open(path).unwrap();
        let v = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        let mut stmt = c
            .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
            .unwrap();
        let objects = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        (v, objects)
    }

    fn sample(at_ms: i64) -> DiskSample {
        DiskSample {
            at_ms,
            volume: "/".into(),
            total_bytes: 1 << 40,
            free_bytes: 1 << 38,
            avail_bytes: 1 << 37,
        }
    }

    #[test]
    fn migrating_twice_gives_the_same_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("scry.db");
        drop(Store::open(&db).unwrap());
        let first = schema(&db);
        assert_eq!(first.0, SCHEMA_VERSION);
        assert!(first.1.iter().any(|o| o.1 == "disk_sample"));
        assert!(first.1.iter().any(|o| o.1 == "audit"));
        let store = Store::open(&db).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        drop(store);
        assert_eq!(schema(&db), first);
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn refuses_a_newer_store_before_changing_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("scry.db");
        // What a newer scry might leave: rollback journal, mode 0644, a higher version.
        let c = Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE future (x); PRAGMA user_version = 99;")
            .unwrap();
        drop(c);
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
        let before = std::fs::read(&db).unwrap();

        for open in [Store::open, Store::open_read_only] {
            let err = open(&db).err().expect("refused");
            assert_eq!(
                err.downcast_ref::<SchemaTooNew>(),
                Some(&SchemaTooNew {
                    found: 99,
                    known: SCHEMA_VERSION
                })
            );
        }
        assert_eq!(std::fs::read(&db).unwrap(), before, "same bytes");
        assert_eq!(mode(&db), 0o644, "same mode");
        assert_eq!(files(tmp.path()), ["scry.db"], "no -wal or -shm");
    }

    #[test]
    fn read_only_open_creates_and_migrates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("scry.db");
        assert!(
            Store::open_read_only(&db).is_err(),
            "a missing store stays missing"
        );
        assert_eq!(files(tmp.path()), Vec::<String>::new());

        std::fs::write(&db, "").unwrap();
        let store = Store::open_read_only(&db).unwrap();
        assert_eq!(store.schema_version().unwrap(), 0);
        drop(store);
        assert_eq!(
            std::fs::metadata(&db).unwrap().len(),
            0,
            "an empty store stays empty"
        );

        std::fs::remove_file(&db).unwrap();
        let store = Store::open(&db).unwrap();
        store.insert_sample(&sample(1)).unwrap();
        drop(store);
        let before = std::fs::read(&db).unwrap();
        let store = Store::open_read_only(&db).unwrap();
        assert_eq!(store.sample_count().unwrap(), 1);
        assert!(store.insert_sample(&sample(2)).is_err(), "read-only");
        drop(store);
        assert_eq!(std::fs::read(&db).unwrap(), before);
        // A WAL reader needs the index files, which SQLite creates with the store's mode.
        assert_eq!(
            std::fs::metadata(tmp.path().join("scry.db-wal"))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(mode(&tmp.path().join("scry.db-shm")), 0o600);
    }

    #[test]
    fn samples_and_audit_round_trip_in_private_files() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("scry.db");
        let store = Store::open(&db).unwrap();
        assert_eq!(store.latest_sample().unwrap(), None);
        store.insert_sample(&sample(2_000)).unwrap();
        store.insert_sample(&sample(1_000)).unwrap();
        assert_eq!(store.sample_count().unwrap(), 2);
        assert_eq!(
            store.latest_sample().unwrap(),
            Some(sample(1_000)),
            "the last written, though dated earlier"
        );

        let row = AuditRow {
            at_ms: 5,
            actor: "cli".into(),
            action: "daemon install".into(),
            target: "com.morozred.scry.daemon".into(),
            result: "ok".into(),
        };
        store.append_audit(&row).unwrap();
        assert_eq!(store.audit_rows().unwrap(), vec![row]);

        for f in ["scry.db", "scry.db-wal", "scry.db-shm"] {
            assert_eq!(mode(&tmp.path().join(f)), 0o600, "{f}");
        }
    }
}
