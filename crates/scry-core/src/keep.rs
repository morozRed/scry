//! The keep list: which entry names a process, and adding one to `config.toml` without
//! touching anything else in the file.
//!
//! An entry matches a process by its name, its executable's full path (`~` for the home
//! directory), or its whole command label. `K` and `scry keep` pick one:
//!
//! - for a program that runs on its own, its executable path, such as `~/bin/tunnel`, which
//!   keeps that program however it is started;
//! - for an interpreter or shell (`node`, `python3`, `bash`, …), its command label, such as
//!   `next dev`, since its executable would keep every script it runs.
//!
//! The edit changes only the `keep` array: comments, spacing, key order and the other
//! settings stay as they were. A config that cannot be read is never edited.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use toml::Spanned;

use crate::config::Config;
use crate::model::Process;
use crate::paths::Paths;

/// Executables that run a script or a command line they are given rather than a program of
/// their own. Matched by file name; `python` also covers `python3.12` and Python.app.
const RUNNERS: [&str; 22] = [
    "node", "bun", "deno", "python", "Python", "ruby", "perl", "php", "java", "sh", "bash", "zsh",
    "dash", "fish", "uv", "uvx", "npm", "npx", "pnpm", "yarn", "tsx", "ts-node",
];

/// Whether a process matches a keep entry: its name, its executable's path, or its command
/// label, exactly. `keep` holds entries with `~` already expanded.
pub fn matches(p: &Process, entry: &str) -> bool {
    entry == p.name
        || p.exe.as_deref() == Some(entry)
        || p.command.as_ref().is_some_and(|c| c.to_string() == entry)
}

/// The entry that keeps `p`, as it is written into the config, with `~` for `home`.
pub fn entry_for(p: &Process, home: Option<&Path>) -> String {
    let file = p
        .exe
        .as_deref()
        .map(|e| e.rsplit('/').next().unwrap_or(e))
        .unwrap_or(&p.name);
    let runner =
        RUNNERS.contains(&file) || file.starts_with("python") || file.starts_with("Python");
    match (&p.exe, &p.command) {
        (_, Some(label)) if runner => label.to_string(),
        (Some(exe), _) if !runner => match home.and_then(|h| Path::new(exe).strip_prefix(h).ok()) {
            Some(rest) => format!("~/{}", rest.display()),
            None => exe.clone(),
        },
        _ => p.label(),
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
    // A broken file is left for the user to fix: an edit could make it worse.
    let config = Config::parse(&text, paths, &path)?;
    if config.keep.iter().any(|k| k == entry) {
        let line = text
            .lines()
            .position(|l| l.trim_start().starts_with("keep"))
            .map_or(1, |i| i + 1);
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
    let found: KeepOnly = toml::from_str(&text).context("reading the keep list")?;
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
pub fn apply(edit: &KeepEdit) -> Result<()> {
    if edit.already {
        return Ok(());
    }
    let now = match std::fs::read_to_string(&edit.config) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", edit.config.display())),
    };
    if now != edit.old_text {
        bail!(
            "{} changed since the edit was shown; nothing was written",
            edit.config.display()
        );
    }
    if let Some(dir) = edit.config.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Written beside it and renamed over it, so a reader never sees half a file.
    let tmp = edit.config.with_extension("toml.scry-tmp");
    std::fs::write(&tmp, &edit.new_text).with_context(|| format!("writing {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(&edit.config) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &edit.config)
        .with_context(|| format!("replacing {}", edit.config.display()))?;
    Ok(())
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

    fn process(name: &str, exe: &str, label: Option<&str>) -> Process {
        let mut p: Process = serde_json::from_value(serde_json::json!({
            "id": {"pid": 500, "start_us": 1},
            "ppid": 1,
            "uid": 501,
            "name": name,
            "exe": exe,
            "footprint": null,
        }))
        .unwrap();
        p.command = label.map(|l| serde_json::from_value(serde_json::json!(l)).unwrap());
        p
    }

    #[test]
    fn programs_are_kept_by_path_and_scripts_by_label() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let home = p.user_home.display().to_string();
        let tunnel = process("tunnel", &format!("{home}/bin/tunnel"), Some("tunnel …"));
        assert_eq!(entry_for(&tunnel, Some(&p.user_home)), "~/bin/tunnel");
        let next = process("node", "/opt/homebrew/bin/node", Some("next dev"));
        assert_eq!(entry_for(&next, Some(&p.user_home)), "next dev");
        let py = process(
            "python3.12",
            "/usr/local/bin/python3.12",
            Some("uvicorn app"),
        );
        assert_eq!(entry_for(&py, Some(&p.user_home)), "uvicorn app");
        for (proc, entry) in [
            (&tunnel, format!("{home}/bin/tunnel")),
            (&next, "next dev".into()),
        ] {
            assert!(matches(proc, &entry));
        }
        assert!(matches(&next, "node"), "by name, as before");
        assert!(!matches(&next, "next"));
    }
}
