//! The keep list: which entry names a process, and adding one to `config.toml` without
//! touching anything else in the file.
//!
//! An entry matches a process by its name, its executable's full path, the real path of
//! the script an interpreter runs, or that script in one working directory, written
//! `<script> in <dir>`. `~` stands for the home directory in each path. `K` and
//! `scry keep` pick the narrowest that is stable:
//!
//! - for a program that runs on its own, its executable path, such as `~/bin/tunnel`, which
//!   keeps that program however it is started;
//! - for an interpreter or shell (`node`, `python3`, `bash`, …), the script it runs, since
//!   its executable would keep every script: `~/acme/server.js`, or for a script outside
//!   the working directory, such as a tool installed once and run in many projects,
//!   `/opt/homebrew/lib/node_modules/vite/bin/vite.js in ~/acme`;
//! - nothing for code that is not a file, such as `python3 -c …`, `node -e …` or a module
//!   run with `-m`: no entry would keep that process and not others. Such a process is
//!   kept by hand.
//!
//! The edit changes only the `keep` array: comments, spacing, key order and the other
//! settings stay as they were. A config that cannot be read is never edited. A keep edit
//! only ever spares processes, so it is allowed under `destructive = false`.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use toml::Spanned;

use crate::config::Config;
use crate::label::{LiveFiles, OMITTED};
use crate::model::Process;
use crate::paths::Paths;

/// Executables that run a script or a command line they are given rather than a program of
/// their own. Matched by file name; `python` also covers `python3.12` and Python.app.
const RUNNERS: [&str; 22] = [
    "node", "bun", "deno", "python", "Python", "ruby", "perl", "php", "java", "sh", "bash", "zsh",
    "dash", "fish", "uv", "uvx", "npm", "npx", "pnpm", "yarn", "tsx", "ts-node",
];

/// Interpreter flags after which the code or module, not a file, says what runs.
const NOT_A_FILE: [&str; 7] = ["-c", "-e", "--eval", "-p", "--print", "-m", "--module"];

/// Splits `<script> in <dir>`.
const IN: &str = " in ";

/// Whether an executable, by its file name, runs scripts it is given.
pub fn is_runner(exe: &str) -> bool {
    let file = exe.rsplit('/').next().unwrap_or(exe);
    RUNNERS.contains(&file) || file.starts_with("python") || file.starts_with("Python")
}

/// The real path of the script an interpreter runs: the first argument, after its own
/// flags, that names an existing file. `None` when the code comes from a flag (`-c`, `-e`,
/// `-m`, …) first, or no argument is a file. Only local volumes are looked at.
pub fn script_of(argv: &[String], cwd: Option<&Path>, files: &LiveFiles) -> Option<String> {
    // A title written over the arguments has no argument boundaries.
    if let [title] = argv
        && title.contains(char::is_whitespace)
    {
        return None;
    }
    for a in argv.iter().skip(1).take(crate::label::MAX_ARGS) {
        if NOT_A_FILE.contains(&a.as_str()) {
            return None;
        }
        if let Some(cluster) = a.strip_prefix('-') {
            // `-uc` is `-u -c`: a cluster that may hold a code or module flag ends the search.
            if !cluster.starts_with('-') && cluster.contains(['c', 'e', 'm', 'p']) {
                return None;
            }
            continue;
        }
        let path = Path::new(a);
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd?.join(path)
        };
        if let Some(real) = files.real_file(&full) {
            return Some(real.display().to_string());
        }
    }
    None
}

/// Whether a process matches a keep entry: its name, its executable's path, its script's
/// real path, or its script in one working directory, exactly. `entry` has `~` expanded,
/// as [`expand`] does.
pub fn matches(p: &Process, entry: &str) -> bool {
    if let Some((script, dir)) = split_in(entry) {
        return p.script.as_deref() == Some(script) && p.cwd.as_deref() == Some(dir);
    }
    entry == p.name || p.exe.as_deref() == Some(entry) || p.script.as_deref() == Some(entry)
}

/// `<script> in <dir>`, when both are paths.
fn split_in(entry: &str) -> Option<(&str, &str)> {
    let (script, dir) = entry.rsplit_once(IN)?;
    let path = |s: &str| s.starts_with('/') || s.starts_with("~/");
    (path(script) && path(dir)).then_some((script, dir))
}

/// An entry as written, with `~` in each of its paths made the home directory.
pub fn expand(entry: &str, paths: &Paths) -> String {
    let one = |s: &str| paths.expand(Path::new(s)).display().to_string();
    match split_in(entry) {
        Some((script, dir)) => format!("{}{IN}{}", one(script), one(dir)),
        None => one(entry),
    }
}

/// What `K` and `scry keep` would add for a process, or why they add nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepAs {
    Entry(String),
    /// No entry would keep only this process. The sentence says why and what to do.
    Refused(String),
}

impl KeepAs {
    pub fn entry(&self) -> Option<&str> {
        match self {
            KeepAs::Entry(e) => Some(e),
            KeepAs::Refused(_) => None,
        }
    }
}

/// The entry that keeps `p`, as it is written into the config, with `~` for `home`.
pub fn entry_for(p: &Process, home: Option<&Path>) -> KeepAs {
    let tilde = |path: &str| match home.and_then(|h| Path::new(path).strip_prefix(h).ok()) {
        Some(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        Some(_) => "~".into(),
        None => path.to_string(),
    };
    let label = p.label();
    let by_hand = |why: &str| {
        KeepAs::Refused(format!(
            "`{label}` {why}, so no entry would keep only it. Add one to `keep` by hand."
        ))
    };
    let Some(exe) = p.exe.as_deref() else {
        return by_hand("has no executable path Scry can read");
    };
    if !is_runner(exe) {
        return KeepAs::Entry(tilde(exe));
    }
    let Some(script) = p.script.as_deref() else {
        return by_hand("runs code that is not a file");
    };
    match p.cwd.as_deref() {
        Some(dir) if Path::new(script).starts_with(dir) => KeepAs::Entry(tilde(script)),
        Some(dir) => KeepAs::Entry(format!("{}{IN}{}", tilde(script), tilde(dir))),
        None => by_hand("runs a script outside a working directory Scry can read"),
    }
}

/// A planned change to the keep list, shown before it is written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KeepEdit {
    pub config: PathBuf,
    pub entry: String,
    /// The entry is on the list already, so there is nothing to write.
    pub already: bool,
    /// The first line the change touches, from 1.
    pub line: usize,
    /// The lines as they are, empty when `keep` is not in the file yet.
    pub before: Vec<String>,
    /// The lines as they will be.
    pub after: Vec<String>,
    #[serde(skip)]
    old_text: Option<String>,
    #[serde(skip)]
    new_text: String,
}

impl KeepEdit {
    /// `- old` and `+ new` lines, as a diff shows them.
    pub fn diff(&self) -> Vec<String> {
        let mut out: Vec<String> = self.before.iter().map(|l| format!("- {l}")).collect();
        out.extend(self.after.iter().map(|l| format!("+ {l}")));
        out
    }
}

#[derive(Deserialize)]
struct KeepOnly {
    keep: Option<Spanned<Vec<Spanned<String>>>>,
}

/// Works out how adding `entry` changes the config at `paths.config`, without writing.
pub fn plan(paths: &Paths, entry: &str) -> Result<KeepEdit> {
    let path = paths.config.clone();
    let old_text = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let text = old_text.clone().unwrap_or_default();
    if entry.trim().is_empty() || entry.contains(OMITTED) {
        bail!("`{entry}` leaves parts out, so it would keep other processes; nothing was written");
    }
    // A broken file is left for the user to fix: an edit could make it worse.
    let config = Config::parse(&text, paths, &path)?;
    let found: KeepOnly = toml::from_str(&text).context("reading the keep list")?;
    if config.keep.iter().any(|k| k == entry) {
        let line = found.keep.as_ref().map_or(1, |list| {
            text[..list.span().start].matches('\n').count() + 1
        });
        return Ok(KeepEdit {
            config: path,
            entry: entry.into(),
            already: true,
            line,
            before: Vec::new(),
            after: Vec::new(),
            new_text: text.clone(),
            old_text,
        });
    }
    let quoted = toml::Value::String(entry.into()).to_string();
    let had_key = found.keep.is_some();
    let (new_text, changed) = match found.keep {
        Some(list) => {
            let span = list.span();
            let replace = match list.get_ref().last() {
                // After the last entry, so a trailing comma or comment stays where it is.
                Some(last) => last.span().end..last.span().end,
                None => span.clone(),
            };
            let insert = match list.get_ref().last() {
                Some(_) => format!(", {quoted}"),
                None => format!("[{quoted}]"),
            };
            let mut t = text.clone();
            t.replace_range(replace.clone(), &insert);
            let new_end = span.end + insert.len() - (replace.end - replace.start);
            (t, (span.start, span.end, new_end))
        }
        None => {
            // Before the first table, where a top-level key belongs.
            let at = table_start(&text).unwrap_or(text.len());
            let mut t = text.clone();
            let mut line = format!("keep = [{quoted}]\n");
            if at == text.len() && !text.is_empty() && !text.ends_with('\n') {
                line.insert(0, '\n');
            }
            let start = at + line.len() - line.trim_start_matches('\n').len();
            t.insert_str(at, &line);
            (t, (start, start, start + "keep = []".len() + quoted.len()))
        }
    };
    let (start, old_end, new_end) = changed;
    let reread = Config::parse(&new_text, paths, &path)
        .context("the edited config does not read back; nothing was written")?;
    if !reread.keep.iter().any(|k| k == entry) || reread.keep.len() != config.keep.len() + 1 {
        bail!("the edited config does not hold the new entry; nothing was written");
    }
    let lines = |t: &str, from: usize, to: usize| -> (usize, Vec<String>) {
        let first = t[..from].matches('\n').count();
        let last = t[..to.max(from)].matches('\n').count();
        let all: Vec<&str> = t.lines().collect();
        let picked = all
            .get(first..=last.min(all.len().saturating_sub(1)))
            .unwrap_or(&[])
            .iter()
            .map(|s| s.to_string())
            .collect();
        (first + 1, picked)
    };
    let before = if had_key {
        lines(&text, start, old_end).1
    } else {
        Vec::new()
    };
    let (line, after) = lines(&new_text, start, new_end);
    Ok(KeepEdit {
        config: path,
        entry: entry.into(),
        already: false,
        line,
        before,
        after,
        old_text,
        new_text,
    })
}

/// The byte offset of the first `[table]` header line.
fn table_start(text: &str) -> Option<usize> {
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with('[') {
            return Some(at);
        }
        at += line.len();
    }
    None
}

/// Writes a planned edit, unless the file changed since it was planned.
///
/// Two Scry processes take turns through a lock on the config's directory. The new text
/// goes to a temporary file of this call's own, beside the config, and is flushed to disk;
/// the config is read once more right before the rename and nothing is written if it
/// changed. A symlinked `config.toml` is written through to its target, so the link stays.
/// An editor that saves between that last read and the rename, a few microseconds, is the
/// one change this cannot see.
pub fn apply(edit: &KeepEdit) -> Result<()> {
    apply_then(edit, || {})
}

/// [`apply`], with an audit row before the write and one after, as every action Scry
/// takes. Nothing is written when the first row cannot be.
pub fn apply_audited(edit: &KeepEdit, paths: &Paths, actor: &str) -> Result<()> {
    if edit.already {
        return Ok(());
    }
    let store = crate::reap::audit_store(paths)
        .map_err(|r| anyhow::anyhow!("{}; nothing was written", r.reason()))?;
    let row = |result: String| crate::store::AuditRow {
        at_ms: crate::sys::now_ms(),
        actor: actor.into(),
        action: "keep".into(),
        target: format!("{} `{}`", paths.tilde(&edit.config), edit.entry),
        result,
    };
    store
        .append_audit(&row(format!("adding at line {}", edit.line)))
        .context("the audit row could not be written; nothing was written")?;
    let result = apply(edit);
    let said = match &result {
        Ok(()) => format!("added at line {}", edit.line),
        Err(e) => format!("not written: {e:#}"),
    };
    let after = store.append_audit(&row(said));
    result?;
    after.context("the keep list was written, but its audit row was not")
}

/// [`apply`], running `meanwhile` after the new text is written and before the config is
/// read the last time, so a test can change the file in between.
fn apply_then(edit: &KeepEdit, meanwhile: impl FnOnce()) -> Result<()> {
    if edit.already {
        return Ok(());
    }
    let target = crate::claude_hooks::resolve(&edit.config);
    let dir = target
        .parent()
        .context("the config has no directory")?
        .to_path_buf();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let _lock = DirLock::take(&dir)?;
    let unchanged = || -> Result<bool> {
        match std::fs::read_to_string(&target) {
            Ok(t) => Ok(Some(t) == edit.old_text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(edit.old_text.is_none()),
            Err(e) => Err(e).with_context(|| format!("reading {}", target.display())),
        }
    };
    let changed = || {
        anyhow::anyhow!(
            "{} changed since the edit was shown; nothing was written",
            edit.config.display()
        )
    };
    if !unchanged()? {
        return Err(changed());
    }
    let mode = std::fs::metadata(&target)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o644);
    let tmp = temp_beside(&target, &edit.new_text, mode)?;
    meanwhile();
    match unchanged() {
        Ok(true) => {}
        other => {
            let _ = std::fs::remove_file(&tmp);
            other?;
            return Err(changed());
        }
    }
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", target.display()));
    }
    // The rename is on disk once the directory is.
    std::fs::File::open(&dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("flushing {}", dir.display()))?;
    Ok(())
}

/// A new file beside `target` holding `text`, flushed to disk, named for this process and
/// this call, so no other writer opens it.
fn temp_beside(target: &Path, text: &str, mode: u32) -> Result<PathBuf> {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let name = target
        .file_name()
        .map_or("config.toml".into(), |n| n.to_string_lossy());
    loop {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = target.with_file_name(format!(
            ".{name}.scry-{}-{}-{n}",
            std::process::id(),
            crate::sys::now_ms()
        ));
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp);
        let mut f = match opened {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", tmp.display())),
        };
        let written = f.write_all(text.as_bytes()).and_then(|()| f.sync_all());
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("writing {}", tmp.display()));
        }
        return Ok(tmp);
    }
}

/// An exclusive `flock` on a directory, released when dropped.
struct DirLock(std::fs::File);

impl DirLock {
    fn take(dir: &Path) -> Result<DirLock> {
        let f = std::fs::File::open(dir).with_context(|| format!("opening {}", dir.display()))?;
        loop {
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(DirLock(f));
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e).with_context(|| format!("locking {}", dir.display()));
            }
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &Path) -> Paths {
        Paths::new(dir.join("home"), Some(dir.join("scry")))
    }

    fn write(p: &Paths, text: &str) {
        std::fs::create_dir_all(p.config.parent().unwrap()).unwrap();
        std::fs::write(&p.config, text).unwrap();
    }

    fn read(p: &Paths) -> String {
        std::fs::read_to_string(&p.config).unwrap()
    }

    #[test]
    fn an_entry_joins_the_list_and_nothing_else_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let text = "# my settings\ncode_roots = [\"~/work\"]   # projects\nkeep = [\"postgres\"] # db\n\n[tui]\nrefresh_secs = 5\n";
        write(&p, text);
        let edit = plan(&p, "next dev").unwrap();
        assert_eq!(edit.line, 3);
        assert_eq!(
            edit.diff(),
            [
                "- keep = [\"postgres\"] # db",
                "+ keep = [\"postgres\", \"next dev\"] # db"
            ]
        );
        assert_eq!(read(&p), text, "planning writes nothing");
        apply(&edit).unwrap();
        assert_eq!(
            read(&p),
            text.replace("[\"postgres\"]", "[\"postgres\", \"next dev\"]")
        );
        assert!(plan(&p, "next dev").unwrap().already);
    }

    #[test]
    fn a_multi_line_list_keeps_its_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let text = "keep = [\n  \"postgres\",  # db\n  \"redis-server\",\n]\n";
        write(&p, text);
        let edit = plan(&p, "~/bin/tunnel").unwrap();
        apply(&edit).unwrap();
        assert_eq!(
            read(&p),
            "keep = [\n  \"postgres\",  # db\n  \"redis-server\", \"~/bin/tunnel\",\n]\n"
        );
        assert_eq!(edit.line, 1);
        assert_eq!(edit.before.len(), 4);
        assert_eq!(edit.after.len(), 4);
    }

    #[test]
    fn a_missing_key_or_file_is_added_before_the_first_table() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let edit = plan(&p, "a \"quoted\" one").unwrap();
        assert!(edit.before.is_empty());
        assert_eq!(edit.after, ["keep = ['a \"quoted\" one']"]);
        apply(&edit).unwrap();
        assert_eq!(read(&p), "keep = ['a \"quoted\" one']\n");
        assert_eq!(Config::load(&p).unwrap().0.keep, ["a \"quoted\" one"]);

        let text = "destructive = true\n[daemon]\nsample_interval_secs = 60\n";
        write(&p, text);
        apply(&plan(&p, "x").unwrap()).unwrap();
        assert_eq!(
            read(&p),
            "destructive = true\nkeep = [\"x\"]\n[daemon]\nsample_interval_secs = 60\n"
        );
        write(&p, "keep = []\ndestructive = true");
        apply(&plan(&p, "x").unwrap()).unwrap();
        assert_eq!(read(&p), "keep = [\"x\"]\ndestructive = true");
        write(&p, "destructive = true");
        apply(&plan(&p, "x").unwrap()).unwrap();
        assert_eq!(read(&p), "destructive = true\nkeep = [\"x\"]\n");
    }

    #[test]
    fn a_broken_or_changed_file_is_not_written() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        write(&p, "keep = [\"a\"\n");
        assert!(plan(&p, "x").is_err());
        assert_eq!(read(&p), "keep = [\"a\"\n");

        write(&p, "keep = [\"a\"]\n");
        let edit = plan(&p, "x").unwrap();
        write(&p, "keep = [\"a\", \"b\"]\n");
        assert!(apply(&edit).unwrap_err().to_string().contains("changed"));
        assert_eq!(read(&p), "keep = [\"a\", \"b\"]\n");
    }

    #[test]
    fn the_line_of_an_entry_already_kept_is_the_keep_key_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        // Line 2 starts with `keep` inside a string; the key is on line 3.
        write(&p, "code_roots = ['''\nkeep''']\nkeep = [\"x\"]\n");
        let edit = plan(&p, "x").unwrap();
        assert!(edit.already);
        assert_eq!(edit.line, 3);
    }

    #[test]
    fn a_label_with_parts_left_out_is_never_written() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        write(&p, "keep = []\n");
        let e = plan(&p, "python3 -c …").unwrap_err().to_string();
        assert!(e.contains("would keep other processes"), "{e}");
        assert_eq!(read(&p), "keep = []\n");
    }

    #[test]
    fn a_symlinked_config_is_written_through_to_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let real = tmp.path().join("dotfiles/scry.toml");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "keep = [\"a\"]\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::create_dir_all(p.config.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &p.config).unwrap();
        apply(&plan(&p, "b").unwrap()).unwrap();
        assert!(
            std::fs::symlink_metadata(&p.config)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "keep = [\"a\", \"b\"]\n"
        );
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let strays: Vec<_> = std::fs::read_dir(real.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "scry.toml")
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
    }

    #[test]
    fn a_save_made_while_the_edit_is_written_is_kept_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        write(&p, "keep = [\"a\"]\n");
        let edit = plan(&p, "x").unwrap();
        let e = apply_then(&edit, || write(&p, "keep = [\"a\"]\ndestructive = false\n"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("changed"), "{e}");
        assert_eq!(read(&p), "keep = [\"a\"]\ndestructive = false\n");
        let strays = std::fs::read_dir(p.config.parent().unwrap())
            .unwrap()
            .count();
        assert_eq!(strays, 1, "the temporary file is gone");
    }

    #[test]
    fn concurrent_edits_take_turns_and_only_the_first_is_written() {
        for _ in 0..20 {
            let tmp = tempfile::tempdir().unwrap();
            let p = paths(tmp.path());
            write(&p, "keep = [\"a\"]\n");
            let long = plan(&p, &"y".repeat(400)).unwrap();
            let short = plan(&p, "x").unwrap();
            let results: Vec<bool> = std::thread::scope(|s| {
                let a = s.spawn(|| apply(&long).is_ok());
                let b = s.spawn(|| apply(&short).is_ok());
                vec![a.join().unwrap(), b.join().unwrap()]
            });
            assert_eq!(results.iter().filter(|ok| **ok).count(), 1, "{results:?}");
            let text = read(&p);
            let winner = if results[0] { &long } else { &short };
            assert_eq!(text, winner.new_text, "one whole edit, nothing mixed");
            assert!(Config::load(&p).is_ok());
        }
    }

    fn process(name: &str, exe: &str, script: Option<&str>, cwd: Option<&str>) -> Process {
        let mut p: Process = serde_json::from_value(serde_json::json!({
            "id": {"pid": 500, "start_us": 1},
            "ppid": 1,
            "uid": 501,
            "name": name,
            "exe": exe,
            "footprint": null,
        }))
        .unwrap();
        p.script = script.map(String::from);
        p.cwd = cwd.map(String::from);
        p
    }

    #[test]
    fn programs_are_kept_by_path_and_scripts_by_their_real_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let home = p.user_home.display().to_string();
        let acme = format!("{home}/acme");
        let entry = |proc: &Process| entry_for(proc, Some(&p.user_home));

        let tunnel = process("tunnel", &format!("{home}/bin/tunnel"), None, None);
        assert_eq!(entry(&tunnel), KeepAs::Entry("~/bin/tunnel".into()));

        let server = process(
            "node",
            "/opt/homebrew/bin/node",
            Some(&format!("{acme}/server.js")),
            Some(&acme),
        );
        assert_eq!(entry(&server), KeepAs::Entry("~/acme/server.js".into()));

        // A tool installed once and run in many projects is kept in this one.
        let vite = "/opt/homebrew/lib/node_modules/vite/bin/vite.js";
        let here = process("node", "/opt/homebrew/bin/node", Some(vite), Some(&acme));
        assert_eq!(entry(&here), KeepAs::Entry(format!("{vite} in ~/acme")));
        let elsewhere = process(
            "node",
            "/opt/homebrew/bin/node",
            Some(vite),
            Some(&format!("{home}/other")),
        );
        let other_script = process(
            "node",
            "/opt/homebrew/bin/node",
            Some(&format!("{acme}/worker.js")),
            Some(&acme),
        );

        for (proc, written) in [
            (&tunnel, "~/bin/tunnel".to_string()),
            (&server, "~/acme/server.js".into()),
            (&here, format!("{vite} in ~/acme")),
        ] {
            let e = expand(&written, &p);
            assert!(matches(proc, &e), "{e}");
            for other in [&tunnel, &server, &here, &elsewhere, &other_script] {
                if !std::ptr::eq(other, proc) {
                    assert!(!matches(other, &e), "{e} keeps {:?}", other.script);
                }
            }
        }
        assert!(matches(&server, "node"), "by name, as before");
    }

    #[test]
    fn code_run_by_an_interpreter_has_no_entry_that_could_keep_other_code() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let mut one = process("python3", "/usr/bin/python3", None, Some("/tmp"));
        one.command = Some(serde_json::from_value(serde_json::json!("python3 -c …")).unwrap());
        let other = one.clone();
        let KeepAs::Refused(why) = entry_for(&one, Some(&p.user_home)) else {
            panic!("python3 -c has no stable entry");
        };
        assert!(
            why.contains("`python3 -c …` runs code that is not a file"),
            "{why}"
        );
        assert!(why.contains("by hand"), "{why}");
        assert!(!matches(&other, "python3 -c …"), "a label is not an entry");
    }

    #[test]
    fn the_script_is_the_first_file_argument_unless_code_comes_first() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(dir.join("server.js"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("server.js"), dir.join("link.js")).unwrap();
        std::fs::create_dir(dir.join("dev")).unwrap();
        let files = LiveFiles::new();
        let script = |argv: &[&str]| {
            let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            script_of(&argv, Some(&dir), &files)
        };
        let server = Some(dir.join("server.js").display().to_string());
        assert_eq!(script(&["node", "--inspect", "link.js"]), server);
        assert_eq!(script(&["deno", "run", "server.js"]), server);
        let abs = dir.join("server.js").display().to_string();
        assert_eq!(script(&["python3", "-u", &abs, "--port", "1"]), server);
        for none in [
            &["node", "-e", "x", "server.js"][..],
            &["python3", "-m", "http.server"],
            &["python3", "-uc", "x", "server.js"],
            &["bun", "run", "dev"],
            &["npm run dev server.js"],
        ] {
            assert_eq!(script(none), None, "{none:?}");
        }
    }
}
