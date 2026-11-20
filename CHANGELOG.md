# Changelog

All notable changes to Scry are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.1] - 2026-11-20

### Fixed

- **Disk cleanup and safety:**
  - Trashing Claude Code or Codex session folders now warns when the agent is running (the
    check previously read an unlabelled snapshot).
  - Unreadable symlinks, symlink loops, and unresolvable relative paths now fail closed
    instead of passing protected-path checks; symlinked catalog entries are resolved before
    inspecting them, and tool-clone stash and config checks fail closed on errors or
    symlinks.
  - FSEvents history queries now drain their dispatch queue before releasing shared state on
    timeout, avoiding a potential use-after-free, and handle poisoned locks without panicking
    across FFI.
  - Background git commands now kill their process group on fast exit too, preventing hangs
    when grandchildren hold open pipes.
- **Lineage, keep list, hooks and stop:**
  - Out-of-order hook events in a single spool batch now apply in timestamp order so a
    resumed session is never left ended, and resuming a hook session preserves its original
    start time and working directory in the store.
  - Live agent-tree processes holding listening ports without an environment stamp now show
    as live instead of unowned.
  - Keeping a shared script run directly in `~` now expands and matches cleanly, and `~`
    display in Disk, Worktrees, and setup no longer shows `~/`.
  - `SIGKILL` outcomes now report the elapsed wait time instead of `0 ms`, and `--kill`
    confirmation prompts note that `SIGKILL` follows if `SIGTERM` does not stop the process.
  - Claude Code hook installation accepts prerelease and build-tagged version strings and
    cleans up temporary or backup files if writing fails; hook runner ancestor walks continue
    past processes whose executable path cannot be read.
  - Older databases that already have session tables are now readable without requiring the
    latest unrelated schema version.
- **CLI and terminal UI:**
  - `scry watch --json` no longer emits a line on every tick from moving CPU and memory
    usage, and `scry agents` still shows config errors and caveats when the list is empty.
  - Leading or trailing whitespace around a cleanup confirmation word in the TUI now matches
    validation instead of failing at execution; `Alt` chords no longer type into cleanup
    confirmations or the command palette, and `Ctrl-D`/`Ctrl-U` no longer scroll the list
    behind open overlays.
  - Failed live refreshes now mark Agents and Top as failed rather than leaving them stuck
    loading.
  - Terminal writes retry on `EINTR`, and unfinished DA1 replies that time out are dropped
    instead of leaking as typed keys.
  - Fixed narrow/empty treemap and gauge underflows, sub-kilobyte `"BB free"` disk gauge
    formatting, over-wide word line-count estimation in cleanup sheets, and `NAME` header
    alignment in `scry top`.
  - Sanitized untrusted paths and names in Worktrees, Disk, and Reap output; deduplicated
    repeated agent names in `scry setup`; rejected repeated leading colons in `scry ports`.

### Changed

- Removed quadratic hot-path scans in lineage attribution, Top grouping, reap ordering, TUI
  board refreshes, and table sizing; cached static memory and page-size lookups; skipped
  per-process rusage calls for other users' processes when unprivileged; and applied macOS
  utility QoS to disk-catalog, sizing, and worktree worker threads.

## [0.1.0] - 2026-11-13

The first public release.

### Added

- **Install with Homebrew:** `brew install morozRed/tap/scry` installs a prebuilt binary
  for Apple silicon.
- **Terminal UI.** `scry` opens six views: Overview, Ports, Agents, Disk, Worktrees and
  Top. One header line holds the views and the pulse (CPU, memory pressure, swap and free
  disk); lists show each row's state as a glyph in a gutter, with the detail beside the
  list from 100 columns, explaining the selected row in one sentence with its evidence.
  Every view has a fuzzy filter, sorting, and empty, loading and error states. `enter`
  opens a row's detail over the whole screen, `space` folds a session, `ctrl-p` or `:`
  opens a palette of every command that applies, and the mouse picks views, rows and tiles.
  It follows the terminal's light or dark theme, honours `NO_COLOR`, works from 80×24 up,
  refreshes live every 5 seconds by default, and restores the terminal after quit,
  SIGTERM, Ctrl-Z or a closed terminal.
- **Ports.** `scry ports` lists every listening TCP and UDP port on the Mac, for all users
  and without root, with its process, project and agent session. `scry why <port>`
  explains who holds a port, with the evidence.
- **Agents.** Claude Code, Codex, OpenCode and Gemini CLI sessions are recognised in every
  install style Scry has a fixture for. `scry agents --tree` shows each session's
  processes, tied by environment stamp, process tree or, shown only, a guess.
  `scry leftovers` lists what ended sessions left running.
- **Top.** `scry top` and the Top view show what uses the CPU and memory, your processes
  grouped by the app or program they belong to and ranked by load. Without root macOS
  reads only your own processes, so the rest of the machine is one line.
- **Stop and reap.** `scry stop <pid|:port>`, `scry reap`, and `x` and `X` in the UI stop
  processes after a confirmation, children first, with SIGTERM and then, only when asked,
  SIGKILL. The process and the config are checked again right before every signal.
- **Keep list.** `scry keep <pid>` and `K` in the UI spare a process from every rule,
  editing only the `keep` list in the config.
- **Worktrees.** `scry worktrees` finds linked git worktrees under your code roots and in
  agents' folders, with their state, size and probable creator.
- **Disk.** `scry disk` shows free space as Finder counts it, a forecast of when it runs out,
  and the largest caches, `node_modules` folders and worktrees, ranked by what deleting them
  frees; the Disk view draws them as a treemap, and `v` switches to a ranked list.
  `scry clean` and `t` in the UI move an item to the Trash, delete a cache after its name
  is typed, show the tool's own cleanup command, or prune the record of a worktree whose
  folder is gone. Sizes are cached and checked against FSEvents, so a reading walks only
  the folders that changed, hard-linked files from local git clones and pnpm included.
- **Background recorder.** `scry daemon install` installs a LaunchAgent that samples free
  space, takes hook events and records agent process trees. `scry daemon status` and
  `scry daemon uninstall` go with it. After an upgrade the daemon restarts itself on the
  new binary within a minute.
- **Claude Code hooks.** `scry hook install claude` adds SessionStart and SessionEnd hooks
  after showing the diff and backing up the file. `scry hook uninstall claude` removes
  them.
- **For scripts and agents.** Every command has `--json`, bare `scry` without a terminal
  prints the Overview, and `scry watch --json` streams every view as JSON lines.
  `scry setup` installs a skill that teaches Claude Code, Codex, OpenCode or Gemini CLI
  when and how to use Scry, and to act only after you agree.
- **Diagnostics.** `scry doctor` checks each part with a fix for each problem.
  `scry snapshot` writes a redacted snapshot, and `scry bench scan` times a scan.
- **Configuration** in `~/.config/scry/config.toml`: `code_roots`, `destructive`, `keep`,
  `leftover_grace_secs`, `[daemon] sample_interval_secs` and `[tui] refresh_secs`.
  `SCRY_HOME` moves the config and data to one folder.

### Safety and privacy

- `destructive = false` refuses every stop, reap and cleanup.
- Every action writes an audit row before it acts, and does nothing when it cannot.
- System processes, other users' processes, and Desktop, Documents and Downloads are never
  touched.
- Commands are shown as labels built only from parts known to be safe; raw command lines
  never appear in JSON or snapshots. Scry has no network code and no telemetry.
- Folder walks, cleanups and the background recorder run at macOS's utility priority, so
  they give way to your own work.

[Unreleased]: https://github.com/morozRed/scry/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/morozRed/scry/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/morozRed/scry/releases/tag/v0.1.0
