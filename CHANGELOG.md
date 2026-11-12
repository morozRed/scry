# Changelog

All notable changes to Scry are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0]

The first public release.

### Added

- **Terminal UI.** `scry` opens five views: Overview, Ports, Agents, Disk and Worktrees.
  Each has a detail pane that explains the selected row in one sentence with its evidence,
  a fuzzy filter, sorting, and empty, loading and error states. It follows the terminal's
  light or dark theme, honours `NO_COLOR`, works from 80×24 up, refreshes live every 5
  seconds by default, and restores the terminal after quit, SIGTERM, Ctrl-Z or a closed
  terminal.
- **Ports.** `scry ports` lists every listening TCP and UDP port on the Mac, for all users
  and without root, with its process, project and agent session. `scry why <port>`
  explains who holds a port, with the evidence.
- **Agents.** Claude Code, Codex, OpenCode and Gemini CLI sessions are recognised in every
  install style Scry has a fixture for. `scry agents --tree` shows each session's
  processes, tied by environment stamp, process tree or, shown only, a guess.
  `scry leftovers` lists what ended sessions left running.
- **Stop and reap.** `scry stop <pid|:port>`, `scry reap`, and `x` and `X` in the UI stop
  processes after a confirmation, children first, with SIGTERM and then, only when asked,
  SIGKILL. The process and the config are checked again right before every signal.
- **Keep list.** `scry keep <pid>` and `K` in the UI spare a process from every rule,
  editing only the `keep` list in the config.
- **Worktrees.** `scry worktrees` finds linked git worktrees under your code roots and in
  agents' folders, with their state, size and probable creator.
- **Disk.** `scry disk` shows free space as Finder counts it, a forecast of when it runs out,
  and the largest caches, `node_modules` folders and worktrees, ranked by what deleting them
  frees. `scry clean` and `t` in the UI move an item to the Trash, delete a cache after its
  name is typed, show the tool's own cleanup command, or prune the record of a worktree
  whose folder is gone. Sizes are cached and checked against FSEvents.
- **Background recorder.** `scry daemon install` installs a LaunchAgent that samples free
  space, takes hook events and records agent process trees. `scry daemon status` and
  `scry daemon uninstall` go with it.
- **Claude Code hooks.** `scry hook install claude` adds SessionStart and SessionEnd hooks
  after showing the diff and backing up the file. `scry hook uninstall claude` removes
  them.
- **For scripts and agents.** Every command has `--json`, bare `scry` without a terminal
  prints the Overview, and `scry watch --json` streams every view as JSON lines.
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

[Unreleased]: https://github.com/morozRed/scry/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/morozRed/scry/releases/tag/v0.1.0
