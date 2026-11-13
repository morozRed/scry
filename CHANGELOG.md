# Changelog

All notable changes to Scry are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Homebrew:** `brew install morozRed/tap/scry` installs a prebuilt binary. Each tagged
  release builds it, publishes it on GitHub and updates the tap.
- After an upgrade, the daemon restarts itself on the new binary within a minute, instead
  of running the old one until `scry daemon install`.

- **Top** (`6`, and `scry top`): what uses the CPU and memory. Your processes are grouped by
  the app or program they belong to, ranked by load, with CPU and memory bars over the list
  and a meter on each row; groups open with `space`, and `x` stops a process. Without root
  macOS reads only your own processes, so the rest of the machine is one line, the total
  less what yours account for.
- **`scry setup`** installs a skill that teaches Claude Code, Codex, OpenCode or Gemini CLI
  to use Scry's CLI: when to reach for it, which command answers what, and to act only after
  you agree. Pick agents from a list, name them, or pass `--all`; `--remove` takes it out.

### Fixed

- A Disk reading walked every folder again whenever a changed one held a hard-linked file,
  which local git clones and pnpm make common, so cached sizes rarely helped: on the
  reference Mac a reading with 60 changed folders of 37,000 took 105 s. Cached sizes now
  keep their hard-linked files apart and count them across all folders on each reading,
  so only the changed folders are walked; the same reading takes 9–18 s. The first reading
  after updating walks everything once.
- The pulse never reached a live board, so the Overview's CPU and memory gauges stayed on
  "warming up" and the header showed process counts. CPU, memory pressure, swap and free
  disk now show, CPU from the second reading a second after start.

### Changed

- Folder walks on Disk and Worktrees, cleanups and the background recorder run at macOS's
  utility priority, so they give way to your own work.
- Linux support moved from "not planned" to the roadmap, with a plan for each part that
  reads the machine.

- **A new look for the terminal UI**, after ghui and disktree. One header line with the
  views and the pulse; lists without column titles, the state as a glyph in a gutter; the
  detail beside the list from 100 columns, with the state and facts as filled chips and its
  sections set apart by rules. The Overview opens with gauges for CPU, memory and the disk,
  and Disk with the volume as a bar and its items as a treemap coloured by kind, hatched
  where Scry can clean them up. `v` switches Disk to the ranked list.
- **Navigation.** `enter` opens the selected row's detail over the whole screen (the
  Overview still goes to its row), and `space` folds a session. `ctrl-p` or `:` opens a
  palette of every command that applies here. The mouse picks views, rows and tiles and
  scrolls with the wheel; the arrows move between treemap tiles.

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
