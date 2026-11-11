//! The user LaunchAgent that keeps `scry daemon run` alive. Install and uninstall observe
//! the plist and launchd, plan the steps that reach the target, and run only those, so
//! running either twice converges.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::{Paths, fnv1a};

/// The label of the daemon that uses the default config and state.
pub const LABEL: &str = "com.morozred.scry.daemon";

/// Whatever the umask. A plist that others can write lets them choose what launchd runs as
/// this user, and some macOS releases refuse to load one.
const PLIST_MODE: u32 = 0o644;

/// `LABEL`, or under `SCRY_HOME` the label plus a hash of that directory, so an install from
/// a scratch `SCRY_HOME` never replaces or removes the real agent.
pub fn label(paths: &Paths) -> String {
    match &paths.scry_home {
        Some(home) => format!("{LABEL}.{:08x}", fnv1a(home.as_os_str().as_bytes()) >> 32),
        None => LABEL.to_string(),
    }
}

/// What launchd says about the job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Launchd {
    NotLoaded,
    /// Loaded. `pid` is `None` between a crash and the restart, or after a clean exit.
    Loaded {
        pid: Option<i32>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlistFile {
    pub text: String,
    pub mode: u32,
}

/// The plist on disk plus launchd's view: everything install and uninstall decide from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed {
    pub plist: Option<PlistFile>,
    /// `launchctl disable`, or the switch in Login Items, which makes `bootstrap` fail.
    pub disabled: bool,
    pub launchd: Launchd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// `launchctl enable`: lift a disable so the job can load.
    Enable,
    /// `launchctl bootout`: stop the job and unload it.
    Bootout,
    WritePlist,
    /// Set the plist's mode to `PLIST_MODE`.
    FixPlistMode,
    RemovePlist,
    /// `launchctl bootstrap`: load the plist, which starts the job (`RunAtLoad`).
    Bootstrap,
    /// `launchctl kickstart`: start a loaded job that is not running.
    Kickstart,
    /// `launchctl kickstart -k`: replace a running job whose binary or config changed.
    Restart,
}

/// Steps from `obs` to "the desired plist is on disk as 0644, enabled, loaded and running".
/// `restart` says the running job started with another binary or config.
pub fn plan_install(obs: &Observed, desired: &str, restart: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    if obs.disabled {
        steps.push(Step::Enable);
    }
    let current = obs.plist.as_ref().filter(|p| p.text == desired);
    if current.is_some_and(|p| p.mode != PLIST_MODE) {
        steps.push(Step::FixPlistMode);
    }
    steps.extend(match (obs.launchd, current.is_some()) {
        (Launchd::NotLoaded, false) => vec![Step::WritePlist, Step::Bootstrap],
        (Launchd::NotLoaded, true) => vec![Step::Bootstrap],
        (Launchd::Loaded { .. }, false) => vec![Step::Bootout, Step::WritePlist, Step::Bootstrap],
        (Launchd::Loaded { pid: None }, true) => vec![Step::Kickstart],
        (Launchd::Loaded { pid: Some(_) }, true) if restart => vec![Step::Restart],
        (Launchd::Loaded { pid: Some(_) }, true) => vec![],
    });
    steps
}

/// Steps from `obs` to "nothing loaded, no plist".
pub fn plan_uninstall(obs: &Observed) -> Vec<Step> {
    let mut steps = Vec::new();
    if obs.launchd != Launchd::NotLoaded {
        steps.push(Step::Bootout);
    }
    if obs.plist.is_some() {
        steps.push(Step::RemovePlist);
    }
    steps
}

/// The binary a LaunchAgent should run: the canonical path of `exe`, or the stable
/// `<prefix>/bin/<name>` link when `exe` lives in a Homebrew Cellar that an upgrade replaces.
pub fn stable_program(exe: &Path) -> Result<PathBuf> {
    let real = fs::canonicalize(exe).with_context(|| format!("resolving {}", exe.display()))?;
    if let (Some(prefix), Some(name)) = (cellar_prefix(&real), real.file_name()) {
        let link = prefix.join("bin").join(name);
        if fs::canonicalize(&link).is_ok_and(|t| t == real) {
            return Ok(link);
        }
    }
    Ok(real)
}

/// `<prefix>` of `<prefix>/Cellar/<formula>/<version>/bin/<name>`.
fn cellar_prefix(p: &Path) -> Option<PathBuf> {
    let parts: Vec<Component> = p.components().collect();
    let i = parts.iter().rposition(|c| c.as_os_str() == "Cellar")?;
    (parts.len() == i + 5 && parts[i + 3].as_os_str() == "bin").then(|| parts[..i].iter().collect())
}

pub struct Spec {
    pub label: String,
    pub program: PathBuf,
    pub log: PathBuf,
    /// Passed through so a daemon installed under `SCRY_HOME` uses the same state.
    pub scry_home: Option<PathBuf>,
}

impl Spec {
    /// The arguments the plist gives launchd, program first.
    pub fn args(program: &Path) -> Vec<String> {
        vec![
            program.to_string_lossy().into_owned(),
            "daemon".into(),
            "run".into(),
        ]
    }

    pub fn env(scry_home: Option<&Path>) -> BTreeMap<String, String> {
        scry_home
            .map(|h| ("SCRY_HOME".to_string(), h.to_string_lossy().into_owned()))
            .into_iter()
            .collect()
    }
}

pub fn render_plist(spec: &Spec) -> String {
    let x = |s: &str| xml_escape(s);
    let args: String = Spec::args(&spec.program)
        .iter()
        .map(|a| format!("\t\t<string>{}</string>\n", x(a)))
        .collect();
    let env = Spec::env(spec.scry_home.as_deref());
    let env = if env.is_empty() {
        String::new()
    } else {
        let pairs: String = env
            .iter()
            .map(|(k, v)| format!("\t\t<key>{}</key>\n\t\t<string>{}</string>\n", x(k), x(v)))
            .collect();
        format!("\t<key>EnvironmentVariables</key>\n\t<dict>\n{pairs}\t</dict>\n")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
{args}	</array>
{env}	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ProcessType</key>
	<string>Background</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        label = x(&spec.label),
        log = x(&spec.log.to_string_lossy()),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The parts of an installed plist that `doctor` checks, read through `plutil` so a plist
/// edited by hand or saved as binary still parses.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlistContents {
    #[serde(rename(deserialize = "Label"))]
    pub label: Option<String>,
    #[serde(rename(deserialize = "ProgramArguments"), default)]
    pub args: Vec<String>,
    #[serde(rename(deserialize = "EnvironmentVariables"), default)]
    pub env: BTreeMap<String, String>,
}

/// Reads `pid` from `launchctl print` output. Only the job's own lines, indented by one
/// tab, count; nested blocks list other pids.
fn parse_print_pid(out: &str) -> Option<i32> {
    out.lines()
        .find_map(|l| l.strip_prefix("\tpid = "))
        .and_then(|v| v.trim().parse().ok())
}

/// Reads `"<label>" => disabled` from `launchctl print-disabled`. Older releases print
/// `true` for a disabled service.
fn parse_disabled(out: &str, label: &str) -> bool {
    let key = format!("\"{label}\" =>");
    out.lines()
        .find_map(|l| l.trim_start().strip_prefix(key.as_str()))
        .is_some_and(|v| matches!(v.trim(), "disabled" | "true"))
}

pub struct LaunchAgent {
    pub label: String,
    pub plist: PathBuf,
    domain: String,
}

impl LaunchAgent {
    pub fn new(paths: &Paths) -> LaunchAgent {
        let label = label(paths);
        LaunchAgent {
            plist: paths
                .user_home
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist")),
            label,
            domain: format!("gui/{}", unsafe { libc::getuid() }),
        }
    }

    /// `gui/<uid>/<label>`.
    fn target(&self) -> String {
        format!("{}/{}", self.domain, self.label)
    }

    pub fn observe(&self) -> Result<Observed> {
        Ok(Observed {
            plist: self.plist_file()?,
            disabled: self.disabled()?,
            launchd: self.launchd()?,
        })
    }

    pub fn plist_file(&self) -> Result<Option<PlistFile>> {
        let mut f = match File::open(&self.plist) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.plist.display())),
        };
        let mut text = String::new();
        f.read_to_string(&mut text)
            .with_context(|| format!("reading {}", self.plist.display()))?;
        let mode = f.metadata()?.permissions().mode() & 0o7777;
        Ok(Some(PlistFile { text, mode }))
    }

    pub fn launchd(&self) -> Result<Launchd> {
        let out = Command::new("launchctl")
            .args(["print", &self.target()])
            .output()
            .context("running launchctl print")?;
        // 113: launchd has no such service.
        if out.status.code() == Some(113) {
            return Ok(Launchd::NotLoaded);
        }
        if !out.status.success() {
            bail!(
                "launchctl print {} failed: {}",
                self.target(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(Launchd::Loaded {
            pid: parse_print_pid(&String::from_utf8_lossy(&out.stdout)),
        })
    }

    pub fn disabled(&self) -> Result<bool> {
        let out = Command::new("launchctl")
            .args(["print-disabled", &self.domain])
            .output()
            .context("running launchctl print-disabled")?;
        if !out.status.success() {
            bail!(
                "launchctl print-disabled {} failed: {}",
                self.domain,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(parse_disabled(
            &String::from_utf8_lossy(&out.stdout),
            &self.label,
        ))
    }

    /// The installed plist's label, arguments and environment, or `None` if it is missing or
    /// `plutil` cannot read it.
    pub fn read_plist(&self) -> Option<PlistContents> {
        let out = Command::new("plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(&self.plist)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| serde_json::from_slice(&out.stdout).ok())
            .flatten()
    }

    /// Unloads the job and removes the plist, and returns the steps that took.
    pub fn uninstall(&self) -> Result<Vec<Step>> {
        let steps = plan_uninstall(&self.observe()?);
        self.apply(&steps, "")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.launchd()? != Launchd::NotLoaded {
            if Instant::now() >= deadline {
                bail!("launchd still lists {} after bootout", self.label);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(steps)
    }

    pub fn apply(&self, steps: &[Step], desired: &str) -> Result<()> {
        for step in steps {
            match step {
                Step::Enable => self.launchctl(&["enable", &self.target()])?,
                Step::Bootout => self.launchctl(&["bootout", &self.target()])?,
                Step::WritePlist => self.write_plist(desired)?,
                Step::FixPlistMode => {
                    fs::set_permissions(&self.plist, Permissions::from_mode(PLIST_MODE))
                        .with_context(|| format!("setting mode on {}", self.plist.display()))?
                }
                Step::RemovePlist => fs::remove_file(&self.plist)
                    .with_context(|| format!("removing {}", self.plist.display()))?,
                Step::Bootstrap => self.bootstrap()?,
                Step::Kickstart => self.launchctl(&["kickstart", &self.target()])?,
                Step::Restart => self.launchctl(&["kickstart", "-k", &self.target()])?,
            }
        }
        Ok(())
    }

    fn write_plist(&self, text: &str) -> Result<()> {
        if let Some(dir) = self.plist.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o755)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(PLIST_MODE)
            .open(&self.plist)
            .with_context(|| format!("writing {}", self.plist.display()))?;
        f.write_all(text.as_bytes())?;
        // `mode` above only applies to a new file, and only after the umask.
        f.set_permissions(Permissions::from_mode(PLIST_MODE))?;
        Ok(())
    }

    fn bootstrap(&self) -> Result<()> {
        let plist = self.plist.to_string_lossy();
        // Right after a bootout launchd can still be tearing the old job down and answers
        // "5: Input/output error" for a moment.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.launchctl(&["bootstrap", &self.domain, &plist]) {
                Ok(()) => return Ok(()),
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200))
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn launchctl(&self, args: &[&str]) -> Result<()> {
        let out = Command::new("launchctl")
            .args(args)
            .output()
            .context("running launchctl")?;
        if !out.status.success() {
            bail!(
                "launchctl {} failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: Launchd = Launchd::Loaded { pid: Some(42) };
    const IDLE: Launchd = Launchd::Loaded { pid: None };

    fn obs(plist: Option<(&str, u32)>, launchd: Launchd) -> Observed {
        Observed {
            plist: plist.map(|(text, mode)| PlistFile {
                text: text.into(),
                mode,
            }),
            disabled: false,
            launchd,
        }
    }

    #[test]
    fn install_converges_from_every_state() {
        use Step::*;
        let want = Some(("new", 0o644));
        let cases = [
            (
                obs(None, Launchd::NotLoaded),
                false,
                vec![WritePlist, Bootstrap],
            ),
            (obs(want, Launchd::NotLoaded), false, vec![Bootstrap]),
            (obs(want, RUNNING), false, vec![]),
            (obs(want, RUNNING), true, vec![Restart]),
            (obs(want, IDLE), false, vec![Kickstart]),
            (obs(want, IDLE), true, vec![Kickstart]),
            (
                obs(Some(("old", 0o644)), RUNNING),
                true,
                vec![Bootout, WritePlist, Bootstrap],
            ),
            (
                obs(None, RUNNING),
                false,
                vec![Bootout, WritePlist, Bootstrap],
            ),
            (
                obs(Some(("new", 0o664)), Launchd::NotLoaded),
                false,
                vec![FixPlistMode, Bootstrap],
            ),
            (
                obs(Some(("new", 0o600)), RUNNING),
                false,
                vec![FixPlistMode],
            ),
            (
                Observed {
                    disabled: true,
                    ..obs(want, Launchd::NotLoaded)
                },
                false,
                vec![Enable, Bootstrap],
            ),
            (
                Observed {
                    disabled: true,
                    ..obs(want, RUNNING)
                },
                false,
                vec![Enable],
            ),
        ];
        for (o, restart, steps) in cases {
            assert_eq!(plan_install(&o, "new", restart), steps, "{o:?} {restart}");
        }
    }

    #[test]
    fn uninstall_converges_from_every_state() {
        use Step::*;
        let cases = [
            (obs(None, Launchd::NotLoaded), vec![]),
            (
                obs(Some(("x", 0o644)), Launchd::NotLoaded),
                vec![RemovePlist],
            ),
            (obs(Some(("x", 0o644)), RUNNING), vec![Bootout, RemovePlist]),
            (obs(None, IDLE), vec![Bootout]),
        ];
        for (o, steps) in cases {
            assert_eq!(plan_uninstall(&o), steps, "{o:?}");
        }
    }

    #[test]
    fn scry_home_gets_its_own_label_and_plist() {
        let real = Paths::new("/Users/dev".into(), None);
        let a = Paths::new("/Users/dev".into(), Some("/tmp/a".into()));
        let b = Paths::new("/Users/dev".into(), Some("/tmp/b".into()));
        assert_eq!(label(&real), "com.morozred.scry.daemon");
        assert!(label(&a).starts_with("com.morozred.scry.daemon."));
        assert_eq!(label(&a).len(), LABEL.len() + 9);
        assert_ne!(label(&a), label(&b));
        let again = Paths::new("/Users/ann".into(), Some("/tmp/a".into()));
        assert_eq!(label(&a), label(&again), "depends on SCRY_HOME only");
        let agent = LaunchAgent::new(&a);
        assert_eq!(
            agent.plist,
            Path::new("/Users/dev/Library/LaunchAgents").join(format!("{}.plist", label(&a)))
        );
        assert!(agent.target().ends_with(&format!("/{}", label(&a))));
    }

    fn agent_at(plist: PathBuf) -> LaunchAgent {
        LaunchAgent {
            label: LABEL.into(),
            plist,
            domain: "gui/0".into(),
        }
    }

    #[test]
    fn plist_is_valid_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = Spec {
            label: "com.morozred.scry.daemon.0000abcd".into(),
            program: "/opt/homebrew/bin/scry".into(),
            log: "/x/a&b/daemon.log".into(),
            scry_home: Some("/x/a&b".into()),
        };
        let agent = agent_at(tmp.path().join("p.plist"));
        fs::write(&agent.plist, render_plist(&spec)).unwrap();
        let get = |key: &str| {
            let out = Command::new("plutil")
                .args(["-extract", key, "raw", "-o", "-"])
                .arg(&agent.plist)
                .output()
                .unwrap();
            assert!(out.status.success(), "{key}");
            String::from_utf8(out.stdout)
                .unwrap()
                .trim_end()
                .to_string()
        };
        assert_eq!(get("KeepAlive.SuccessfulExit"), "false");
        assert_eq!(get("RunAtLoad"), "true");
        assert_eq!(get("StandardErrorPath"), "/x/a&b/daemon.log");
        assert_eq!(
            agent.read_plist(),
            Some(PlistContents {
                label: Some(spec.label.clone()),
                args: Spec::args(&spec.program),
                env: Spec::env(Some(Path::new("/x/a&b"))),
            })
        );

        let plain = render_plist(&Spec {
            scry_home: None,
            ..spec
        });
        assert!(!plain.contains("EnvironmentVariables"));
        fs::write(&agent.plist, "not a plist").unwrap();
        assert_eq!(agent.read_plist(), None);
    }

    #[test]
    fn plist_is_written_and_repaired_as_0644() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_at(tmp.path().join("LaunchAgents/p.plist"));
        let mode = || fs::metadata(&agent.plist).unwrap().permissions().mode() & 0o7777;

        agent.apply(&[Step::WritePlist], "one").unwrap();
        assert_eq!(mode(), 0o644);
        fs::set_permissions(&agent.plist, Permissions::from_mode(0o666)).unwrap();
        agent.apply(&[Step::WritePlist], "two").unwrap();
        assert_eq!(mode(), 0o644, "rewriting repairs the mode");
        assert_eq!(fs::read_to_string(&agent.plist).unwrap(), "two");

        fs::set_permissions(&agent.plist, Permissions::from_mode(0o664)).unwrap();
        assert_eq!(agent.plist_file().unwrap().unwrap().mode, 0o664);
        agent.apply(&[Step::FixPlistMode], "").unwrap();
        assert_eq!(mode(), 0o644);
    }

    #[test]
    fn stable_program_prefers_the_homebrew_link() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let cellar_bin = root.join("Cellar/scry/0.1.0/bin");
        fs::create_dir_all(&cellar_bin).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        let real = cellar_bin.join("scry");
        fs::write(&real, "").unwrap();
        std::os::unix::fs::symlink("../Cellar/scry/0.1.0/bin/scry", root.join("bin/scry")).unwrap();

        assert_eq!(stable_program(&real).unwrap(), root.join("bin/scry"));
        assert_eq!(
            stable_program(&root.join("bin/scry")).unwrap(),
            root.join("bin/scry")
        );

        fs::remove_file(root.join("bin/scry")).unwrap();
        assert_eq!(stable_program(&real).unwrap(), real, "no link, no guess");

        let plain = root.join("target/debug/scry");
        fs::create_dir_all(plain.parent().unwrap()).unwrap();
        fs::write(&plain, "").unwrap();
        assert_eq!(stable_program(&plain).unwrap(), plain);
    }

    #[test]
    fn reads_the_job_pid_not_a_nested_one() {
        let out = "gui/501/x = {\n\tstate = running\n\tenvironment = {\n\t\tpid = 7\n\t}\n\tpid = 976\n}\n";
        assert_eq!(parse_print_pid(out), Some(976));
        assert_eq!(
            parse_print_pid("gui/501/x = {\n\tstate = waiting\n}\n"),
            None
        );
    }

    #[test]
    fn reads_the_disabled_flag_for_this_label_only() {
        let out = "disabled services = {\n\t\"com.morozred.scry.daemon.1\" => disabled\n\t\"com.morozred.scry.daemon\" => enabled\n}\n";
        assert!(!parse_disabled(out, "com.morozred.scry.daemon"));
        assert!(parse_disabled(out, "com.morozred.scry.daemon.1"));
        assert!(!parse_disabled(out, "com.other"));
        assert!(parse_disabled("\t\"x\" => true\n", "x"), "older format");
        assert!(!parse_disabled("\t\"x\" => false\n", "x"));
    }
}
