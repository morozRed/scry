//! Installing Scry's Claude Code hooks into `settings.json`, and taking them out again.
//!
//! The hooks are exec-form (`command` plus `args`, so no shell parses them) on SessionStart
//! and SessionEnd. They run `scry hook run claude <event>`, which prints nothing, always
//! exits 0 and appends one line to the spool. Scry never installs a `WorktreeCreate` hook:
//! that hook replaces git's own worktree creation.
//!
//! Everything here is a plan first ([`Plan`]): the text before and after, so the CLI can show
//! the diff, back the file up and write it in one rename. Keys keep their order, and any
//! hook that is not Scry's is left as it was.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// The events Scry hooks, in the order they are written.
pub const EVENTS: [&str; 2] = ["SessionStart", "SessionEnd"];

/// Where Claude Code keeps managed settings on macOS: the file, then the drop-in directory
/// whose `*.json` files merge after it.
pub const MANAGED: [&str; 2] = [
    "/Library/Application Support/ClaudeCode/managed-settings.json",
    "/Library/Application Support/ClaudeCode/managed-settings.d",
];

/// The user's `settings.json`: in `$CLAUDE_CONFIG_DIR` when that is set, else `~/.claude`.
pub fn settings_path(home: &Path, config_dir: Option<&Path>) -> PathBuf {
    config_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".claude"))
        .join("settings.json")
}

/// A managed setting that would stop user hooks from running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocker {
    pub file: PathBuf,
    pub key: &'static str,
}

impl Blocker {
    pub fn sentence(&self) -> String {
        format!(
            "{} sets {} to true, which stops hooks in your own settings.json from running.",
            self.file.display(),
            self.key
        )
    }
}

/// Reads the managed settings in `places` (files, and directories of `*.json`) for the keys
/// that would override a user hook. A file that cannot be parsed is reported as an error,
/// since it could hold either.
pub fn managed_blockers(places: &[PathBuf]) -> Result<Vec<Blocker>> {
    let mut files = Vec::new();
    for p in places {
        if p.is_dir() {
            let mut inside: Vec<PathBuf> = fs::read_dir(p)?
                .flatten()
                .map(|e| e.path())
                .filter(|f| {
                    f.extension().is_some_and(|x| x == "json")
                        && !f
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                })
                .collect();
            inside.sort();
            files.extend(inside);
        } else if p.is_file() {
            files.push(p.clone());
        }
    }
    let mut out = Vec::new();
    for f in files {
        let text = fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?;
        let v: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", f.display()))?;
        for key in ["allowManagedHooksOnly", "disableAllHooks"] {
            if v.get(key) == Some(&Value::Bool(true)) {
                out.push(Blocker {
                    file: f.clone(),
                    key,
                });
            }
        }
    }
    Ok(out)
}

/// One change to `settings.json`, not yet written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub path: PathBuf,
    /// The file as it is, `None` when it does not exist.
    pub before: Option<String>,
    pub after: String,
}

impl Plan {
    pub fn changes(&self) -> bool {
        self.before.as_deref() != Some(self.after.as_str())
    }

    /// The change as unified-diff lines: ` ` kept, `-` removed, `+` added.
    pub fn diff(&self) -> String {
        diff(self.before.as_deref().unwrap_or(""), &self.after)
    }

    /// Backs the old file up beside it as `settings.json.scry-backup-<ms>`, then writes the
    /// new text to a temporary file and renames it over, keeping the old file's mode.
    /// Returns the backup's path, `None` when there was no file to back up.
    pub fn apply(&self, now_ms: i64) -> Result<Option<PathBuf>> {
        if !self.changes() {
            return Ok(None);
        }
        let dir = self
            .path
            .parent()
            .context("settings.json has no directory")?;
        fs::create_dir_all(dir)?;
        let current = fs::read_to_string(&self.path).ok();
        if current != self.before {
            bail!(
                "{} changed while Scry was planning; run the command again",
                self.path.display()
            );
        }
        let mode = fs::metadata(&self.path)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        let backup = match &self.before {
            Some(text) => {
                let b = self
                    .path
                    .with_file_name(format!("settings.json.scry-backup-{now_ms}"));
                write_new(&b, text, mode)?;
                Some(b)
            }
            None => None,
        };
        let tmp = self
            .path
            .with_file_name(format!(".settings.json.scry-{now_ms}"));
        write_new(&tmp, &self.after, mode)?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(backup)
    }
}

fn write_new(path: &Path, text: &str, mode: u32) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

/// Adds Scry's two hooks, replacing any Scry hooks already there (from another path, say).
pub fn plan_install(path: &Path, before: Option<String>, scry: &Path) -> Result<Plan> {
    let mut root = parse(&before)?;
    remove_ours(&mut root);
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks) = hooks else {
        bail!("`hooks` in {} is not an object", path.display());
    };
    for event in EVENTS {
        let list = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(list) = list else {
            bail!("`hooks.{event}` in {} is not a list", path.display());
        };
        list.push(json!({
            "hooks": [{
                "type": "command",
                "command": scry.display().to_string(),
                "args": ["hook", "run", "claude", event],
                "timeout": 5
            }]
        }));
    }
    Ok(Plan {
        path: path.to_path_buf(),
        after: render(&root),
        before,
    })
}

/// Removes Scry's hooks and nothing else. Lists and objects left empty by that are removed.
pub fn plan_uninstall(path: &Path, before: Option<String>) -> Result<Plan> {
    let mut root = parse(&before)?;
    if !remove_ours(&mut root) {
        return Ok(Plan {
            path: path.to_path_buf(),
            after: before.clone().unwrap_or_default(),
            before,
        });
    }
    Ok(Plan {
        path: path.to_path_buf(),
        after: render(&root),
        before,
    })
}

/// Whether `settings.json` holds Scry's hook on every event.
pub fn installed(text: Option<&str>) -> bool {
    let Some(Value::Object(root)) = text.and_then(|t| serde_json::from_str(t).ok()) else {
        return false;
    };
    EVENTS.iter().all(|e| {
        root.get("hooks")
            .and_then(|h| h.get(*e))
            .and_then(Value::as_array)
            .is_some_and(|groups| {
                groups.iter().any(|g| {
                    g.get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(|hs| hs.iter().any(ours))
                })
            })
    })
}

fn parse(text: &Option<String>) -> Result<Map<String, Value>> {
    match text.as_deref().map(str::trim) {
        None | Some("") => Ok(Map::new()),
        Some(t) => match serde_json::from_str(t).context("settings.json is not valid JSON")? {
            Value::Object(m) => Ok(m),
            _ => bail!("settings.json is not a JSON object"),
        },
    }
}

fn render(root: &Map<String, Value>) -> String {
    let mut s = serde_json::to_string_pretty(root).expect("a JSON value renders");
    s.push('\n');
    s
}

/// A hook entry Scry wrote: a `scry` executable run with `hook run claude`.
fn ours(h: &Value) -> bool {
    let file = h
        .get("command")
        .and_then(Value::as_str)
        .map(|c| c.rsplit('/').next().unwrap_or(c));
    let args: Vec<&str> = h
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    file == Some("scry") && args.starts_with(&["hook", "run", "claude"])
}

/// Takes Scry's entries out of every event. True when anything was removed.
fn remove_ours(root: &mut Map<String, Value>) -> bool {
    let Some(Value::Object(hooks)) = root.get_mut("hooks") else {
        return false;
    };
    let mut removed = false;
    for groups in hooks.values_mut() {
        let Value::Array(groups) = groups else {
            continue;
        };
        for g in groups.iter_mut() {
            if let Some(Value::Array(hs)) = g.get_mut("hooks") {
                let n = hs.len();
                hs.retain(|h| !ours(h));
                removed |= hs.len() != n;
            }
        }
        groups.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|hs| !hs.is_empty())
        });
    }
    if removed {
        hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
        if hooks.is_empty() {
            root.remove("hooks");
        }
    }
    removed
}

/// A line diff by longest common subsequence. Settings files are small.
pub fn diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = String::new();
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            out.push_str(&format!("  {}\n", a[i]));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            out.push_str(&format!("- {}\n", a[i]));
            i += 1;
        } else {
            out.push_str(&format!("+ {}\n", b[j]));
            j += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRY: &str = "/opt/homebrew/bin/scry";

    fn existing() -> String {
        r#"{
  "model": "opus",
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "~/bin/check-bash" }]
      }
    ],
    "SessionStart": [
      {
        "matcher": "startup",
        "hooks": [{ "type": "command", "command": "echo hi" }]
      }
    ]
  },
  "permissions": { "allow": ["Bash(ls:*)"] }
}
"#
        .into()
    }

    fn value(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn install_adds_exec_form_hooks_and_keeps_everything_else_in_order() {
        let p = Path::new("/x/settings.json");
        let plan = plan_install(p, Some(existing()), Path::new(SCRY)).unwrap();
        assert!(plan.changes());
        let v = value(&plan.after);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "hooks", "permissions"], "order kept");
        let start = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 2);
        assert_eq!(
            start[0]["hooks"][0]["command"], "echo hi",
            "theirs first, untouched"
        );
        assert_eq!(start[1]["hooks"][0]["command"], SCRY);
        assert_eq!(
            start[1]["hooks"][0]["args"],
            json!(["hook", "run", "claude", "SessionStart"])
        );
        assert!(
            start[1].get("matcher").is_none(),
            "every source: startup, resume, clear"
        );
        assert_eq!(
            v["hooks"]["SessionEnd"][0]["hooks"][0]["args"][3],
            "SessionEnd"
        );
        assert!(v["hooks"].get("WorktreeCreate").is_none());
        assert_eq!(
            v["hooks"]["PreToolUse"],
            value(&existing())["hooks"]["PreToolUse"]
        );
        assert!(installed(Some(&plan.after)));
        assert!(!installed(Some(&existing())));

        let again = plan_install(p, Some(plan.after.clone()), Path::new(SCRY)).unwrap();
        assert!(!again.changes(), "installing twice changes nothing");
        let moved = plan_install(
            p,
            Some(plan.after.clone()),
            Path::new("/usr/local/bin/scry"),
        )
        .unwrap();
        let v = value(&moved.after);
        assert_eq!(
            v["hooks"]["SessionStart"].as_array().unwrap().len(),
            2,
            "replaced"
        );
    }

    #[test]
    fn uninstall_removes_only_scrys_hooks() {
        let p = Path::new("/x/settings.json");
        let installed_text = plan_install(p, Some(existing()), Path::new(SCRY))
            .unwrap()
            .after;
        let plan = plan_uninstall(p, Some(installed_text)).unwrap();
        assert_eq!(value(&plan.after), value(&existing()));
        let plan = plan_uninstall(p, Some(existing())).unwrap();
        assert!(!plan.changes(), "nothing of Scry's to remove");

        let fresh = plan_install(p, None, Path::new(SCRY)).unwrap();
        let back = plan_uninstall(p, Some(fresh.after)).unwrap();
        assert_eq!(back.after.trim(), "{}");
    }

    #[test]
    fn diff_marks_added_and_removed_lines() {
        assert_eq!(diff("a\nb\nc\n", "a\nx\nc\n"), "  a\n- b\n+ x\n  c\n");
        assert_eq!(diff("", "{}\n"), "+ {}\n");
    }

    #[test]
    fn broken_settings_are_refused_not_overwritten() {
        let p = Path::new("/x/settings.json");
        assert!(plan_install(p, Some("{ \"a\": 1, }".into()), Path::new(SCRY)).is_err());
        assert!(plan_install(p, Some("[]".into()), Path::new(SCRY)).is_err());
        assert!(plan_install(p, Some(r#"{"hooks": []}"#.into()), Path::new(SCRY)).is_err());
    }

    #[test]
    fn managed_settings_that_stop_user_hooks_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("managed-settings.json");
        let dir = tmp.path().join("managed-settings.d");
        fs::create_dir(&dir).unwrap();
        fs::write(&file, r#"{"permissions": {}}"#).unwrap();
        fs::write(
            dir.join("10-hooks.json"),
            r#"{"allowManagedHooksOnly": true}"#,
        )
        .unwrap();
        fs::write(dir.join(".hidden.json"), r#"{"disableAllHooks": true}"#).unwrap();
        fs::write(dir.join("notes.txt"), r#"{"disableAllHooks": true}"#).unwrap();
        let places = [file.clone(), dir.clone(), tmp.path().join("missing.json")];
        let found = managed_blockers(&places).unwrap();
        assert_eq!(
            found,
            [Blocker {
                file: dir.join("10-hooks.json"),
                key: "allowManagedHooksOnly"
            }]
        );
        fs::write(&file, "{").unwrap();
        assert!(
            managed_blockers(&places).is_err(),
            "unreadable means unknown"
        );
    }

    #[test]
    fn apply_backs_up_and_refuses_a_file_changed_meanwhile() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".claude/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, existing()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let plan = plan_install(&path, Some(existing()), Path::new(SCRY)).unwrap();
        let backup = plan.apply(7).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&backup).unwrap(), existing());
        assert_eq!(fs::read_to_string(&path).unwrap(), plan.after);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );

        let stale = plan_install(&path, Some(existing()), Path::new("/other/scry")).unwrap();
        assert!(stale.apply(8).is_err());
    }
}
