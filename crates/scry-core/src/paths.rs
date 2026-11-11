//! Where Scry keeps its config and state. `SCRY_HOME` moves both under one directory, so
//! tests and manual runs never touch the real ones.

use std::fs::{self, DirBuilder, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const HOME_ENV: &str = "SCRY_HOME";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// `config.toml`. Read only; Scry never writes it.
    pub config: PathBuf,
    /// Holds `scry.db`, `spool/`, `daemon.lock`, `daemon.log` and `daemon.log.1`. Mode 0700.
    pub state: PathBuf,
    /// The user's home, for `~` in config values and the LaunchAgents directory.
    pub user_home: PathBuf,
    /// Set when `SCRY_HOME` is in effect. The LaunchAgent carries it to the daemon.
    pub scry_home: Option<PathBuf>,
}

impl Paths {
    pub fn from_env() -> Result<Paths> {
        let Some(user_home) = std::env::home_dir().filter(|h| !h.as_os_str().is_empty()) else {
            bail!("cannot tell the home directory");
        };
        let scry_home = match std::env::var_os(HOME_ENV) {
            Some(h) if !h.is_empty() => Some(std::path::absolute(PathBuf::from(h))?),
            _ => None,
        };
        Ok(Paths::new(user_home, scry_home))
    }

    pub fn new(user_home: PathBuf, scry_home: Option<PathBuf>) -> Paths {
        let (config, state) = match &scry_home {
            Some(h) => (h.join("config.toml"), h.join("state")),
            None => (
                user_home.join(".config/scry/config.toml"),
                user_home.join("Library/Application Support/Scry"),
            ),
        };
        Paths {
            config,
            state,
            user_home,
            scry_home,
        }
    }

    pub fn db(&self) -> PathBuf {
        self.state.join("scry.db")
    }

    pub fn spool(&self) -> PathBuf {
        self.state.join("spool")
    }

    pub fn lock(&self) -> PathBuf {
        self.state.join("daemon.lock")
    }

    pub fn log(&self) -> PathBuf {
        self.state.join("daemon.log")
    }

    /// Creates the state and spool directories as 0700, tightening them if they exist.
    pub fn ensure_state(&self) -> Result<()> {
        for dir in [self.state.clone(), self.spool()] {
            private_dir(&dir)?;
        }
        Ok(())
    }

    /// `~/x` becomes `<home>/x`. Other paths pass through.
    pub fn expand(&self, p: &Path) -> PathBuf {
        match p.strip_prefix("~") {
            Ok(rest) => self.user_home.join(rest),
            Err(_) => p.to_path_buf(),
        }
    }

    /// The reverse of `expand`, for display.
    pub fn tilde(&self, p: &Path) -> String {
        match p.strip_prefix(&self.user_home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        }
    }
}

pub fn private_dir(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting mode 0700 on {}", dir.display()))
}

/// 64-bit FNV-1a. Unlike `DefaultHasher` it gives the same value in every build, so two
/// scry binaries agree on a config's fingerprint and on the label derived from `SCRY_HOME`.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Opens `path` for reading and writing, creating it as 0600 and tightening it if it exists.
pub fn private_file(path: &Path) -> Result<fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting mode 0600 on {}", path.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scry_home_moves_config_and_state_together() {
        let p = Paths::new("/Users/dev".into(), Some("/tmp/x".into()));
        assert_eq!(p.config, Path::new("/tmp/x/config.toml"));
        assert_eq!(p.db(), Path::new("/tmp/x/state/scry.db"));

        let p = Paths::new("/Users/dev".into(), None);
        assert_eq!(p.config, Path::new("/Users/dev/.config/scry/config.toml"));
        assert_eq!(
            p.lock(),
            Path::new("/Users/dev/Library/Application Support/Scry/daemon.lock")
        );
    }

    #[test]
    fn state_is_private() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new("/nonexistent".into(), Some(tmp.path().into()));
        fs::create_dir_all(&p.state).unwrap();
        fs::set_permissions(&p.state, fs::Permissions::from_mode(0o755)).unwrap();
        p.ensure_state().unwrap();
        let mode = |q: &Path| fs::metadata(q).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p.state), 0o700);
        assert_eq!(mode(&p.spool()), 0o700);
        private_file(&p.lock()).unwrap();
        assert_eq!(mode(&p.lock()), 0o600);
    }

    #[test]
    fn tilde_round_trips() {
        let p = Paths::new("/Users/ann".into(), None);
        assert_eq!(p.expand(Path::new("~/src")), Path::new("/Users/ann/src"));
        assert_eq!(p.tilde(Path::new("/Users/ann/src")), "~/src");
        assert_eq!(p.tilde(Path::new("/Users/anna/src")), "/Users/anna/src");
    }

    #[test]
    fn fnv1a_matches_the_published_vectors() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
