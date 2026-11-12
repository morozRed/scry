//! The daemon's life: `install` and `uninstall` around the LaunchAgent, `run` as one
//! instance per state directory sampling the root volume until a signal stops it, and
//! `Status` for what is true right now.

use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::config::Config;
use crate::daemon_log;
use crate::launch_agent::{
    LaunchAgent, Launchd, PlistContents, Spec, Step, plan_install, render_plist, stable_program,
};
use crate::model::{DiskSample, ProcId};
use crate::paths::{Paths, private_file};
use crate::store::{AuditRow, SCHEMA_VERSION, SchemaTooNew, Store};
use crate::{recorder, sys};

pub const SAMPLED_VOLUME: &str = "/";

/// A binary as it is on disk. A rebuild or an upgrade changes the path, the inode or the
/// modification time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binary {
    /// With symlinks resolved, so a repointed Homebrew link counts as a change.
    pub path: PathBuf,
    pub inode: u64,
    pub mtime_ns: i64,
}

impl Binary {
    pub fn of(path: &Path) -> Option<Binary> {
        let path = fs::canonicalize(path).ok()?;
        let m = fs::metadata(&path).ok()?;
        Some(Binary {
            inode: m.ino(),
            mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            path,
        })
    }
}

/// What a running daemon runs with. It writes this into `daemon.lock` when it starts, so
/// `install`, `status` and `doctor` can tell whether a restart would change anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runtime {
    pub pid: i32,
    pub start_us: i64,
    pub version: String,
    /// `None` if the daemon could not read its own executable.
    pub binary: Option<Binary>,
    /// `Config::fingerprint` of the config it loaded.
    pub config: String,
    pub interval_secs: u64,
    pub scry_home: Option<PathBuf>,
}

impl Runtime {
    /// This process, about to run the daemon with `config`.
    pub fn current(paths: &Paths, config: &Config) -> Result<Runtime> {
        let me = sys::proc_id(std::process::id() as i32).context("reading own start time")?;
        Ok(Runtime {
            pid: me.pid,
            start_us: me.start_us,
            version: env!("CARGO_PKG_VERSION").into(),
            binary: sys::exe_path(me.pid).and_then(|p| Binary::of(Path::new(&p))),
            config: config.fingerprint.clone(),
            interval_secs: config.daemon.sample_interval_secs,
            scry_home: paths.scry_home.clone(),
        })
    }

    /// Why a daemon started now, from `binary` with `config`, would differ from this one.
    /// Empty when a restart would change nothing. `None` skips that comparison.
    pub fn drift(
        &self,
        paths: &Paths,
        binary: Option<&Binary>,
        config: Option<&Config>,
    ) -> Vec<String> {
        let mut why = Vec::new();
        if let (Some(have), Some(want)) = (&self.binary, binary) {
            if have.path != want.path {
                why.push(format!(
                    "the binary changed from {} to {}",
                    paths.tilde(&have.path),
                    paths.tilde(&want.path)
                ));
            } else if have != want {
                why.push(format!("{} was replaced", paths.tilde(&have.path)));
            }
        }
        if let Some(c) = config {
            let want = c.daemon.sample_interval_secs;
            if want != self.interval_secs {
                why.push(format!(
                    "the sample interval changed from {} s to {want} s",
                    self.interval_secs
                ));
            } else if c.fingerprint != self.config {
                why.push(format!("{} changed", paths.tilde(&paths.config)));
            }
        }
        if paths.scry_home != self.scry_home {
            let show = |h: &Option<PathBuf>| match h {
                Some(h) => h.display().to_string(),
                None => "unset".into(),
            };
            why.push(format!(
                "SCRY_HOME changed from {} to {}",
                show(&self.scry_home),
                show(&paths.scry_home)
            ));
        }
        why
    }

    pub fn proc_id(&self) -> ProcId {
        ProcId {
            pid: self.pid,
            start_us: self.start_us,
        }
    }
}

/// `daemon.lock`, held with `flock` for the daemon's whole life. It holds the holder's
/// `Runtime` as JSON, so `status` can name it without touching the lock.
pub struct Lock {
    file: File,
}

#[derive(Debug, PartialEq, Eq)]
pub struct LockHeld {
    pub path: PathBuf,
    pub holder: Option<ProcId>,
}

impl fmt::Display for LockHeld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.holder {
            Some(h) => write!(
                f,
                "another scry daemon is already running as pid {} (it holds {})",
                h.pid,
                self.path.display()
            ),
            None => write!(
                f,
                "another scry daemon is already running (it holds {})",
                self.path.display()
            ),
        }
    }
}

impl std::error::Error for LockHeld {}

impl Lock {
    pub fn acquire(path: &Path, me: &Runtime) -> Result<Lock> {
        let mut file = private_file(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(LockHeld {
                    path: path.to_path_buf(),
                    holder: read_holder(&mut file).map(|r| r.proc_id()),
                }
                .into());
            }
            return Err(err).with_context(|| format!("locking {}", path.display()));
        }
        file.set_len(0)?;
        file.rewind()?;
        serde_json::to_writer(&mut file, me)?;
        file.flush()?;
        file.sync_data()?;
        Ok(Lock { file })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Emptied before the descriptor closes and releases the flock, so a reader never
        // sees a stale holder after a clean exit.
        let _ = self.file.set_len(0);
    }
}

fn read_holder(file: &mut File) -> Option<Runtime> {
    let mut text = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

/// The live process holding the lock, if any. Checks pid and start time, so a recycled pid
/// is not mistaken for the daemon.
pub fn running_holder(lock_path: &Path) -> Option<Runtime> {
    let mut file = File::open(lock_path).ok()?;
    let recorded = read_holder(&mut file)?;
    (sys::proc_id(recorded.pid) == Some(recorded.proc_id())).then_some(recorded)
}

#[derive(Debug)]
pub struct Installed {
    /// Empty when everything was already in place.
    pub steps: Vec<Step>,
    /// Why the running job was restarted, when `steps` holds `Step::Restart`.
    pub restarted_because: Vec<String>,
    pub pid: i32,
    pub program: PathBuf,
    /// Problems after the daemon was running, such as an audit row that was not written.
    pub warnings: Vec<String>,
}

/// Brings the LaunchAgent to "installed, enabled, loaded, running `exe` with the current
/// config" from whatever state it is in, and returns once that daemon holds its lock.
pub fn install(paths: &Paths, exe: &Path) -> Result<Installed> {
    // Refusals come before any change. A config or store the daemon cannot use would only
    // crash-loop under launchd, and a store from a newer scry must stay as it is.
    let (config, _) = Config::load(paths)?;
    Store::refuse_newer(&paths.db())?;
    paths.ensure_state()?;
    let store = Store::open(&paths.db())?;
    // launchd would create the log with its own umask.
    private_file(&paths.log())?;
    let program = stable_program(exe)?;
    let binary = Binary::of(&program);
    let agent = LaunchAgent::new(paths);
    let desired = render_plist(&Spec {
        label: agent.label.clone(),
        program: program.clone(),
        log: paths.log(),
        scry_home: paths.scry_home.clone(),
    });
    let obs = agent.observe()?;
    let drift = match (obs.launchd, running_holder(&paths.lock())) {
        (Launchd::Loaded { pid: Some(pid) }, Some(r)) if r.pid == pid => {
            r.drift(paths, binary.as_ref(), Some(&config))
        }
        _ => Vec::new(),
    };
    let steps = plan_install(&obs, &desired, !drift.is_empty());
    agent.apply(&steps, &desired)?;
    let pid = wait_until_current(paths, &agent, binary.as_ref(), &config)?;
    let mut warnings = Vec::new();
    if !steps.is_empty()
        && let Err(e) = audit(&store, "daemon install", &agent.label, &steps)
    {
        warnings.push(format!("the audit row was not written: {e:#}"));
    }
    Ok(Installed {
        restarted_because: if steps.contains(&Step::Restart) {
            drift
        } else {
            Vec::new()
        },
        steps,
        pid,
        program,
        warnings,
    })
}

/// How long install waits for the new daemon. launchd starts a job at most once per 10 s
/// (its default ThrottleInterval, which the plist keeps), so a job restarted soon after it
/// started can take 10 s to come back; twice that leaves room to take the lock.
const RESTART_WAIT: Duration = Duration::from_secs(20);

/// Waits until launchd's job holds the lock and runs `binary` with `config`, and returns
/// its pid. A restarted job takes a moment to stop, and its successor to take the lock.
fn wait_until_current(
    paths: &Paths,
    agent: &LaunchAgent,
    binary: Option<&Binary>,
    config: &Config,
) -> Result<i32> {
    let deadline = Instant::now() + RESTART_WAIT;
    loop {
        let pid = match agent.launchd()? {
            Launchd::Loaded { pid } => pid,
            Launchd::NotLoaded => None,
        };
        let holder = running_holder(&paths.lock());
        let drift = holder
            .as_ref()
            .map(|h| h.drift(paths, binary, Some(config)));
        match (pid, &holder, &drift) {
            (Some(pid), Some(h), Some(d)) if h.pid == pid && d.is_empty() => return Ok(pid),
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            (_, Some(h), _) if Some(h.pid) != pid => bail!(
                "pid {} already runs the daemon outside launchd; stop it and launchd's copy takes over",
                h.pid
            ),
            (_, Some(h), Some(d)) => bail!(
                "pid {} still runs the old setup ({}); see {}",
                h.pid,
                d.join("; "),
                paths.log().display()
            ),
            (Some(pid), _, _) => bail!(
                "launchd started pid {pid} but it has not taken the lock; see {}",
                paths.log().display()
            ),
            (None, _, _) => bail!(
                "launchd loaded {} but it is not running; see {}",
                agent.label,
                paths.log().display()
            ),
        }
    }
}

#[derive(Debug)]
pub struct Uninstalled {
    /// Empty when nothing was installed.
    pub steps: Vec<Step>,
    /// Problems after the agent was gone, such as an audit row that was not written.
    pub warnings: Vec<String>,
}

/// Brings the LaunchAgent to "not loaded, no plist". The store stays.
pub fn uninstall(paths: &Paths) -> Result<Uninstalled> {
    let agent = LaunchAgent::new(paths);
    let steps = agent.uninstall()?;
    let mut warnings = Vec::new();
    // The agent is gone by now, so a store this scry cannot write costs only the audit row.
    if !steps.is_empty() && paths.db().exists() {
        let written = Store::open(&paths.db())
            .and_then(|store| audit(&store, "daemon uninstall", &agent.label, &steps));
        if let Err(e) = written {
            warnings.push(format!("the audit row was not written: {e:#}"));
        }
    }
    Ok(Uninstalled { steps, warnings })
}

fn audit(store: &Store, action: &str, target: &str, steps: &[Step]) -> Result<()> {
    store.append_audit(&AuditRow {
        at_ms: sys::now_ms(),
        actor: "cli".into(),
        action: action.into(),
        target: target.into(),
        result: serde_json::to_string(steps)?,
    })
}

/// Runs the daemon in the foreground until SIGTERM or SIGINT.
pub fn run(paths: &Paths) -> Result<()> {
    daemon_log::attach(&paths.log());
    std::panic::set_hook(Box::new(|info| daemon_log::line(&format!("panic: {info}"))));
    let (config, _) = Config::load(paths)?;
    paths.ensure_state()?;
    let me = Runtime::current(paths, &config)?;
    let lock = Lock::acquire(&paths.lock(), &me)?;
    let store = Store::open(&paths.db())?;
    let interval = Duration::from_secs(me.interval_secs);

    let mut signals = Signals::new([SIGTERM, SIGINT])?;
    let handle = signals.handle();
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            let _ = tx.send(sig);
        }
    });

    daemon_log::line(&format!(
        "started as pid {} (scry {}), sampling {SAMPLED_VOLUME} every {} s and recording agent sessions into {}",
        me.pid,
        me.version,
        interval.as_secs(),
        paths.db().display()
    ));
    let (recorder, recording) = recorder::spawn(paths.clone())?;
    let sig = sample_until_stopped(&store, interval, &rx, || {
        let volume = Path::new(SAMPLED_VOLUME);
        sys::volume_space(volume).map(|mut s| {
            s.important_bytes = crate::disk::space::important(volume);
            s
        })
    });
    recorder.stop();
    let _ = recording.join();
    handle.close();
    let _ = waiter.join();
    drop(store);
    drop(lock);
    daemon_log::line(&format!("stopped by signal {sig}"));
    Ok(())
}

/// Samples now and then once per `interval` until a signal number arrives on `stop`.
/// Returns that signal, or 0 if the sender went away.
pub fn sample_until_stopped(
    store: &Store,
    interval: Duration,
    stop: &Receiver<i32>,
    mut sample: impl FnMut() -> Result<DiskSample>,
) -> i32 {
    // Waits are capped and measured on the wall clock because the monotonic clock stops
    // while the Mac sleeps, which would push the next sample a whole interval past wake.
    const MAX_WAIT: Duration = Duration::from_secs(60);
    let mut next = SystemTime::now();
    loop {
        let now = SystemTime::now();
        if now >= next {
            if let Err(e) = sample().and_then(|s| store.insert_sample(&s)) {
                daemon_log::line(&format!("sample failed: {e:#}"));
            }
            next = now + interval;
        } else if next.duration_since(now).unwrap_or_default() > interval {
            // The clock went backwards.
            next = now + interval;
        }
        let wait = next
            .duration_since(SystemTime::now())
            .unwrap_or_default()
            .min(MAX_WAIT);
        match stop.recv_timeout(wait) {
            Ok(sig) => return sig,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return 0,
        }
    }
}

/// Everything `daemon status` and `doctor` report about the daemon, gathered without
/// starting, creating, migrating or locking anything.
#[derive(Debug, Serialize)]
pub struct Status {
    pub label: String,
    pub plist: PathBuf,
    pub installed: bool,
    /// The installed plist as `plutil` reads it. `None` when missing or unreadable.
    pub plist_contents: Option<PlistContents>,
    #[serde(skip)]
    pub plist_mode: Option<u32>,
    /// launchd will not load the job until it is enabled again.
    pub disabled: bool,
    pub launchd: Launchd,
    /// The live lock holder, whether launchd started it or someone ran it in a terminal.
    pub running: Option<Runtime>,
    /// What the installed plist runs.
    pub program: Option<PathBuf>,
    pub program_exists: bool,
    /// Why the running daemon differs from one started now. Empty when nothing does.
    pub restart_needed: Vec<String>,
    pub store: StoreStatus,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum StoreStatus {
    Missing {
        path: PathBuf,
    },
    /// Older than this scry. The daemon migrates it when it starts.
    Outdated {
        path: PathBuf,
        schema: u32,
    },
    Ready {
        path: PathBuf,
        schema: u32,
        samples: u64,
        newest: Option<DiskSample>,
    },
    Unreadable {
        path: PathBuf,
        message: String,
        too_new: bool,
    },
}

impl StoreStatus {
    pub fn too_new(&self) -> bool {
        matches!(self, StoreStatus::Unreadable { too_new: true, .. })
    }
}

impl Status {
    pub fn gather(paths: &Paths) -> Result<Status> {
        let agent = LaunchAgent::new(paths);
        let observed = agent.observe()?;
        let plist_contents = observed.plist.as_ref().and_then(|_| agent.read_plist());
        let program = plist_contents
            .as_ref()
            .and_then(|c| c.args.first())
            .map(PathBuf::from);
        let running = running_holder(&paths.lock());
        let config = Config::load(paths).ok().map(|(c, _)| c);
        let restart_needed = running
            .as_ref()
            .map(|r| {
                let binary = program.as_deref().and_then(Binary::of);
                r.drift(paths, binary.as_ref(), config.as_ref())
            })
            .unwrap_or_default();
        Ok(Status {
            label: agent.label,
            installed: observed.plist.is_some(),
            plist_mode: observed.plist.map(|p| p.mode),
            plist_contents,
            disabled: observed.disabled,
            launchd: observed.launchd,
            running,
            program_exists: program.as_deref().is_some_and(Path::is_file),
            program,
            restart_needed,
            plist: agent.plist,
            store: store_status(&paths.db()),
        })
    }
}

fn store_status(path: &Path) -> StoreStatus {
    let path = path.to_path_buf();
    if !path.exists() {
        return StoreStatus::Missing { path };
    }
    let read = |path: PathBuf| -> Result<StoreStatus> {
        let store = Store::open_read_only(&path)?;
        let schema = store.schema_version()?;
        if schema < SCHEMA_VERSION {
            return Ok(StoreStatus::Outdated { path, schema });
        }
        Ok(StoreStatus::Ready {
            schema,
            samples: store.sample_count()?,
            newest: store.latest_sample()?,
            path,
        })
    };
    read(path.clone()).unwrap_or_else(|e| StoreStatus::Unreadable {
        path,
        too_new: e.downcast_ref::<SchemaTooNew>().is_some(),
        message: format!("{e:#}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DaemonConfig;

    fn me() -> Runtime {
        let id = sys::proc_id(std::process::id() as i32).unwrap();
        Runtime {
            pid: id.pid,
            start_us: id.start_us,
            version: "0.0.0".into(),
            binary: None,
            config: "c".into(),
            interval_secs: 600,
            scry_home: None,
        }
    }

    #[test]
    fn a_second_lock_is_refused_and_names_the_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.lock");
        let first = Lock::acquire(&path, &me()).unwrap();
        assert_eq!(running_holder(&path), Some(me()));

        let err = Lock::acquire(&path, &me())
            .err()
            .expect("second lock refused");
        let held = err.downcast_ref::<LockHeld>().expect("LockHeld");
        assert_eq!(held.holder, Some(me().proc_id()));
        assert!(err.to_string().contains("already running"));

        drop(first);
        assert_eq!(
            running_holder(&path),
            None,
            "clean release leaves no holder"
        );
        // A child that a parallel test is spawning holds a copy of the descriptor, and so
        // the flock, until it execs.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while Lock::acquire(&path, &me()).is_err() {
            assert!(std::time::Instant::now() < deadline, "lock never freed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_stale_holder_is_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.lock");
        let stale = Runtime {
            start_us: me().start_us + 1,
            ..me()
        };
        fs::write(&path, serde_json::to_string(&stale).unwrap()).unwrap();
        assert_eq!(running_holder(&path), None, "same pid, other start time");
        fs::write(&path, "garbage").unwrap();
        assert_eq!(running_holder(&path), None);
    }

    fn config(interval: u64, fingerprint: &str) -> Config {
        Config {
            code_roots: vec![],
            destructive: false,
            destructive_line: None,
            keep: vec![],
            leftover_grace: crate::attribution::DEFAULT_GRACE,
            daemon: DaemonConfig {
                sample_interval_secs: interval,
            },
            tui: Default::default(),
            fingerprint: fingerprint.into(),
        }
    }

    #[test]
    fn drift_names_each_change_that_a_restart_would_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("scry");
        fs::write(&exe, "1").unwrap();
        let paths = Paths::new("/Users/dev".into(), None);
        let running = Runtime {
            binary: Binary::of(&exe),
            ..me()
        };
        let same = config(600, "c");
        assert_eq!(
            running.drift(&paths, Binary::of(&exe).as_ref(), Some(&same)),
            Vec::<String>::new()
        );
        assert_eq!(running.drift(&paths, None, None), Vec::<String>::new());

        let why = running.drift(&paths, None, Some(&config(5, "d")));
        assert_eq!(why, ["the sample interval changed from 600 s to 5 s"]);
        let why = running.drift(&paths, None, Some(&config(600, "d")));
        assert_eq!(why, ["~/.config/scry/config.toml changed"]);

        let other = tmp.path().join("scry-2");
        fs::write(&other, "2").unwrap();
        let why = running.drift(&paths, Binary::of(&other).as_ref(), Some(&same));
        assert!(why[0].starts_with("the binary changed from "), "{why:?}");

        // Replaced in place, as `cargo build` or `mv` would: same path, new inode.
        fs::write(&other, "1").unwrap();
        fs::rename(&other, &exe).unwrap();
        let why = running.drift(&paths, Binary::of(&exe).as_ref(), Some(&same));
        assert_eq!(why.len(), 1);
        assert!(why[0].ends_with(" was replaced"), "{why:?}");

        let moved = Paths::new("/Users/dev".into(), Some("/tmp/x".into()));
        let why = running.drift(&moved, None, None);
        assert_eq!(why, ["SCRY_HOME changed from unset to /tmp/x"]);
    }

    #[test]
    fn install_waits_out_launchds_restart_throttle() {
        let plist = render_plist(&Spec {
            label: "x".into(),
            program: "/x/scry".into(),
            log: "/x/daemon.log".into(),
            scry_home: None,
        });
        assert!(
            !plist.contains("ThrottleInterval"),
            "launchd's 10 s default"
        );
        assert!(RESTART_WAIT >= 2 * Duration::from_secs(10));
    }

    #[test]
    fn samples_at_once_then_stops_on_a_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("scry.db")).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut calls = 0;
        let sig = sample_until_stopped(&store, Duration::from_millis(20), &rx, || {
            calls += 1;
            if calls == 3 {
                tx.send(SIGTERM).unwrap();
            }
            sys::volume_space(Path::new("/"))
        });
        assert_eq!(sig, SIGTERM);
        assert_eq!(store.sample_count().unwrap(), 3);
    }

    #[test]
    fn store_status_reads_without_creating_or_migrating() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("scry.db");
        assert!(matches!(store_status(&db), StoreStatus::Missing { .. }));
        assert!(!db.exists());

        fs::write(&db, "").unwrap();
        assert!(matches!(
            store_status(&db),
            StoreStatus::Outdated { schema: 0, .. }
        ));
        assert_eq!(fs::metadata(&db).unwrap().len(), 0, "not migrated");

        fs::remove_file(&db).unwrap();
        drop(Store::open(&db).unwrap());
        rusqlite::Connection::open(&db)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let s = store_status(&db);
        assert!(s.too_new(), "{s:?}");
    }
}
