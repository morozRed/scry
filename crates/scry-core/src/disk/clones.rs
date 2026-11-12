//! Git clones a tool keeps inside its own cache and makes again when they are gone:
//! SwiftPM's package checkouts and mirrors in Xcode's DerivedData, and Yarn's git
//! dependencies. A repository inside a cache blocks its cleanup, except one at a place in
//! [`CLONES`] that git, read right before the cleanup, shows holds nothing of yours.
//!
//! A checkout counts when its `.git` is a folder of its own, its config names no include,
//! filter or other work tree, `git status` shows nothing changed or untracked, no index
//! entry hides a change from it, there is no stash, and every commit on HEAD and on every
//! local branch is on a remote-tracking branch. A mirror counts when it is a bare repository
//! whose config marks a remote as its mirror, so every ref in it is that remote's. Anything
//! else still blocks.

use std::path::{Component, Path};
use std::time::Duration;

use crate::git;

/// How a tool keeps a clone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// A working copy with a `.git` folder.
    Checkout,
    /// A bare `git clone --mirror`.
    Mirror,
}

/// A place a tool keeps clones: the catalog item it sits in, the path below that item's
/// folder, one component per `/` (`*` is any name, `@*` any name starting with `@`), and
/// what is kept there.
pub struct Clone {
    pub item: &'static str,
    pub at: &'static str,
    pub shape: Shape,
}

/// Reviewed by hand against what each tool writes.
pub static CLONES: &[Clone] = &[
    // SwiftPM in Xcode: `<project>-<hash>/SourcePackages/checkouts/<package>`, cloned from
    // the mirror next to it and checked out at the resolved version.
    Clone {
        item: "xcode-derived-data",
        at: "*/SourcePackages/checkouts/*",
        shape: Shape::Checkout,
    },
    // SwiftPM in Xcode: `<project>-<hash>/SourcePackages/repositories/<package>-<hash>`.
    Clone {
        item: "xcode-derived-data",
        at: "*/SourcePackages/repositories/*",
        shape: Shape::Mirror,
    },
    // Yarn 1: a git dependency's package in `v6/npm-<name>-<version>-<hash>/node_modules`.
    Clone {
        item: "yarn-cache",
        at: "*/*/node_modules/*",
        shape: Shape::Checkout,
    },
    Clone {
        item: "yarn-cache",
        at: "*/*/node_modules/@*/*",
        shape: Shape::Checkout,
    },
];

/// The shape of the clone a tool keeps at `rel` below the folder of `item`, if the table
/// names that place.
pub fn managed(item: &str, rel: &Path) -> Option<Shape> {
    let parts: Option<Vec<&str>> = rel
        .components()
        .map(|c| match c {
            Component::Normal(n) => n.to_str(),
            _ => None,
        })
        .collect();
    let parts = parts?;
    CLONES
        .iter()
        .filter(|c| c.item == item)
        .find(|c| {
            let pattern: Vec<&str> = c.at.split('/').collect();
            pattern.len() == parts.len()
                && pattern
                    .iter()
                    .zip(&parts)
                    .all(|(p, n)| match p.strip_suffix('*') {
                        Some(prefix) => n.starts_with(prefix) && n.len() > prefix.len(),
                        None => p == n,
                    })
        })
        .map(|c| c.shape)
}

/// Whether the clone at `path` holds nothing that is not on a remote, read with git now.
/// `Err` says what it holds, or why that could not be told.
pub fn regenerable(path: &Path, shape: Shape, timeout: Duration) -> Result<(), String> {
    let gitdir = match shape {
        Shape::Checkout => path.join(".git"),
        Shape::Mirror => path.to_path_buf(),
    };
    let real_dir = std::fs::symlink_metadata(&gitdir).is_ok_and(|m| m.is_dir());
    if !real_dir || gitdir.join("commondir").exists() {
        return Err("is not a clone of its own".into());
    }
    let read = |args: &[&std::ffi::OsStr]| {
        git::stdout(path, args, timeout).map_err(|e| format!("could not be read: {e:#}"))
    };
    let config = git::parse_config(&read(&git::config_list_args(&gitdir.join("config")))?);
    let value = |key: &str| {
        config
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.as_deref())
    };
    let truthy = |v: Option<&str>| matches!(v, Some("true" | "yes" | "on" | "1"));
    let steered = config.iter().any(|(k, _)| {
        k.starts_with("include.")
            || k.starts_with("includeif.")
            || k.starts_with("filter.")
            || k == "core.worktree"
    });
    if steered {
        return Err(
            "has a config that includes files, sets filters or names another work tree".into(),
        );
    }
    let git_dir = format!("--git-dir={}", gitdir.display());
    match shape {
        Shape::Mirror => {
            let mirror = config.iter().any(|(k, v)| {
                k.starts_with("remote.") && k.ends_with(".mirror") && truthy(v.as_deref())
            });
            if truthy(value("core.bare")) && mirror {
                Ok(())
            } else {
                Err("is not a mirror of a remote".into())
            }
        }
        Shape::Checkout => {
            let work_tree = format!("--work-tree={}", path.display());
            let with = |rest: &[&str]| {
                let mut args: Vec<&std::ffi::OsStr> = vec![git_dir.as_ref(), work_tree.as_ref()];
                args.extend(rest.iter().map(std::ffi::OsStr::new));
                read(&args)
            };
            let status = git::parse_status(&with(&git::STATUS)?).map_err(|e| e.to_string())?;
            let index = git::parse_index(&with(&git::LS_FILES)?).map_err(|e| e.to_string())?;
            if status.changed + status.untracked > 0 || index.hidden > 0 {
                return Err("has local changes".into());
            }
            let stash = git::run(
                path,
                &[
                    git_dir.as_str(),
                    "rev-parse",
                    "-q",
                    "--verify",
                    "refs/stash",
                ],
                timeout,
            )
            .map_err(|e| format!("could not be read: {e:#}"))?;
            if stash.code == Some(0) {
                return Err("has a stash".into());
            }
            let unpushed = with(&[
                "rev-list",
                "--count",
                "HEAD",
                "--branches",
                "--not",
                "--remotes",
            ])?;
            match unpushed.trim() {
                "0" => Ok(()),
                n => Err(format!(
                    "has {n} commits that no remote-tracking branch holds"
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_names_only_the_places_tools_clone_into() {
        let at = |item, rel: &str| managed(item, Path::new(rel));
        let d = "xcode-derived-data";
        assert_eq!(
            at(d, "App-abc/SourcePackages/checkouts/swift-log"),
            Some(Shape::Checkout)
        );
        assert_eq!(
            at(d, "App-abc/SourcePackages/repositories/swift-log-1a2b"),
            Some(Shape::Mirror)
        );
        assert_eq!(at(d, "App-abc/SourcePackages/checkouts"), None);
        assert_eq!(at(d, "App-abc/SourcePackages/checkouts/a/b"), None);
        assert_eq!(at(d, "App-abc/Build/checkouts/swift-log"), None);
        assert_eq!(at("npm-cache", "App-abc/SourcePackages/checkouts/x"), None);
        let y = "yarn-cache";
        assert_eq!(
            at(y, "v6/npm-pkg-1.0.0-abc/node_modules/pkg"),
            Some(Shape::Checkout)
        );
        assert_eq!(
            at(y, "v6/npm-@s-pkg-1.0.0-abc/node_modules/@s/pkg"),
            Some(Shape::Checkout)
        );
        assert_eq!(at(y, "v6/npm-pkg-1.0.0-abc/node_modules/s/pkg"), None);
        assert_eq!(at(y, "v6/npm-pkg-1.0.0-abc/node_modules/@/pkg"), None);
        assert_eq!(at(y, "v6/pkg"), None);
        assert_eq!(at(y, "v6/../x/node_modules/pkg"), None);
    }
}
