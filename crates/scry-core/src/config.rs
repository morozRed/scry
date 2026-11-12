//! `config.toml`. A missing file means defaults. A broken one is an error that names the line.
//!
//! ```toml
//! code_roots = ["~/sources", "~/work"]   # where projects and worktrees live
//! destructive = false                      # false refuses every stop, reap and cleanup
//! keep = ["postgres", "~/bin/tunnel"]      # never selected by a rule
//! leftover_grace_secs = 300                # how old a leftover must be
//!
//! [daemon]
//! sample_interval_secs = 600               # disk sampling period
//!
//! [tui]
//! refresh_secs = 5                         # 1 to 60; 0 refreshes only on `r`
//! ```

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::{Paths, fnv1a};

pub const DEFAULT_CODE_ROOTS: [&str; 4] = ["~/sources", "~/code", "~/src", "~/Developer"];
pub const DEFAULT_SAMPLE_INTERVAL_SECS: u64 = 600;
pub const DEFAULT_REFRESH_SECS: u64 = 5;
pub const MAX_REFRESH_SECS: u64 = 60;
/// The longest leftover grace period the config takes: a week, as long as history is kept.
pub const MAX_GRACE_SECS: u64 = 7 * 86_400;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Absolute. The defaults are the ones in `DEFAULT_CODE_ROOTS` that exist.
    pub code_roots: Vec<PathBuf>,
    /// False turns every kill and trash path off, for people and agents alike. True when the
    /// file does not say: the confirmation sheet and `--yes` guard each action.
    pub destructive: bool,
    /// The line of `destructive = false`, so a refusal can point at it.
    pub destructive_line: Option<usize>,
    /// Process names or executable paths that no rule ever selects.
    pub keep: Vec<String>,
    /// How old a process must be before it can be a leftover.
    pub leftover_grace: std::time::Duration,
    pub daemon: DaemonConfig,
    pub tui: TuiConfig,
    /// Hex hash of the settings in the file, so a daemon started with other settings can be
    /// told apart. Comments, spacing and key order do not change it.
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonConfig {
    pub sample_interval_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiConfig {
    /// Seconds between live refreshes; 0 refreshes only when asked.
    pub refresh_secs: u64,
}

impl Default for TuiConfig {
    fn default() -> Self {
        TuiConfig {
            refresh_secs: DEFAULT_REFRESH_SECS,
        }
    }
}

/// Where a loaded config came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Defaults,
    File(PathBuf),
}

#[derive(Debug)]
pub struct ConfigError {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(n) => write!(f, "{} line {n}: {}", self.path.display(), self.message),
            None => write!(f, "{}: {}", self.path.display(), self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    code_roots: Option<Vec<PathBuf>>,
    destructive: Option<bool>,
    keep: Option<Vec<String>>,
    /// Left out of the fingerprint: only clients judge leftovers.
    #[serde(default, skip_serializing)]
    leftover_grace_secs: Option<Grace>,
    #[serde(default)]
    daemon: FileDaemon,
    /// Left out of the fingerprint: the daemon does not read it, so changing it must not
    /// ask for a daemon restart.
    #[serde(default, skip_serializing)]
    tui: FileTui,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileTui {
    refresh_secs: Option<Refresh>,
}

#[derive(Deserialize)]
#[serde(try_from = "u64")]
struct Refresh(u64);

impl TryFrom<u64> for Refresh {
    type Error = String;
    fn try_from(secs: u64) -> Result<Self, Self::Error> {
        if secs > MAX_REFRESH_SECS {
            Err(format!(
                "refresh_secs must be 0 (refresh only on `r`) or 1 to {MAX_REFRESH_SECS}"
            ))
        } else {
            Ok(Refresh(secs))
        }
    }
}

#[derive(Deserialize)]
#[serde(try_from = "u64")]
struct Grace(u64);

impl TryFrom<u64> for Grace {
    type Error = String;
    fn try_from(secs: u64) -> Result<Self, Self::Error> {
        if secs > MAX_GRACE_SECS {
            Err(format!("leftover_grace_secs must be 0 to {MAX_GRACE_SECS}"))
        } else {
            Ok(Grace(secs))
        }
    }
}

/// Where `destructive` is set, read apart from the settings so their fingerprint stays as it
/// was.
#[derive(Deserialize)]
struct DestructiveAt {
    destructive: Option<toml::Spanned<bool>>,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
struct FileDaemon {
    sample_interval_secs: Option<Interval>,
}

#[derive(Deserialize, Serialize)]
#[serde(try_from = "u64")]
struct Interval(u64);

impl TryFrom<u64> for Interval {
    type Error = &'static str;
    fn try_from(secs: u64) -> Result<Self, Self::Error> {
        if secs == 0 {
            Err("sample_interval_secs must be at least 1")
        } else {
            Ok(Interval(secs))
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<(Config, Source), ConfigError> {
        let text = match std::fs::read_to_string(&paths.config) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Config::parse("", paths, &paths.config)?, Source::Defaults));
            }
            Err(e) => {
                return Err(ConfigError {
                    path: paths.config.clone(),
                    line: None,
                    message: e.to_string(),
                });
            }
        };
        let config = Config::parse(&text, paths, &paths.config)?;
        Ok((config, Source::File(paths.config.clone())))
    }

    pub fn parse(text: &str, paths: &Paths, origin: &Path) -> Result<Config, ConfigError> {
        let file: FileConfig = toml::from_str(text).map_err(|e| ConfigError {
            path: origin.to_path_buf(),
            line: e.span().map(|s| text[..s.start].matches('\n').count() + 1),
            message: e.message().trim_end().to_string(),
        })?;
        let canonical = serde_json::to_vec(&file).expect("TOML strings are UTF-8");
        let fingerprint = format!("{:016x}", fnv1a(&canonical));
        let code_roots = match file.code_roots {
            Some(roots) => roots.iter().map(|r| paths.expand(r)).collect(),
            None => DEFAULT_CODE_ROOTS
                .iter()
                .map(|r| paths.expand(Path::new(r)))
                .filter(|r| r.is_dir())
                .collect(),
        };
        let destructive = file.destructive.unwrap_or(true);
        let destructive_line = (!destructive)
            .then(|| toml::from_str::<DestructiveAt>(text).ok()?.destructive)
            .flatten()
            .map(|s| text[..s.span().start].matches('\n').count() + 1);
        Ok(Config {
            code_roots,
            destructive,
            destructive_line,
            keep: file.keep.unwrap_or_default(),
            leftover_grace: std::time::Duration::from_secs(
                file.leftover_grace_secs
                    .map_or(crate::attribution::DEFAULT_GRACE.as_secs(), |g| g.0),
            ),
            daemon: DaemonConfig {
                sample_interval_secs: file
                    .daemon
                    .sample_interval_secs
                    .map_or(DEFAULT_SAMPLE_INTERVAL_SECS, |i| i.0),
            },
            tui: TuiConfig {
                refresh_secs: file.tui.refresh_secs.map_or(DEFAULT_REFRESH_SECS, |r| r.0),
            },
            fingerprint,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(home: &Path) -> Paths {
        Paths::new(home.into(), Some(home.join("scry")))
    }

    #[test]
    fn missing_file_means_defaults_with_existing_roots_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("code")).unwrap();
        let (c, src) = Config::load(&paths(tmp.path())).unwrap();
        assert_eq!(src, Source::Defaults);
        assert_eq!(c.code_roots, vec![tmp.path().join("code")]);
        assert!(c.destructive, "on unless the file turns it off");
        assert_eq!(c.destructive_line, None);
        assert_eq!(c.leftover_grace.as_secs(), 300);
        assert!(c.keep.is_empty());
        assert_eq!(c.daemon.sample_interval_secs, 600);
        assert_eq!(c.tui.refresh_secs, 5);
    }

    #[test]
    fn refresh_secs_takes_0_to_60_and_leaves_the_fingerprint_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let parse = |text: &str| Config::parse(text, &p, Path::new("/x/config.toml"));
        assert_eq!(parse("").unwrap().tui.refresh_secs, 5);
        assert_eq!(parse("[tui]\n").unwrap().tui.refresh_secs, 5);
        for secs in [0, 1, 2, 60] {
            let c = parse(&format!("[tui]\nrefresh_secs = {secs}\n")).unwrap();
            assert_eq!(c.tui.refresh_secs, secs);
            assert_eq!(c.fingerprint, parse("").unwrap().fingerprint);
        }
        for bad in ["61", "3600", "-1", "\"2\"", "1.5"] {
            let e = parse(&format!("[tui]\nrefresh_secs = {bad}\n")).unwrap_err();
            assert_eq!(e.line, Some(2), "{bad}: {e}");
        }
        let e = parse("[tui]\nrefresh_secs = 61\n").unwrap_err();
        assert!(
            e.to_string().contains("0 (refresh only on `r`) or 1 to 60"),
            "{e}"
        );
        let e = parse("[tui]\nrefresh = 2\n").unwrap_err();
        assert!(e.to_string().contains("refresh"), "{e}");
    }

    #[test]
    fn parses_every_field_and_expands_tilde() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let text = r#"
code_roots = ["~/work", "/opt/src"]
destructive = true
keep = ["postgres"]

[daemon]
sample_interval_secs = 1
"#;
        let c = Config::parse(text, &p, Path::new("c.toml")).unwrap();
        assert_eq!(
            c.code_roots,
            vec![tmp.path().join("work"), PathBuf::from("/opt/src")]
        );
        assert!(c.destructive);
        assert_eq!(c.keep, vec!["postgres"]);
        assert_eq!(c.daemon.sample_interval_secs, 1);
    }

    #[test]
    fn destructive_false_names_its_line_and_grace_takes_0_to_a_week() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let parse = |text: &str| Config::parse(text, &p, Path::new("c.toml"));
        let c = parse("# off for now\nkeep = []\n  destructive   =  false # agents\n").unwrap();
        assert!(!c.destructive);
        assert_eq!(c.destructive_line, Some(3));
        assert_eq!(parse("destructive = true\n").unwrap().destructive_line, None);
        assert_eq!(
            parse("leftover_grace_secs = 0\n").unwrap().leftover_grace,
            std::time::Duration::ZERO
        );
        let e = parse("\nleftover_grace_secs = 604801\n").unwrap_err();
        assert_eq!(e.line, Some(2), "{e}");
        assert_eq!(
            parse("leftover_grace_secs = 60\n").unwrap().fingerprint,
            parse("").unwrap().fingerprint,
            "the daemon does not judge leftovers"
        );
    }

    #[test]
    fn fingerprint_follows_the_settings_not_the_text() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let print = |text: &str| {
            Config::parse(text, &p, Path::new("c.toml"))
                .unwrap()
                .fingerprint
        };
        let base = print("keep = [\"a\"]\n[daemon]\nsample_interval_secs = 5\n");
        let reworded = "# note\nkeep = [ \"a\" ]   # kept\n\n[daemon]\n  sample_interval_secs=5\n";
        assert_eq!(base, print(reworded));
        assert_ne!(
            base,
            print("keep = [\"a\"]\n[daemon]\nsample_interval_secs = 6\n")
        );
        assert_ne!(
            base,
            print("keep = [\"b\"]\n[daemon]\nsample_interval_secs = 5\n")
        );
        assert_eq!(
            print(""),
            Config::load(&p).unwrap().0.fingerprint,
            "no file, empty file"
        );
    }

    #[test]
    fn errors_name_the_line() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let cases = [
            ("keep = []\ndestructive = \"yes\"\n", 2, "boolean"),
            ("\n\ncode_roots = [\n", 3, ""),
            ("destructive = false\nkepp = []\n", 2, "kepp"),
            ("[daemon]\nsample_interval_secs = 0\n", 2, "at least 1"),
        ];
        for (text, line, needle) in cases {
            let e = Config::parse(text, &p, Path::new("/x/config.toml")).unwrap_err();
            assert_eq!(e.line, Some(line), "{text:?}: {e}");
            assert!(e.to_string().contains(needle), "{text:?}: {e}");
            assert!(
                e.to_string()
                    .starts_with(&format!("/x/config.toml line {line}: "))
            );
        }
    }
}
