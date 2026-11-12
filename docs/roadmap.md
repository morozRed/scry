# Roadmap

Scry is a macOS system monitor whose edge is **lineage**: every process, port, worktree
and gigabyte is traced back to the project and the agent session that made it. This page
says what version 0.1 includes, what comes next, and what is not planned. For how the code
works, see [architecture.md](architecture.md).

## Principles

These hold for every release.

- **Answer "is anything wrong?" in one glance.** The Overview is a ranked list of what
  needs attention, not a wall of gauges. When it is empty, it says so.
- **Every row explains itself.** The detail pane leads with one plain sentence and the
  evidence behind it.
- **State is a word and a glyph, never colour alone.** The UI follows the terminal's light
  or dark theme, and works with `NO_COLOR`.
- **Quiet by default.** Scry redraws only when data changes and samples slowly.
- **Nothing destructive happens on one key.** Every action opens a sheet listing the exact
  targets first, and `destructive = false` in the config turns every action off.
- **Agents get the same answers as people.** Every command has `--json`, and bare `scry`
  without a terminal prints a plain summary instead of opening the UI. Agents call the CLI;
  there is no MCP server.
- **Private by design.** No network code, no telemetry, no raw command lines in output.

## Version 0.1 (the first release)

| Area | What it does |
|---|---|
| Terminal UI | `scry` opens five views: Overview, Ports, Agents, Disk and Worktrees. Each has a detail pane, a fuzzy filter, sorting, guarded actions, and empty, loading and error states that say how to fill the view. It works from 80×24 up, in light and dark terminals. |
| Ports | Every listening port on the Mac, yours and other users', traced to its process, project and agent session. `scry why` explains one port with its evidence. Stop a process with SIGTERM, and SIGKILL after a second confirmation. |
| Agents | Claude Code, Codex, OpenCode and Gemini CLI sessions as a tree, with RAM and ports. Processes left running after a session ended are flagged as leftovers and can be stopped after review. A keep list spares processes you meant to leave running. |
| Disk | Free space as Finder counts it, a forecast of when it runs out, and the largest caches, `node_modules` folders and worktrees, ranked by what deleting them frees. Each item allows one cleanup: Trash, delete, a command for you to run, or none. |
| Worktrees | Every linked git worktree under your code roots, with its state (can go, dirty, active, locked, …), its size and the agent that probably made it. |
| Pulse | A header strip with CPU, memory pressure and swap. |
| CLI | `overview`, `ports`, `why`, `stop`, `agents`, `leftovers`, `reap`, `keep`, `worktrees`, `disk`, `clean`, `watch`, each with `--json`. |
| Background recorder | An optional LaunchAgent that records disk history for the forecast, takes hook events, and remembers which session each process belonged to while its agent lived. |
| Claude Code hooks | `scry hook install claude` adds SessionStart and SessionEnd hooks, so sessions are known exactly. |
| Health | `scry doctor` checks each part and says how to fix what is wrong. |

Requirements: macOS 14 or later on Apple Silicon. No root access is needed.

## Next

### A menu bar item

- An icon that carries the number of things that need attention.
- A panel with the pulse strip and the Overview list. A row expands to its sentence and
  evidence, can be stopped after a native confirmation, or opened in the terminal UI at
  that row.
- Notifications for a new leftover, a disk threshold and a watched port.
- A signed, notarized app bundle installed as a Homebrew cask.

The menu bar item reads the same view models the terminal UI draws, through
`scry watch --json`. The toolkit is still being chosen; a short spike will measure the
time from click to first frame and the idle cost before work starts.

### The full app

- Ports, Agents, Disk and Worktrees join the panel as tabs, with the same filter and
  actions as the terminal UI.
- The panel can detach into a standalone window. The menu bar item stays the default.
- Every sentence and count in the app matches the terminal UI for the same data, because
  both draw one set of view models.

### Distribution

- A Homebrew tap with the formula in [`packaging/homebrew/scry.rb`](../packaging/homebrew/scry.rb).
- Release binaries signed with a Developer ID.

## Later, not yet scheduled

- Hooks for Codex and Gemini CLI, once their payloads are checked against real sessions.
  Until then they are tracked by environment stamp and process tree.
- A socket on the daemon, when a client needs events pushed to it or cannot scan for
  itself.
- Approving an agent's request to stop a process or trash a folder from inside the UI.
- GPU, battery and network in the pulse strip. Other tools already do these well.

## Not planned

- Linux or Intel Macs.
- Per-app network traffic.
- Historical process charts.
- Remote Macs: `ssh` in and run `scry`.

## Known limits

- Scry cannot tell abandoned work from a job you meant to outlive its session, such as one
  started with `nohup`. Add such a job to the keep list.
- The socket table comes from a private kernel layout. A contract test checks it against
  `netstat`, and the Ports view says so when the table cannot be read. A `scry doctor`
  check for it is planned.
- Agents' environment keys are undocumented and may change. Each install style has a
  fixture, and the process tree is a second source of evidence.
- The disk forecast needs 48 hours of samples from the background recorder.
