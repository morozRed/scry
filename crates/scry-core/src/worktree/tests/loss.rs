//! States built to reach "can go" while holding work, from two outside reviews, and the
//! neighbouring states that were right before. Each test builds one home, scans it, and lists
//! every row whose label or reason is not the one expected.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use super::super::{ScanOptions, measure, scan};
use super::{add, commit, git, new_repo, now, row, s, says, write};
use crate::view::worktrees::WorktreesView;

/// Scans `home/code` and sizes it, as `scry worktrees` does.
fn view(home: &Path) -> WorktreesView {
    let mut scan = scan(&ScanOptions::new(home, vec![home.join("code")]));
    measure(&mut scan, home);
    WorktreesView::new(&scan, now(), home)
}

/// Each row's label, and a phrase its sentence or evidence must hold. Lists every miss.
fn expect(v: &WorktreesView, cases: &[(&str, &str, &str)]) {
    let wrong: Vec<String> = cases
        .iter()
        .filter_map(|&(name, word, phrase)| {
            let r = row(v, name);
            (r.status.word() != word || !says(r, phrase)).then(|| {
                format!(
                    "{name}: want {word:?} saying {phrase:?}, got {:?}: {}",
                    r.status.word(),
                    r.sentence
                )
            })
        })
        .collect();
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

fn detach(repo: &Path, name: &str, at: &str) -> std::path::PathBuf {
    let path = repo.parent().unwrap().join(name);
    git(repo, &["worktree", "add", "-q", "--detach", s(&path), at]);
    path
}

fn head(dir: &Path) -> String {
    git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

/// A repo with a tracked `local.conf` and `src/file.txt`, and the worktree config extension
/// on, so a worktree can carry its own config.
fn app(base: &Path, dir: &Path) {
    new_repo(base, dir);
    write(&dir.join("local.conf"), "conf=1\n");
    write(&dir.join("src/file.txt"), "file\n");
    write(&dir.join("docs/guide.txt"), "guide\n");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "files"]);
    git(dir, &["push", "-q", "origin", "main"]);
    git(dir, &["config", "core.repositoryformatversion", "1"]);
    git(dir, &["config", "extensions.worktreeConfig", "true"]);
    let exclude = dir.join(".git/info/exclude");
    let mut text = fs::read_to_string(&exclude).unwrap_or_default();
    text.push_str(".claude/\nscratch/\n");
    fs::write(exclude, text).unwrap();
}

#[test]
fn what_removing_a_folder_would_destroy_keeps_it_from_can_go() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    let repo = code.join("app");
    app(&base, &repo);

    add(&repo, "merged-clean", "merged-clean");
    let untracked = add(&repo, "untracked", "untracked");
    write(&untracked.join("new.txt"), "new\n");

    // Edits git status does not compare, because the index says not to look.
    let au = add(&repo, "assume-unchanged", "assume-unchanged");
    git(&au, &["update-index", "--assume-unchanged", "local.conf"]);
    write(&au.join("local.conf"), "conf=only here\n");
    let sw = add(&repo, "skip-worktree", "skip-worktree");
    git(&sw, &["update-index", "--skip-worktree", "local.conf"]);
    write(&sw.join("local.conf"), "conf=only here too\n");
    // Sparse: a file written outside the cone at a tracked path.
    let sparse = add(&repo, "sparse", "sparse");
    git(&sparse, &["sparse-checkout", "set", "src"]);
    write(&sparse.join("docs/guide.txt"), "edited outside the cone\n");

    // `core.worktree` aims git at a clean copy elsewhere; the draft is in the real folder.
    let redirect = add(&repo, "core-worktree", "core-worktree");
    let copy = base.join("clean-copy");
    fs::create_dir_all(&copy).unwrap();
    let tar = base.join("main.tar");
    git(&repo, &["archive", "-o", s(&tar), "main"]);
    let untar = Command::new("tar")
        .args(["-xf", s(&tar), "-C", s(&copy)])
        .status()
        .unwrap();
    assert!(untar.success());
    git(
        &redirect,
        &["config", "--worktree", "core.worktree", s(&copy)],
    );
    write(&redirect.join("draft.txt"), "unique work\n");

    // A repo inside a tracked folder, whose only copy of a draft is its own stash.
    let nested = add(&repo, "nested-tracked", "nested-tracked");
    git(&nested, &["init", "-q", "-b", "nested", "src"]);
    git(&nested.join("src"), &["add", "file.txt"]);
    git(&nested.join("src"), &["commit", "-qm", "nested"]);
    write(&nested.join("src/file.txt"), "draft\n");
    git(&nested.join("src"), &["stash", "push", "-q", "-m", "draft"]);
    // A repo inside an ignored folder.
    let ignored = add(&repo, "nested-ignored", "nested-ignored");
    git(&ignored, &["init", "-q", "scratch/inner"]);
    commit(&ignored.join("scratch/inner"), "inner.txt");
    // A repo inside an untracked folder, which git status lists.
    let untracked_repo = add(&repo, "nested-untracked", "nested-untracked");
    git(&untracked_repo, &["init", "-q", "inner"]);
    commit(&untracked_repo.join("inner"), "inner.txt");

    // A dirty worktree inside a clean, merged one, under an ignored `.claude/`.
    let outer = add(&repo, "outer", "outer");
    let inner = outer.join(".claude/worktrees/inner-dirty");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "inner-dirty", s(&inner)],
    );
    write(&inner.join("work.txt"), "uncommitted\n");

    // Worktree-private state in the record: config, a hook, a private ref, operations.
    let admin = |name: &str| repo.join(".git/worktrees").join(name);
    let own = add(&repo, "record-config", "record-config");
    git(
        &own,
        &["config", "--worktree", "scry.onlyCopy", "unique-value"],
    );
    write(
        &admin("record-config").join("hooks/pre-commit"),
        "#!/bin/sh\n",
    );
    let wref = add(&repo, "worktree-ref", "worktree-ref");
    commit(&wref, "kept-by-a-private-ref.txt");
    git(&wref, &["update-ref", "refs/worktree/keep", "HEAD"]);
    git(&wref, &["reset", "-q", "--hard", "main"]);
    let rebasing = add(&repo, "rebasing", "rebasing");
    commit(&rebasing, "r.txt");
    let editor = base.join("break-first.sh");
    write(
        &editor,
        "{ echo break; cat \"$1\"; } > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"\n",
    );
    let seq = format!("sequence.editor=sh {}", s(&editor));
    git(&rebasing, &["-c", &seq, "rebase", "-q", "-i", "main"]);
    let bisecting = add(&repo, "bisecting", "bisecting");
    git(&bisecting, &["bisect", "start"]);
    git(&bisecting, &["bisect", "bad"]);
    // A file some other tool keeps in the record. Unknown, so it needs a look once the work
    // is done; while it is in progress the worktree stays active, and the file is evidence.
    add(&repo, "tool-file", "tool-file");
    write(&admin("tool-file").join("AGENT_BASE"), "0123abcd\n");
    let busy = add(&repo, "tool-file-active", "tool-file-active");
    commit(&busy, "busy.txt");
    write(&admin("tool-file-active").join("AGENT_BASE"), "0123abcd\n");
    // What git's last merge left behind is git's own.
    add(&repo, "auto-merge-left", "auto-merge-left");
    write(&admin("auto-merge-left").join("AUTO_MERGE"), "0123abcd\n");

    // A detached HEAD with a commit no branch or tag holds.
    let orphan = detach(&repo, "detached-orphan", "main");
    commit(&orphan, "orphan.txt");

    // A lock git keeps, though its file cannot be read.
    let unreadable = add(&repo, "lock-unreadable", "lock-unreadable");
    git(&repo, &["worktree", "lock", s(&unreadable)]);
    let lock = admin("lock-unreadable").join("locked");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o000)).unwrap();

    // Moved by hand with a staged file, so the index in its record is the only copy.
    let moved = add(&repo, "moved", "moved");
    write(&moved.join("staged.txt"), "staged\n");
    git(&moved, &["add", "staged.txt"]);
    fs::rename(&moved, code.join("moved-elsewhere")).unwrap();

    // Submodules: added to main, then checked out in worktrees.
    let lib = base.join("lib");
    write(&lib.join("lib.txt"), "lib\n");
    git(&base, &["init", "-q", s(&lib)]);
    git(&lib, &["add", "."]);
    git(&lib, &["commit", "-qm", "lib"]);
    git(&repo, &["submodule", "add", "-q", s(&lib), "vendor/lib"]);
    git(&repo, &["commit", "-qm", "add lib"]);
    git(&repo, &["push", "-q", "origin", "main"]);
    // The submodule's own config hides its untracked draft from the outer status.
    let sub_hidden = add(&repo, "sub-hidden", "sub-hidden");
    git(&sub_hidden, &["submodule", "update", "--init", "-q"]);
    git(
        &sub_hidden.join("vendor/lib"),
        &["config", "status.showUntrackedFiles", "no"],
    );
    write(&sub_hidden.join("vendor/lib/draft.txt"), "draft\n");
    // A submodule commit recorded and merged in the parent but pushed nowhere.
    let unpushed = add(&repo, "sub-unpushed", "sub-unpushed");
    git(&unpushed, &["submodule", "update", "--init", "-q"]);
    commit(&unpushed.join("vendor/lib"), "unpushed.txt");
    git(&unpushed, &["add", "vendor/lib"]);
    git(&unpushed, &["commit", "-qm", "bump lib"]);
    git(&repo, &["merge", "-q", "--ff-only", "sub-unpushed"]);
    git(&repo, &["push", "-q", "origin", "main"]);
    // An embedded submodule, its git folder inside the worktree, with an unpushed commit.
    let embedded = add(&repo, "sub-embedded", "sub-embedded");
    git(&base, &["clone", "-q", s(&lib), s(&embedded.join("sub2"))]);
    commit(&embedded.join("sub2"), "unique.txt");
    let gitmodules = |key: &str, value: &str| {
        git(&embedded, &["config", "--file", ".gitmodules", key, value]);
    };
    gitmodules("submodule.sub2.path", "sub2");
    gitmodules("submodule.sub2.url", s(&lib));
    git(&embedded, &["add", ".gitmodules", "sub2"]);
    git(&embedded, &["commit", "-qm", "embed sub2"]);
    git(&repo, &["merge", "-q", "--ff-only", "sub-embedded"]);
    git(&repo, &["push", "-q", "origin", "main"]);

    let v = view(&home);
    expect(
        &v,
        &[
            ("merged-clean", "can go", "as of the last fetch"),
            ("untracked", "dirty", "1 untracked entry"),
            (
                "assume-unchanged",
                "needs a look",
                "1 file is hidden from git status",
            ),
            (
                "skip-worktree",
                "needs a look",
                "1 file is hidden from git status",
            ),
            ("sparse", "dirty", "1 changed entry"),
            ("core-worktree", "dirty", "1 untracked entry"),
            (
                "nested-tracked",
                "needs a look",
                "1 repository sits inside it",
            ),
            (
                "nested-ignored",
                "needs a look",
                "1 repository sits inside it",
            ),
            ("nested-untracked", "dirty", "1 untracked entry"),
            ("outer", "needs a look", "1 other worktree sits inside it"),
            ("inner-dirty", "dirty", "1 untracked entry"),
            (
                "record-config",
                "needs a look",
                "also holds config.worktree (scry.onlycopy) and hooks",
            ),
            ("worktree-ref", "needs a look", "also holds refs"),
            ("rebasing", "needs a look", "rebase-merge"),
            ("bisecting", "needs a look", "BISECT_LOG"),
            ("tool-file", "needs a look", "also holds AGENT_BASE"),
            (
                "tool-file-active",
                "active",
                "its record also holds AGENT_BASE",
            ),
            ("auto-merge-left", "can go", "as of the last fetch"),
            (
                "detached-orphan",
                "active",
                "has 1 commit on its detached HEAD that no branch or tag holds",
            ),
            (
                "lock-unreadable",
                "locked",
                "its lock file could not be read",
            ),
            ("moved", "needs a look", "git worktree repair"),
            (
                "sub-hidden",
                "needs a look",
                "`git worktree remove` refuses to remove",
            ),
            (
                "sub-unpushed",
                "needs a look",
                "it has 1 submodule checked out",
            ),
            (
                "sub-embedded",
                "needs a look",
                "it has 1 submodule checked out",
            ),
        ],
    );
}

/// What Claude Code, the Claude desktop app and Codex write into the records of worktrees
/// they make, as found on a real machine.
const AGENT_FILES: [(&str, &str); 5] = [
    ("CLAUDE_BASE", "0123456789abcdef0123456789abcdef01234567\n"),
    ("claude-desktop-worktree", ""),
    (
        "codex-thread.json",
        "{\"version\":1,\"ownerThreadId\":\"00000000-0000-0000-0000-000000000000\"}",
    ),
    (
        "codex-synced-branch.json",
        "{\"branch\":\"refs/heads/task\",\"lastSyncedTreeRef\":\"0123abcd\"}\n",
    ),
    ("preview-slot", "1\n"),
];

#[test]
fn known_agent_files_in_a_record_keep_neither_can_go_nor_can_prune_away() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    let repo = code.join("app");
    app(&base, &repo);
    let admin = |name: &str| repo.join(".git/worktrees").join(name);

    // Each file alone, in a merged, clean worktree outside any agent's path layout.
    let mut cases = Vec::new();
    for (i, (file, text)) in AGENT_FILES.iter().enumerate() {
        let name = format!("agent-file-{i}");
        add(&repo, &name, &name);
        write(&admin(&name).join(file), text);
        cases.push((name, *file));
    }
    // Registered, folder gone, with two of them beside git's own files.
    let gone = add(&repo, "agent-files-gone", "agent-files-gone");
    write(
        &admin("agent-files-gone").join("claude-desktop-worktree"),
        "",
    );
    write(&admin("agent-files-gone").join("preview-slot"), "2\n");
    fs::remove_dir_all(&gone).unwrap();
    // A file no reviewed agent writes still needs a look, beside a known one or alone, and so
    // does a folder that takes a known file's name.
    add(&repo, "unknown-beside", "unknown-beside");
    write(&admin("unknown-beside").join("CLAUDE_BASE"), "0123abcd\n");
    write(&admin("unknown-beside").join("tool-state.json"), "{}\n");
    let unknown_gone = add(&repo, "unknown-gone", "unknown-gone");
    write(&admin("unknown-gone").join("tool-state.json"), "{}\n");
    fs::remove_dir_all(&unknown_gone).unwrap();
    add(&repo, "folder-named-like-one", "folder-named-like-one");
    write(
        &admin("folder-named-like-one").join("CLAUDE_BASE/notes.txt"),
        "work\n",
    );

    let v = view(&home);
    let mut want: Vec<(&str, &str, &str)> = cases
        .iter()
        .map(|(name, _)| (name.as_str(), "can go", "as of the last fetch"))
        .collect();
    want.extend([
        ("agent-files-gone", "can prune", "the folder does not exist"),
        (
            "unknown-beside",
            "needs a look",
            "its record in the repo's git folder also holds tool-state.json.",
        ),
        ("unknown-gone", "needs a look", "also holds tool-state.json"),
        (
            "folder-named-like-one",
            "needs a look",
            "its record in the repo's git folder also holds CLAUDE_BASE.",
        ),
    ]);
    expect(&v, &want);

    // A file only one agent writes names it, though the path follows no agent's layout.
    for (name, file) in &cases {
        let r = row(&v, name);
        let creator = match *file {
            "CLAUDE_BASE" | "claude-desktop-worktree" => "Claude Code",
            "codex-thread.json" | "codex-synced-branch.json" => "Codex",
            _ => "unknown",
        };
        assert_eq!(r.creator, creator, "{file}: {:?}", r.evidence);
        if creator != "unknown" {
            let line = format!("probably made by {creator}: record file {file}");
            assert!(says(r, &line), "{file}: {:?}", r.evidence);
        }
    }
}

#[test]
fn config_worktree_passes_only_with_keys_agents_are_known_to_set() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    let repo = code.join("app");
    app(&base, &repo);
    let set = |wt: &Path, key: &str, value: &str| {
        git(wt, &["config", "--worktree", key, value]);
    };

    // What the Claude desktop app and Codex set, with `core.worktree` naming the folder.
    let allowed = add(&repo, "config-allowed", "config-allowed");
    set(&allowed, "core.untrackedCache", "true");
    set(&allowed, "core.fsmonitor", "false");
    set(
        &allowed,
        "codex.localEnvironmentConfigPath",
        "/opt/env.toml",
    );
    set(&allowed, "core.worktree", s(&allowed));
    // git reads a relative `core.worktree` from the record's folder.
    let relative = add(&repo, "config-relative", "config-relative");
    set(&relative, "core.worktree", "../../../../config-relative");

    // A hook path beside an allowed key, an identity, and a work tree aimed elsewhere.
    let hooks = add(&repo, "config-hooks", "config-hooks");
    set(&hooks, "core.untrackedCache", "true");
    set(&hooks, "core.hooksPath", "/opt/private-hooks");
    let email = add(&repo, "config-email", "config-email");
    set(&email, "user.email", "someone@example.com");
    let elsewhere = add(&repo, "config-elsewhere", "config-elsewhere");
    let other = base.join("other-folder");
    fs::create_dir_all(&other).unwrap();
    set(&elsewhere, "core.worktree", s(&other));

    let v = view(&home);
    expect(
        &v,
        &[
            ("config-allowed", "can go", "as of the last fetch"),
            ("config-relative", "can go", "as of the last fetch"),
            (
                "config-hooks",
                "needs a look",
                "also holds config.worktree (core.hookspath).",
            ),
            (
                "config-email",
                "needs a look",
                "also holds config.worktree (user.email).",
            ),
            (
                "config-elsewhere",
                "needs a look",
                "also holds config.worktree (core.worktree).",
            ),
        ],
    );
    // Key names are evidence; values stay out of it.
    for (name, value) in [
        ("config-hooks", "private-hooks"),
        ("config-email", "example.com"),
        ("config-elsewhere", "other-folder"),
    ] {
        let r = row(&v, name);
        assert!(!says(r, value), "{name} shows {value:?}: {r:?}");
    }
}

#[test]
fn finished_means_merged_or_pushed_as_of_the_last_fetch_with_no_stash() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    let repo = code.join("app");
    new_repo(&base, &repo);
    let remote = base.join("app.git");

    let live = add(&repo, "live-upstream", "live-upstream");
    commit(&live, "live.txt");
    git(&live, &["push", "-q", "-u", "origin", "live-upstream"]);
    let gone = add(&repo, "gone-only", "gone-only");
    commit(&gone, "gone.txt");
    git(&gone, &["push", "-q", "-u", "origin", "gone-only"]);
    git(&repo, &["push", "-q", "origin", "--delete", "gone-only"]);
    git(&repo, &["fetch", "-q", "--prune"]);
    // Deleted on the remote after that fetch: the remote-tracking ref still vouches for it.
    let stale = add(&repo, "stale-upstream", "stale-upstream");
    commit(&stale, "stale.txt");
    git(&stale, &["push", "-q", "-u", "origin", "stale-upstream"]);
    git(&remote, &["branch", "-D", "stale-upstream"]);
    let local = add(&repo, "local-upstream", "local-upstream");
    commit(&local, "lu.txt");
    git(&repo, &["branch", "-q", "fake", "main"]);
    git(&local, &["branch", "-q", "-u", "fake"]);

    // Stash entries: on a branch, on a detached HEAD, and on one that moved on since.
    let on_branch = add(&repo, "stash-branch", "stash-branch");
    write(&on_branch.join("README"), "stashed\n");
    git(&on_branch, &["stash", "-q"]);
    let on_detached = detach(&repo, "stash-detached", "main");
    write(&on_detached.join("README"), "stashed detached\n");
    git(&on_detached, &["stash", "-q"]);
    let moved_on = detach(&repo, "stash-detached-moved", "main");
    write(&moved_on.join("README"), "stashed, then moved\n");
    git(&moved_on, &["stash", "-q"]);
    // A branch renamed after its stash: the entry names the old branch. Stash entries
    // belong to the repo and outlive the worktree, so this one is not counted against it.
    let renamed = add(&repo, "before", "renamed");
    write(&renamed.join("README"), "stashed before the rename\n");
    git(&renamed, &["stash", "-q"]);
    git(&renamed, &["branch", "-m", "after"]);
    // A commit only HEAD's reflog holds: the reflog is git's recovery log, not a ref.
    let reflog = add(&repo, "reflog-only", "reflog-only");
    git(&reflog, &["checkout", "-q", "--detach"]);
    commit(&reflog, "rr.txt");
    git(&reflog, &["checkout", "-q", "reflog-only"]);
    commit(&repo, "more.txt");
    git(&repo, &["push", "-q", "origin", "main"]);
    git(&moved_on, &["checkout", "-q", "--detach", "main"]);

    // The remote rewrote main from C back to B, and only the local main followed: the
    // stale origin/main still holds C, and no branch here does.
    let rewritten_repo = code.join("app2");
    new_repo(&base, &rewritten_repo);
    let b = head(&rewritten_repo);
    commit(&rewritten_repo, "c.txt");
    git(&rewritten_repo, &["push", "-q", "origin", "main"]);
    detach(&rewritten_repo, "rewritten", "main");
    git(
        &base.join("app2.git"),
        &["update-ref", "refs/heads/main", &b],
    );
    git(&rewritten_repo, &["reset", "-q", "--hard", &b]);

    // No origin/HEAD, main or master: the main checkout's feature branch is no default.
    let seed = base.join("seed-trunk");
    write(&seed.join("README"), "trunk\n");
    git(&base, &["init", "-q", "-b", "trunk", s(&seed)]);
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-qm", "first"]);
    let trunk_remote = base.join("trunk.git");
    git(
        &base,
        &["clone", "-q", "--bare", s(&seed), s(&trunk_remote)],
    );
    let trunk = code.join("trunk-app");
    git(&base, &["clone", "-q", s(&trunk_remote), s(&trunk)]);
    git(&trunk, &["remote", "set-head", "origin", "-d"]);
    git(&trunk, &["checkout", "-q", "-b", "feature"]);
    commit(&trunk, "unpushed.txt");
    detach(&trunk, "on-feature", "feature");

    let v = view(&home);
    expect(
        &v,
        &[
            (
                "live-upstream",
                "can go",
                "every commit is on origin/live-upstream as of the last fetch or push (last fetch ",
            ),
            (
                "stale-upstream",
                "can go",
                "every commit is on origin/stale-upstream as of the last fetch or push (last fetch ",
            ),
            (
                "gone-only",
                "needs a look",
                "its upstream origin/gone-only is gone",
            ),
            ("local-upstream", "active", "has commits not in origin/main"),
            (
                "stash-branch",
                "needs a look",
                "1 stash entry names its branch",
            ),
            ("stash-detached", "needs a look", "stash entr"),
            ("stash-detached-moved", "can go", "as of the last fetch"),
            ("renamed", "can go", "as of the last fetch"),
            ("reflog-only", "can go", "as of the last fetch"),
            (
                "rewritten",
                "needs a look",
                "its detached HEAD holds 1 commit that no branch or tag holds",
            ),
            (
                "on-feature",
                "active",
                "its repo has no main branch to compare it with",
            ),
        ],
    );
    assert!(
        says(row(&v, "rewritten"), "no fetch on record"),
        "{:?}",
        row(&v, "rewritten").evidence
    );
}
