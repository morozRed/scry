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

/// The managed preferences domain an MDM profile sets.
pub const PREFERENCES_DOMAIN: &str = "com.anthropic.claudecode";

/// The first Claude Code that runs exec-form hooks (`command` plus `args`). An older one
/// runs `command` alone, which would print `scry`'s overview into every session.
pub const MIN_EXEC_FORM: (u32, u32, u32) = (2, 1, 139);

/// Every place managed settings can come from that Scry can read: MDM preference files,
/// device-wide first, then the managed-settings file and its drop-in directory.
#[derive(Clone, Debug, Default)]
pub struct ManagedSources {
    pub preferences: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
}

impl ManagedSources {
    /// The macOS locations, for the user named `user`.
    pub fn macos(user: Option<&str>) -> ManagedSources {
        let prefs = Path::new("/Library/Managed Preferences");
        let mut preferences = vec![prefs.join(format!("{PREFERENCES_DOMAIN}.plist"))];
        if let Some(u) = user.filter(|u| !u.is_empty() && !u.contains('/')) {
            preferences.push(prefs.join(u).join(format!("{PREFERENCES_DOMAIN}.plist")));
        }
        ManagedSources {
            preferences,
            files: MANAGED.map(PathBuf::from).to_vec(),
        }
    }
}

/// Whether the Claude Code that `claude --version` described runs exec-form hooks. An
/// answer Scry cannot read is a no, with why.
pub fn check_exec_form(version_output: &str) -> std::result::Result<(), String> {
    let need = format!(
        "{}.{}.{}",
        MIN_EXEC_FORM.0, MIN_EXEC_FORM.1, MIN_EXEC_FORM.2
    );
    let word = version_output.split_whitespace().next().unwrap_or("");
    let parts: Vec<u32> = word.split('.').map_while(|p| p.parse().ok()).collect();
    let [a, b, c] = parts[..] else {
        return Err(format!(
            "could not read Claude Code's version from `{}`; Scry's hooks need {need} or newer",
            version_output.trim()
        ));
    };
    if (a, b, c) < MIN_EXEC_FORM {
        return Err(format!(
            "Claude Code {word} runs a hook's `command` without its `args`, so the hook would print Scry's overview into every session. Update Claude Code to {need} or newer, then install again"
        ));
    }
    Ok(())
}

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

/// The keys that stop user hooks from running.
const BLOCKING: [&str; 2] = ["allowManagedHooksOnly", "disableAllHooks"];

/// Reads the managed settings for the keys that would stop a user hook, as Claude Code
/// applies them: the first source that sets any key wins whole (MDM preferences before the
/// files), unless one sets `managedSourcesBehavior` to `merge`, when any source's `true`
/// holds. Within a source, later files replace earlier values. A file that cannot be read
/// is an error, since it could hold either key.
pub fn managed_blockers(sources: &ManagedSources) -> Result<Vec<Blocker>> {
    let mut prefs = Vec::new();
    for p in sources.preferences.iter().filter(|p| p.is_file()) {
        prefs.push((p.clone(), read_plist(p)?));
    }
    let files = read_managed_files(&sources.files)?;
    let merge = prefs
        .iter()
        .chain(&files)
        .any(|(_, v)| v.get("managedSourcesBehavior") == Some(&json!("merge")));
    let delivers = |list: &[(PathBuf, Value)]| {
        list.iter()
            .any(|(_, v)| v.as_object().is_some_and(|m| !m.is_empty()))
    };
    let chosen: Vec<&[(PathBuf, Value)]> = if merge {
        vec![&prefs, &files]
    } else if delivers(&prefs) {
        vec![&prefs]
    } else {
        vec![&files]
    };
    let mut out = Vec::new();
    for key in BLOCKING {
        for source in &chosen {
            // The last file in a source that sets the key decides it.
            if let Some((file, _)) = source
                .iter()
                .rev()
                .find(|(_, v)| v.get(key).is_some())
                .filter(|(_, v)| v.get(key) == Some(&Value::Bool(true)))
            {
                out.push(Blocker {
                    file: file.clone(),
                    key,
                });
            }
        }
    }
    Ok(out)
}

/// An MDM preference file as JSON, by `plutil`, which reads the binary form too.
fn read_plist(path: &Path) -> Result<Value> {
    let out = std::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(path)
        .output()
        .with_context(|| format!("running plutil on {}", path.display()))?;
    if !out.status.success() {
        bail!(
            "reading {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).with_context(|| format!("parsing {}", path.display()))
}

fn read_managed_files(places: &[PathBuf]) -> Result<Vec<(PathBuf, Value)>> {
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
        out.push((f, v));
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
    /// new text to a temporary file and renames it over, keeping the old file's mode. A
    /// symlinked `settings.json` is written through to its target, so the link stays.
    /// Returns the backup's path, `None` when there was no file to back up.
    pub fn apply(&self, now_ms: i64) -> Result<Option<PathBuf>> {
        self.apply_then(now_ms, || {})
    }

    /// [`Plan::apply`], running `meanwhile` after the new text is written and before the
    /// file is compared and replaced, so a test can change the file in between.
    fn apply_then(&self, now_ms: i64, meanwhile: impl FnOnce()) -> Result<Option<PathBuf>> {
        if !self.changes() {
            return Ok(None);
        }
        let target = resolve(&self.path);
        let dir = target.parent().context("settings.json has no directory")?;
        fs::create_dir_all(dir)?;
        let mode = fs::metadata(&target)
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
        let name = target
            .file_name()
            .map_or("settings.json".into(), |n| n.to_string_lossy());
        let tmp = target.with_file_name(format!(".{name}.scry-{now_ms}"));
        write_new(&tmp, &self.after, mode)?;
        meanwhile();
        // Compared last, so a save made while the files above were written is not lost.
        if fs::read_to_string(&target).ok() != self.before {
            let _ = fs::remove_file(&tmp);
            if let Some(b) = &backup {
                let _ = fs::remove_file(b);
            }
            bail!(
                "{} changed while Scry was working; nothing was written. Run the command again",
                self.path.display()
            );
        }
        fs::rename(&tmp, &target).with_context(|| format!("replacing {}", target.display()))?;
        Ok(backup)
    }
}

/// The file a chain of symlinks ends at, which may not exist yet.
fn resolve(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    for _ in 0..40 {
        match fs::read_link(&p) {
            Ok(t) if t.is_absolute() => p = t,
            Ok(t) => p = p.parent().unwrap_or(Path::new("/")).join(t),
            Err(_) => break,
        }
    }
    p
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
    let original = parse(&before)?;
    let mut root = original.clone();
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
    // Installed already: the text stays as it is, however it is laid out.
    let after = match &before {
        Some(text) if root == original => text.clone(),
        _ => render(&root, before.as_deref()),
    };
    Ok(Plan {
        path: path.to_path_buf(),
        after,
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
        after: render(&root, before.as_deref()),
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

/// The settings as JSON, indented as `before` was (two spaces when it was not), with its
/// keys in their order and its final newline kept or left out, so the diff stays small.
fn render(root: &Map<String, Value>, before: Option<&str>) -> String {
    let indent = before.and_then(indent_of).unwrap_or("  ");
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(
        &mut buf,
        serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes()),
    );
    serde::Serialize::serialize(root, &mut ser).expect("a JSON value renders");
    let mut s = String::from_utf8(buf).expect("JSON is UTF-8");
    if before.is_none_or(|b| b.trim().is_empty() || b.ends_with('\n')) {
        s.push('\n');
    }
    s
}

/// The whitespace before the first indented line, which is a top-level key's indent.
fn indent_of(text: &str) -> Option<&str> {
    text.lines().skip(1).find_map(|l| {
        let rest = l.trim_start_matches([' ', '\t']);
        (!rest.is_empty() && rest.len() < l.len()).then(|| &l[..l.len() - rest.len()])
    })
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
        let sources = ManagedSources {
            preferences: vec![tmp.path().join("no.plist")],
            files: vec![file.clone(), dir.clone(), tmp.path().join("missing.json")],
        };
        let found = managed_blockers(&sources).unwrap();
        assert_eq!(
            found,
            [Blocker {
                file: dir.join("10-hooks.json"),
                key: "allowManagedHooksOnly"
            }]
        );
        fs::write(&file, "{").unwrap();
        assert!(
            managed_blockers(&sources).is_err(),
            "unreadable means unknown"
        );
    }

    #[test]
    fn a_later_managed_file_overrides_an_earlier_one() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("managed-settings.json");
        let dir = tmp.path().join("managed-settings.d");
        fs::create_dir(&dir).unwrap();
        fs::write(&file, r#"{"allowManagedHooksOnly": true}"#).unwrap();
        fs::write(
            dir.join("20-team.json"),
            r#"{"allowManagedHooksOnly": false}"#,
        )
        .unwrap();
        let sources = ManagedSources {
            preferences: Vec::new(),
            files: vec![file.clone(), dir.clone()],
        };
        assert_eq!(managed_blockers(&sources).unwrap(), []);
        fs::write(
            dir.join("30-lock.json"),
            r#"{"allowManagedHooksOnly": true}"#,
        )
        .unwrap();
        assert_eq!(
            managed_blockers(&sources).unwrap(),
            [Blocker {
                file: dir.join("30-lock.json"),
                key: "allowManagedHooksOnly"
            }]
        );
    }

    fn plist(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join(format!("{PREFERENCES_DOMAIN}.plist"));
        fs::write(
            &p,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>{body}</dict></plist>
"#
            ),
        )
        .unwrap();
        p
    }

    #[test]
    fn mdm_preferences_are_read_and_win_over_the_files() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("managed-settings.json");
        fs::write(&file, r#"{"disableAllHooks": true}"#).unwrap();
        let locked = plist(tmp.path(), "<key>allowManagedHooksOnly</key><true/>");
        let sources = ManagedSources {
            preferences: vec![locked.clone()],
            files: vec![file.clone()],
        };
        assert_eq!(
            managed_blockers(&sources).unwrap(),
            [Blocker {
                file: locked.clone(),
                key: "allowManagedHooksOnly"
            }],
            "the MDM source delivers keys, so the files are not read"
        );
        // An MDM profile that sets other keys still wins whole, so the file's lock is off.
        plist(tmp.path(), "<key>model</key><string>opus</string>");
        assert_eq!(managed_blockers(&sources).unwrap(), []);
        // Unless a source asks for the sources to merge: then any lock holds.
        plist(
            tmp.path(),
            "<key>managedSourcesBehavior</key><string>merge</string>",
        );
        assert_eq!(
            managed_blockers(&sources).unwrap(),
            [Blocker {
                file: file.clone(),
                key: "disableAllHooks"
            }]
        );
    }

    #[test]
    fn exec_form_needs_a_recent_claude_code() {
        assert!(check_exec_form("2.1.322 (Claude Code)").is_ok());
        assert!(check_exec_form("2.1.139 (Claude Code)").is_ok());
        assert!(check_exec_form("3.0.0").is_ok());
        let old = check_exec_form("2.1.138 (Claude Code)").unwrap_err();
        assert!(old.contains("2.1.139 or newer"), "{old}");
        assert!(check_exec_form("").is_err());
        assert!(check_exec_form("command not found").is_err());
    }

    #[test]
    fn apply_writes_through_a_symlinked_settings_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dotfiles = tmp.path().join("dotfiles/claude");
        fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("settings.json");
        fs::write(&target, existing()).unwrap();
        let link = tmp.path().join(".claude/settings.json");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("../dotfiles/claude/settings.json", &link).unwrap();
        let plan = plan_install(&link, Some(existing()), Path::new(SCRY)).unwrap();
        let backup = plan.apply(9).unwrap().unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), plan.after);
        assert_eq!(
            backup.parent(),
            link.parent(),
            "the backup sits by the link"
        );
        assert_eq!(fs::read_to_string(&backup).unwrap(), existing());
        assert_eq!(
            fs::read_dir(&dotfiles).unwrap().count(),
            1,
            "no temp file left"
        );
    }

    #[test]
    fn a_change_saved_while_applying_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, existing()).unwrap();
        let plan = plan_install(&path, Some(existing()), Path::new(SCRY)).unwrap();
        let theirs = existing().replace("opus", "sonnet");
        let err = plan
            .apply_then(10, || fs::write(&path, &theirs).unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
        let left: Vec<_> = fs::read_dir(tmp.path()).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "no backup or temp file left: {left:?}");
    }

    #[test]
    fn install_keeps_the_files_indent_and_final_newline() {
        let p = Path::new("/x/settings.json");
        let four = "{\n    \"model\": \"opus\",\n    \"permissions\": {\n        \"allow\": [\n            \"Bash(ls:*)\"\n        ]\n    }\n}";
        let plan = plan_install(p, Some(four.into()), Path::new(SCRY)).unwrap();
        assert!(!plan.after.ends_with('\n'), "none before, none after");
        assert!(
            plan.after
                .contains("\n    \"hooks\": {\n        \"SessionStart\"")
        );
        assert!(
            !plan.diff().lines().any(|l| l.starts_with('-')),
            "only added lines:\n{}",
            plan.diff()
        );
        let back = plan_uninstall(p, Some(plan.after.clone())).unwrap();
        assert_eq!(back.after, four);
        let tabs = "{\n\t\"model\": \"opus\"\n}\n";
        let plan = plan_install(p, Some(tabs.into()), Path::new(SCRY)).unwrap();
        assert!(
            plan.after
                .contains("\n\t\"hooks\": {\n\t\t\"SessionStart\"")
        );
        // Installed already, in any layout: nothing to write.
        let squeezed: Value = serde_json::from_str(&plan.after).unwrap();
        let compact = serde_json::to_string(&squeezed).unwrap();
        assert!(
            !plan_install(p, Some(compact), Path::new(SCRY))
                .unwrap()
                .changes()
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
