# Scry agent guide

Scry is a macOS system monitor for coding agents. It links processes, listening
ports, worktrees, and disk usage to projects and sessions where evidence allows.
The CLI and terminal UI use the same engine. The optional daemon records history
so both can recover evidence a live scan would miss.

Read [CONTRIBUTING.md](CONTRIBUTING.md) for setup and pull requests,
[docs/architecture.md](docs/architecture.md) for data flow, and
[docs/design.md](docs/design.md) for terminal UI conventions.

## Find the code

<!-- CODEGRAPH_START -->
If `.codegraph/` exists at the repository root, use `codegraph_explore` or
`codegraph explore "<symbol or question>"` before searching or reading code.
Name a file or symbol to get its current source and callers. If the index is
absent, use the repository files directly. Indexing is the user's decision.
<!-- CODEGRAPH_END -->

| Change | Start here |
| --- | --- |
| macOS process and socket reads | [`sys`](crates/scry-core/src/sys.rs), [`engine.rs`](crates/scry-core/src/engine.rs) |
| Session evidence and leftover verdicts | [`attribution.rs`](crates/scry-core/src/attribution.rs), [`live.rs`](crates/scry-core/src/live.rs) |
| Shared view models | [`scry-core/src/view`](crates/scry-core/src/view) |
| Stop and reap | [`control.rs`](crates/scry-core/src/control.rs), [`reap.rs`](crates/scry-core/src/reap.rs) |
| Disk catalog and cleanup | [`disk/catalog.rs`](crates/scry-core/src/disk/catalog.rs), [`disk/clean.rs`](crates/scry-core/src/disk/clean.rs) |
| Worktree discovery and status | [`worktree`](crates/scry-core/src/worktree), [`git.rs`](crates/scry-core/src/git.rs) |
| Background recording and persistence | [`recorder.rs`](crates/scry-core/src/recorder.rs), [`spool.rs`](crates/scry-core/src/spool.rs), [`store.rs`](crates/scry-core/src/store.rs) |
| CLI and terminal UI | [`main.rs`](crates/scry/src/main.rs), [`tui/app.rs`](crates/scry/src/tui/app.rs), [`tui/views`](crates/scry/src/tui/views), [`tui/kit`](crates/scry/src/tui/kit) |

## Preserve these invariants

- **Keep decisions in `scry-core`.** The CLI and TUI draw from shared verdicts and
  view models. Keep terminal types in `scry`. Keep live reads usable with the
  recorder stopped.
- **Identify a process by `ProcId`.** A PID can be reused. Revalidate PID and
  start time before each signal. Only stamp or process-tree evidence can qualify
  a process for reap. Preserve keep-list and protected-process checks.
- **Treat a cleanup plan as a preview.** Recheck `destructive`, target identity,
  and protected paths at execution time. Preserve the no-symlink and
  no-cross-volume checks. Trash eligible worktrees and limit permanent deletion
  to rebuildable caches with typed confirmation.
- **Write audit intent before acting.** Stop, reap, and cleanup must refuse the
  action if the intent cannot be written. Preserve the report of what changed
  when a cleanup stops partway through.
- **Use synthetic data for committed fixtures and screenshots.** Keep recorded
  snapshots in the ignored `fixtures/recorded/` directory. Home-path redaction
  leaves process names and other paths that can still identify the machine.

## Run and verify

Use Apple Silicon macOS 14 or later and Rust 1.89 or later. From the repo root:

```sh
cargo run -- tui --fixture fixtures/typical.json
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

- Set `SCRY_HOME` to a throwaway directory for manual tests that use Scry state.
  Use throwaway processes and directories as targets when exercising actions.
- For TUI changes, review the changed frames with `cargo insta review`. If the
  visible result changes, regenerate the screenshots with `scripts/shots.sh`.
- For terminal lifecycle changes, build the binary with `cargo build` and run
  `scripts/pty-smoke.sh target/debug/scry` with `tmux` installed.
- Before opening a PR, run `scripts/local-ci.sh HEAD` on the proposed commit.
  The script checks a clean worktree at that commit.
