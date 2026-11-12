# Scry build plan

Revision 2, 9 Nov 2026. TUI first.

Source: the doc *Cowl, Scry & Sigil: Names, Features and Pains* (8 Nov 2026).
Scry is a macOS system monitor whose edge is **lineage**. Every process, port,
worktree and gigabyte is traced back to the project and agent session that made it.
Agents get the same answers from the CLI, where every command has `--json`.

## 0. Finish line for the v1 autopilot run

v1 is finished when M0 to M6 are merged into `main` of `morozRed/scry` and all of
these hold on the reference Mac from a clean clone:

1. **The TUI works on live data.** `scry` opens Overview, Ports, Agents, Disk and
   Worktrees. Each view has a detail pane that explains the selected row in one
   sentence with its evidence, a filter, a sort, and the actions from section 2,
   each behind a confirmation sheet.
   On first run each view says what it will show and how to fill it (for example,
   which command installs the agent hooks). Live data refreshes every 2 s by
   default, set by `[tui] refresh_secs` in the config, and `r` refreshes at once.
2. **It looks good, by evidence.** Two or three looks are rendered and one is
   picked. Screenshots of every view in light and dark, at 80×24 and 120×40, are
   committed and shown in the README. No layout breaks from 80×24 up. Every view
   has empty, loading and error states.
3. **It is quiet and fast.** A frame renders in under 16 ms. The idle TUI stays
   under 1% of one core over 10 minutes. The daemon stays under 0.5% CPU and
   40 MB over an idle hour.
4. **The CLI matches the TUI.** `overview`, `ports`, `why`, `agents`,
   `leftovers`, `disk`, `worktrees`, `stop`, `reap` and `watch`, each with
   `--json`.
5. **Agents get the same answers from the CLI.** Every command has `--json`, and
   bare `scry` without a terminal prints a summary. There is no MCP server; the
   operator dropped it because agents call the CLI easily.
6. **The background parts install and report.** `scry daemon install`,
   `scry hook install claude` and `scry doctor`.
7. **Every M0 to M6 exit criterion is a test or a recorded number.**
8. **Each milestone lands as its own PR,** reviewed by agents that did not write
   it, with green CI.
9. **It is ready to open-source.** A stranger can read the README, build it and
   contribute: MIT `LICENSE`, `CONTRIBUTING.md`, crate metadata, planning notes
   rewritten for outside readers under `docs/`, and `scripts/check-public.sh`
   (run by local CI) finding no personal paths, emails, private IPs or
   credentials in tracked files. Fixtures and screenshots use synthetic data.

Left to the operator: publishing a Homebrew tap, Developer ID signing,
installing hooks into the real `~/.claude/settings.json`, and leaving the
LaunchAgent running. Waiting for later work: M7 and M8, a live disk forecast
(it needs 48 hours of samples), and Codex or Gemini hooks whose payloads cannot
be verified.

## What changed in this revision

1. **The first face is a terminal UI.** The menu bar item and the app follow v1 as
   scheduled milestones M7 and M8. Rune, an app bundle, signing and notarization
   leave the v1 critical path. Section 2 lists what v1 keeps ready for the app.
2. **All 15 findings from `ASSESSMENT.md` are folded in.** Section 11 maps each one
   to where it landed.
3. **The daemon records, it does not serve.** A TUI is already a long-running
   process, so it scans in-process. The socket and its protocol wait for the first
   client that needs them. Section 3 explains the trade.
4. **Four fixes from the second review** that neither the first plan nor the
   assessment had:
   - Multiplexer servers (`tmux`, `screen`, `zellij`) join the never-reap list, and
     the leftover rule spares any process with a live tty descendant. A tmux server
     first started from inside an agent's shell passes the agent's env stamp to
     every later pane.
   - An MCP server identifies its caller by parent pid, not by `CLAUDE_PID`. The
     three MCP servers under this Claude session carry `CLAUDE_CODE_SESSION_ID`
     but no `CLAUDE_PID` (measured here).
   - Bare `scry` without a TTY prints a plain summary and exits. Agents will run it
     from their Bash tool, and a TUI there would hang them.
   - `pcblist_n` is a private kernel layout. It gets a contract test against
     `netstat -anv` and a `doctor` check.

## 1. Scope

### v1
| Area | Result |
|---|---|
| TUI | `scry` opens a full-screen terminal app with five views: Overview, Ports, Agents, Disk, Worktrees. Filter, sort, detail pane, guarded actions. |
| Ports | Port to process to project root to agent session, with age and evidence. Stop with SIGTERM. Other users' listeners are shown, not hidden. |
| Agents | Claude Code, Codex, OpenCode and Gemini CLI sessions as a tree with RAM and ports. Leftovers are flagged and can be reaped after review. |
| Disk | Free space, trend, forecast, top eaters by kind. Sizes show what a delete gives back. Cleanup is per kind. |
| Worktrees | Dirty, merged, branch gone, orphaned. Size including deps. Probable creator with its evidence. |
| Pulse | A header strip: CPU, memory pressure, swap. |
| CLI | Every view has a command with `--json`: `overview`, `ports`, `why`, `agents`, `leftovers`, `disk`, `worktrees`, `stop`, `reap`. `scry watch --json` streams them. |

### After v1, scheduled
| Release | Result |
|---|---|
| v1.1 (M7) | A menu bar item. The icon carries the attention count. Clicking it opens a panel with the pulse strip and the Overview list. Rows can be stopped or opened in the TUI. Desktop notifications. |
| v1.2 (M8) | The full app. All five views in the panel, and the same views as a standalone window. |

### After v1, unscheduled
- The socket protocol for Cowl and Sigil.
- GPU, battery and network in Pulse. Stats already does these well.
- Approving an agent's stop or trash request inside the TUI.

### Not planned
Linux, per-app network traffic, historical process charts. Remote Macs need no
feature: `ssh` in and run `scry`.

## 2. The TUI

### Principles
- **Answer "is anything wrong?" in one glance.** The Overview is a ranked list of
  things that need attention, not a wall of gauges. Empty means "Nothing needs
  attention".
- **Every row can explain itself.** The detail pane leads with one plain sentence
  and the evidence behind it.
- **State is a word and a glyph, never colour alone.** The palette is the
  terminal's own 16 ANSI colours, so it follows the user's theme in light and dark.
- **Quiet by default.** It redraws only when data changes. It samples slowly when
  the terminal loses focus.
- **Nothing destructive happens on one key.** Every action opens a sheet listing the
  exact targets first.

### Layout

```
 scry   cpu ▂▃▅▃▂ 23%   mem ● normal   swap 1.2G   disk 84G free, full in ~19d
 1 Overview   2 Ports   [3 Agents ·3]   4 Disk   5 Worktrees
────────────────────────────────────────────────────────────────────────────────
 SESSION / PROCESS                     RAM   PORT    AGE   EVIDENCE   STATE
 ▾ claude  acme/wt-auth                931M          2h               ended
   ├ next dev                          412M  :3000   2h    stamp      leftover
   ├ node mcp-server-github            218M          2h    stamp      leftover
   └ chrome --headless                 301M          2h    tree       leftover
 ▾ claude  scry                        134M          41m              live
   ├ node codegraph                     96M          41m   stamp
   └ paper mcp                          38M          41m   stamp
 ▸ codex   billing-api (4 processes)   1.1G  :5173   3h               live
────────────────────────────────────────────────────────────────────────────────
 next dev   pid 48213   started 14:02
 :3000 is held by `next dev` in ~/acme/wt-auth. Its Claude Code session ended
 2 h ago.
 Evidence: CLAUDE_CODE_SESSION_ID in the process environment (exact).
────────────────────────────────────────────────────────────────────────────────
 j/k move   ⏎ open   / filter   x stop   X reap leftovers   K keep   ? help
```

The detail pane sits below the list under 120 columns and beside it above that.
Below 80×24 the app shows a "terminal too small" line instead of a broken layout.

### Views
| Key | View | Shows |
|---|---|---|
| `1` | Overview | Attention list across all areas: leftovers, ports held by ended sessions, disk forecast, stale worktrees. Enter jumps to the row's view. |
| `2` | Ports | Listening sockets with process, project, session, owner, age. |
| `3` | Agents | Sessions as a tree, with evidence and state per process. |
| `4` | Disk | Free space and trend, then eaters ranked by reclaimable size, with the cleanup each kind allows. |
| `5` | Worktrees | State, branch, size, probable creator. |

### Keys
`1` to `5` switch views. `j`/`k` or arrows move. `enter` opens or expands. `/` filters.
`s` cycles sort. `y` copies the selected path, pid or port. `x` stops the selected
process. `X` reaps leftovers. `K` adds to the keep list. `t` cleans up a disk or
worktree item. `r` rescans. `?` help. `q` quits. Mouse is optional and off the
critical path.

### Actions
- `x` and `X` open a sheet listing each target: pid, start time, command, session,
  evidence and the RAM it frees. `enter` confirms and `esc` cancels.
- Stop sends SIGTERM. If the process is still alive after 3 s, the sheet offers
  SIGKILL as a second, separate confirmation.
- Identity `(pid, start_time)` is re-checked immediately before every signal.
- `X` acts children first and never includes a process whose evidence is `guess`.
- Every accepted action writes an audit row.

### Terminal behaviour
- Bare `scry` opens the TUI when stdout is a TTY. Otherwise it prints the Overview
  as plain text and exits. `scry tui` forces the TUI and fails clearly without a TTY.
- The terminal is restored on quit, panic, SIGINT, SIGTERM and Ctrl-Z.
- `NO_COLOR` is honoured.
- Scry excludes its own process tree from leftovers.
- `scry tui --fixture <file>` renders from a recorded snapshot. Design work,
  screenshots and tests all use it, so none of them depends on live machine state.

### Stack
| Need | Choice |
|---|---|
| Rendering | `ratatui` 0.30 with `crossterm` 0.29. `ratatui::init` and `restore` install the panic hook. |
| Tree and tables | The built-in `Table`, with the tree flattened into rows. No tree crate, because columns must align across depths. |
| Filter | Substring match first. `nucleo-matcher` only if it feels weak. |
| Sparklines, gauges | Built-in `Sparkline` and `Gauge`. |
| Tests | `TestBackend` with `insta` snapshots, plus one PTY smoke test. |
| Concurrency | Standard threads and channels. No async runtime in the TUI. |

The TUI is a model, update, view loop:
- The collector thread sends an immutable `Snapshot`.
- A pure function turns `Snapshot` plus UI state into a `ViewModel` of rows, badges
  and sentences. This is where the tests live.
- `update` handles keys and returns commands such as `Stop` or `Reap`, which run off
  the UI thread.
- The frame is drawn only when the model changed. Sparklines tick at 1 Hz.
- Sampling runs every 2 s while focused and every 10 s after the terminal reports
  focus lost (`EnableFocusChange`). Disk and worktree scans run only when their
  view is open or their cache is stale.

### Kept ready for the menu bar app
The app must be a second skin, not a second product. v1 holds to these rules so
M7 adds drawing code only:
- **The `ViewModel` is plain data.** Rows, badges, sentences and the attention
  count are computed in `scry-core` and carry no terminal types. The TUI draws
  them now. The app draws the same ones later.
- **Every view model is available as JSON.** `scry overview --json` gives one
  reading. `scry watch --json` streams them as lines. The stream is the app's
  first data source.
- **Every action is a command.** `stop`, `reap` and cleanup return JSON results and
  run the same checks whoever calls them.
- **The TUI can be opened at a place.** `scry tui --view agents --select <id>` lets
  a panel row hand over to the terminal.

### Layout spike (M0, half a day)
Render three static layouts from one fixture and pick one:
- **A.** Tabs with list and detail, as drawn above.
- **B.** One dashboard of panels, like `btop`.
- **C.** A single attention feed with drill-down.

The default is A with C's attention feed as its Overview. The spike exists to
confirm or overturn that before five views are built on it.

## 3. Architecture

```
  agent hooks ──▶ spool (one JSON line per event)
                      │
        ┌─────────────▼──────────────┐
        │ scry daemon  (LaunchAgent) │  records only:
        │ statfs samples             │  disk history, session events,
        │ agent-tree ancestry        │  ancestry seen before a parent died
        │ spool ingest               │
        └─────────────┬──────────────┘
                      ▼
              SQLite (WAL) store ◀── audit rows, keep list
                      ▲
          ┌───────────┴───────────┐
      scry (TUI)          scry <command>
  engine in-process, live scan + store reads
```

- **One engine, two hosts.** `scry-core` exposes `Engine::snapshot()`. The TUI
  and the CLI each call it in-process. They agree because they run
  the same code over the same store.
- **The daemon records what a live scan cannot recover.** That is disk history,
  session events from hooks, and the ancestry of agent process trees observed
  before a parent died. It watches known agent pids with kqueue `NOTE_EXIT` and
  `NOTE_FORK` and rescans that tree on each event.
- **Everything works without the daemon,** minus history. Env stamps and a live
  scan still attribute most agent children. `scry doctor` says what is missing.
- **One writer per table.** The daemon alone writes samples, sessions and ancestry.
  Clients append audit rows and edit the keep list. Without the daemon, clients
  read the spool in memory and do not ingest it.
- **Version safety is the schema version.** One binary serves every role, and a
  client refuses a store with a newer `user_version`.
- **No network code, no telemetry.**

### Why no socket yet
The first plan had the daemon own a live index behind JSON-RPC. That design earns
its cost when a client cannot scan for itself or needs events pushed to it. The
TUI and the CLI can both scan. A proxy measurement on this Mac puts a full
listing of 876 processes at about 76 ms through `ps` and the socket table at about
15 ms through `netstat`. `scry bench scan` in M0 replaces that proxy with the real
number. If an in-process scan costs more than the TUI budget allows, the socket
moves forward.

The socket arrives when a client needs it. The menu bar app starts on the
`scry watch --json` stream, and M7's spike decides whether that is enough. Cowl
and Sigil will need the socket either way. `Engine` is the seam: a `RemoteEngine`
behind the same interface is the only change clients see.

## 4. Repository layout

```
scry/
  crates/
    scry-core/      # modules: sys (FFI), model, engine, attribution, store, catalog
    scry/           # the one binary: tui, CLI commands, hook, daemon
  fixtures/         # redacted snapshots (synthetic ones in git, recorded ones ignored)
  PLAN.md
  ASSESSMENT.md
```

A crate is split out when a second consumer exists. `shell-mac` and a Swift or
Rune app target appear in M7.

Paths:
- Config: `~/.config/scry/config.toml` (code roots, `destructive = false`, keep list).
- State: `~/Library/Application Support/Scry/` (`scry.db`, `spool/`, `daemon.lock`).
- LaunchAgent: `~/Library/LaunchAgents/<bundle-id>.daemon.plist`, pointing at the
  stable `$(brew --prefix)/bin/scry` symlink, not a Cellar path.

### Crates and APIs
| Need | Crate / API |
|---|---|
| Processes, cwd, fds | `libproc`, raw `proc_pidinfo` / `proc_pidfdinfo` |
| argv and env stamps | `sysctl KERN_PROCARGS2`, read once per `(pid, start_time)` |
| Socket table, all users | `sysctl net.inet.{tcp,udp}.pcblist_n`, joined with `libproc` for own processes |
| CPU, memory pressure, swap | `host_statistics64`, `kern.memorystatus_vm_pressure_level`, `vm.swapusage` |
| Process exit and fork | kqueue `EVFILT_PROC` with `NOTE_EXIT`, `NOTE_FORK` |
| Sizes | `getattrlistbulk` with `ATTR_FILE_ALLOCSIZE` and `ATTR_CMNEXT_PRIVATESIZE` |
| Git | The `git` CLI (`worktree list --porcelain`, `status --porcelain`) |
| File change hints | `notify` (FSEvents) |
| Store | `rusqlite` (bundled, WAL) |
| CLI | `clap`, `--json` on every command |

## 5. Attribution model

Entities:
- `Process{pid, start_time, ppid, uid, exe, argv, cwd, footprint, tty}`
- `Socket{proto, port, addr, pid, uid}`
- `Project{root, kind}`
- `Worktree{path, repo, branch, state}`
- `Session{agent, session_id, cwd, started_at, ended_at, end_reason, source}`
- `SessionProcess{session, pid, start_time, role, first_seen, last_seen, evidence}`

**Identity is `(pid, start_time)`.** Every stored reference and every signal checks
both, so pid reuse cannot redirect a kill.

**One agent process hosts many sessions.** `/clear`, `resume`, `compact` and `fork`
start a new session in the same process. `SessionProcess` carries time ranges for
that reason.

### Which session owns a process
1. **Env stamp (`stamp`, exact).** The process environment carries the agent's
   session id. Only allowlisted keys are kept: `CLAUDE_CODE_SESSION_ID`,
   `CLAUDE_PID`, `CODEX_SESSION_ID`, and `SCRY_SESSION`, which Scry's own
   SessionStart hook writes through `CLAUDE_ENV_FILE`. The rest of the environment
   is dropped in memory and never stored.
2. **Ancestry (`tree`, exact while observed).** A live ppid walk reaches a known
   agent process, or the daemon recorded that ancestry before the parent died.
3. **Time and place (`guess`, inferred).** The process started inside a session's
   time window in that session's directory. Shown, never acted on by rule.
4. **Project only.** Walk cwd up to `.git`, `package.json`, `Cargo.toml`, `go.mod`
   or `pyproject.toml`. A `.git` file means a worktree or a submodule. Ask git
   which.

### Leftover rule
A process is a leftover only if all of these hold:
- It is attributed to a session by `stamp` or `tree`.
- That session's agent process `(pid, start_time)` is gone.
- It is not on the never-reap list: `claude daemon`, `--bg-pty-host`,
  `codex app-server`, helpers inside `*.app/Contents/`, and `tmux`, `screen` and
  `zellij` servers.
- It has no live controlling tty, and no descendant that has one.
- It is older than a grace period.
- It is not on the user's keep list.

A process with ppid 1 and no attribution is *unattributed*. `reap` never selects it.

### Hooks
- `scry hook install claude` shows the diff, backs up `settings.json` and checks for
  managed settings that would override it.
- Hooks are exec-form, print nothing to stdout, always exit 0 and finish in under
  50 ms. SessionStart stdout would land in the model's context, and SessionEnd has
  a 1.5 s budget.
- A hook appends one JSON line to the spool. Ingest is idempotent by event id.
- The leftover check runs when the agent process exits, never on `/clear` or
  `resume`. Sessions are also reconciled on every scan, because SessionEnd does
  not fire on SIGKILL.
- Codex and Gemini hooks share one normalized `SessionEvent`. They ship once their
  payload fields are checked. Only the existence of `~/.codex/hooks.json` is
  verified so far.
- Scry never installs a `WorktreeCreate` hook. It would replace git's own worktree
  creation.

## 6. Milestones

About 15 working days to v1, then about two weeks for the menu bar item and app. Week one ends with a TUI showing ports and agents,
read-only `--json` output, and nothing yet able to kill or trash by rule.

### M0. Skeleton and first pixels (1.5 days)
- `scry-core` and the `scry` binary. `fmt`, `clippy`, `nextest`, CI on macOS arm64.
- Process scan, the `Snapshot` type, `scry snapshot -o <file>` (redacted by default)
  and the fixture loader.
- TUI shell: header, tabs, footer, help overlay, resize, clean exit, `--fixture`.
- The layout spike.
- `scry daemon install|uninstall|status|run`: user LaunchAgent, single-instance
  lock, `statfs` sampler writing from day one.
- Config file, `scry doctor`, `scry bench scan`.
- **Exit:**
  - `scry tui --fixture fixtures/typical.json` renders the chosen layout at 80×24
    and 120×40, and the snapshot tests pass.
  - A PTY test proves the terminal is restored after quit, panic and SIGTERM.
  - `bench scan` prints the full-scan time on the reference machine (this Mac,
    about 880 processes).
  - `daemon status` shows disk samples accumulating.
- **Status, 11 Nov 2026.** Done: the workspace, the process scan, `Snapshot`,
  `scry snapshot`, the fixture loader, the TUI shell, `overview`, `bench scan`,
  snapshot tests, the PTY smoke test (`scripts/pty-smoke.sh`) and the CI file.
  Left: the layout spike, the daemon, the config file, `doctor`, and a panic case
  in the PTY test.
- **Measured on the reference machine** (M-series, 32 GB, about 1,070 processes):
  - Full scan: median 4.6 ms over 30 release runs. This settles section 3's
    question for processes. The socket table is not in the scan yet.
  - Live TUI, idle and focused: 0.20 s of CPU over 60 s (0.3% of one core),
    6 MB resident. The 10-minute check comes with M1.
- **Found while building.**
  - `proc_pidinfo` with `PROC_PIDTBSDINFO` fails on 202 of 1,049 processes
    without root. `PROC_PIDT_SHORTBSDINFO` reads all of them but has no start
    time, so start times come from `sysctl kern.proc.all`, read by fixed offsets
    into the 648-byte arm64 `kinfo_proc`. A layout change fails loudly, and a
    test checks the result against `ps`.
  - The shell resets tty modes when a job is killed or stopped, so the PTY test
    also checks that the TUI has left the alternate screen. A SIGKILL control
    run fails that check, as it should.

### M1. Ports (2 days)
- Socket table from `pcblist_n`, joined with `libproc`. Env-stamp reader. Project
  roots.
- Ports view with detail pane, filter and sort. The pulse header.
- Each pulse metric carries an availability state. A first reading says
  "warming up" instead of showing a zero.
- `scry ports`, `scry why <port>`, `scry stop <pid|:port>`, and `x` in the TUI.
- **Exit:**
  - A CI contract test shows `scry ports --json` lists every LISTEN socket that
    `netstat -anv` shows.
  - Stopping a fixture server works from the TUI and the CLI, and a pid-reuse
    fixture is refused.
  - CPU of the focused, idle TUI is measured over 10 minutes with `top`. The
    target is under 1% of one core. The number is recorded here.

### M2. Agents, read-only (2 to 3 days)
- Agent detection by path with one fixture per install style. The never-reap list.
- `Session` and `SessionProcess`. Claude hooks. Daemon ancestry recording.
- Agents view and the Overview attention list.
- `scry agents --tree`, `scry leftovers`, `scry overview`, `scry watch --json`.
- **Exit:**
  - A fixture agent spawns a child that ignores SIGHUP, then exits. The child
    appears as a leftover with `stamp` evidence.
  - On this Mac, `scry leftovers` selects none of the Claude daemons, the
    background pty hosts, the Codex app-server or the tmux server.
  - Three days of daily use before M4 starts. False leftovers are logged as
    fixtures.

### M3. Dropped
The MCP server is out of scope. Agents call the CLI and read its `--json`
output.

### M4. Reap (1 day)
- `X` and `K` in the TUI. `scry reap --dry-run` and `scry reap`.
- **Exit:**
  - Reaping the M2 fixture kills the child and nothing else, children first.
  - With `destructive = false`, every write path refuses.

### M5. Worktrees and disk (4 days)
- Worktrees first. Discover through each repo's `.git/worktrees/*/gitdir` and known
  agent directories such as `~/.codex/worktrees`.
- Creator by path convention, then by stamped processes whose cwd is inside, then
  by time window labelled `guess`, else "unknown".
- Disk: one `getattrlistbulk` pass reports allocated and reclaimable sizes. Rank
  by reclaimable. Hard links count once per `(dev, ino)`.
- Each catalog kind declares its cleanup: `trash`, `delete` (regenerable caches,
  typed confirmation), `command` (`docker system prune`,
  `xcrun simctl delete unavailable`) or `never`. `Docker.raw` is `never`.
- After a trash action the view says "moved N GB to Trash" and shows the Trash's
  size, because space returns only when the Trash is emptied.
- Free space is the figure Finder calls "available". Snapshots and purgeable space
  are listed apart.
- The forecast appears after 48 h of samples and uses a robust slope.
- FSEvents marks subtrees stale. Sizes always come from a rescan.
- Scans stay inside the catalog and the code roots. They never walk Desktop,
  Documents or Downloads, which would raise a permission prompt in the name of the
  terminal app.
- **Exit:** both views finish under 2 s warm and under 30 s cold on the reference
  machine with its real `~/sources`. A pnpm fixture shows reclaimable below
  allocated.

### M6. Polish and ship v1 (2 to 3 days)
- Empty, loading and error states for every view. The narrow layout.
- A check in light and dark themes in Terminal, iTerm2 and Ghostty.
- README with recorded screens rendered from fixtures.
- Homebrew formula in a tap, and `cargo install`.
- **Exit:** a clean machine goes from `brew install` to a useful Overview with no
  step that needs root. The daemon stays under 0.5% CPU and 40 MB over an idle hour.

### M7. Menu bar item, v1.1 (about 1 week, a guess until the spike)
- **Toolkit: Rune.** Checked against its repo on 9 Nov 2026, which is far ahead
  of the source doc's picture. It has an accessory (no Dock icon) mode,
  borderless always-on-top windows placed by position, List, Popover, Menu,
  TextInput and Toast components, `rune-notify` for notifications, and headless
  PNG and element-tree output for agent checks. Cowl and Sigil are planned on it
  too. SwiftUI `MenuBarExtra` is the fallback.
- **What Rune lacks, and Scry supplies.**
  - No macOS status item. `rune::Tray` supports Linux and Windows only (issue
    141). Scry owns an `NSStatusItem` through `objc2` and places the Rune window
    under the item's button. This is the `shell-mac` crate.
  - No live icon or title updates in its tray API (issue 305). Scry's own status
    item covers it.
  - Launcher panels take about 280 ms to appear (issue 335, reported upstream).
  - No releases. Pin a `dev` commit and budget time for upgrades.
  - No table, tree or sparkline in rune-kit. Scry builds its rows from elements.
- **Spike first (2 days).** Settled by a working sketch:
  - A status item opens a Rune panel under it, drawing the Overview from a
    fixture, and renders the same panel headlessly to PNG.
  - Measured on this Mac: time from click to first frame, and idle CPU with the
    panel closed. A click-to-frame time well past 200 ms, or an idle cost above
    the M6 daemon budget, sends M7 to the SwiftUI fallback.
  - Data path: the bundled `scry` binary run as a child with `watch --json`,
    against a socket on the daemon. The default is the child stream.
- The icon shows the attention count and changes glyph when something needs
  attention.
- The panel shows the pulse strip and the Overview list. A row expands to its
  sentence and evidence.
- Row actions: stop with a native confirmation, and "Open in Scry", which opens
  the TUI at that row in the user's terminal.
- Notifications for a new leftover, a disk threshold and a watched port.
- App bundle, Developer ID signing, notarization, a Homebrew cask. `SMAppService`
  takes over the LaunchAgent. No sandbox and no Mac App Store.
- **Exit:**
  - The icon count equals `scry overview --json` in a contract test.
  - The app's idle CPU and power are measured over 10 minutes with
    `top -stats cpu,power`, with the panel closed and open. The numbers are
    recorded here.
  - Quitting the app leaves no `scry watch` child behind.

### M8. The full app, v1.2 (about 1 week, a guess)
- Ports, Agents, Disk and Worktrees join the panel as tabs, with filter and the
  same actions as the TUI.
- The panel can detach into a standalone window with a Dock icon. This is opt-in,
  and the menu bar item stays the default.
- Screens are checked by snapshot from the same fixtures the TUI uses.
- **Exit:** every sentence and count in the app matches the TUI for the same
  fixture, because both draw one `ViewModel`.

## 7. Safety and privacy contract

**Destructive commands**
- `scry stop`, `scry reap` and cleanup act only after a confirmation, or `--yes`
  from a script or an agent. `destructive = false` in the config disables all of
  them, and every accepted action is audited.

**Process control**
- Current user's processes only.
- The human-driven TUI and CLI may stop any of them after confirmation. Rules
  (`reap`) act only on leftovers.

**Privacy**
- Environment values outside the allowlist are never kept.
- Command lines appear only as allowlisted labels; raw argv prints only with
  `scry why --show-command`, never in JSON or snapshots.
- Recorded snapshots are redacted by default and stay out of git.
- The store and spool are mode 0600 in a 0700 directory.

## 8. Testing
- **View models.** Pure functions from fixture snapshots to rows and sentences.
- **Screens.** `TestBackend` snapshots per view at 80×24 and 120×40, each in empty,
  typical and overloaded states.
- **PTY smoke test.** Spawn the TUI in a pty, send keys, assert the screen and a
  restored terminal. Agents can check the same thing by hand through
  `tmux capture-pane`.
- **Attribution.** Leftover and never-reap rules run against fixtures, including
  one per agent version for the undocumented env keys.
- **Live integration (macOS CI).** A fixture HTTP server for ports. A fixture agent
  tree for leftovers. The `netstat` contract test.
- **Budgets.** `bench scan` and the idle CPU measurements are recorded per
  milestone.

## 9. Risks
| Risk | Mitigation |
|---|---|
| Reaping the wrong thing | Leftover rule, never-reap list, keep list, tty check, grace period, identity re-check, sheet before every action, three days of read-only use first. |
| Env stamp keys are undocumented and may change | Fixture per agent version. `tree` evidence as the second signal. `doctor` reports when no stamps are found under a live agent. |
| A stamped multiplexer server spreads a dead session's stamp | Multiplexers on the never-reap list. The tty-descendant check. Not reproduced here; it follows from env inheritance. |
| `pcblist_n` layout changes between macOS releases | Contract test against `netstat -anv`. `doctor` check. Fallback to `libproc` for the user's own sockets. |
| In-process scans cost too much | `bench scan` in M0. Focus-aware sampling. Argv and env read once per process. If the budget fails, the socket moves forward. |
| Rune changes under a pinned commit | It is off the v1 critical path. M7 pins one `dev` commit, and SwiftUI stays the fallback. |
| Terminals differ | 16 ANSI colours, glyph plus word for state, tested in three terminals, minimum-size guard. |
| The LaunchAgent breaks on upgrade | Plist points at the stable brew symlink. `doctor` verifies the path. macOS shows a "Background Items Added" notice at install, and the README says so. |
| Distribution without Developer ID | A Homebrew formula is not a cask, so I expect no quarantine. Verify on a clean machine in M6 before promising it. |
| Name clash with "Scry: Remote Desktop" | Distinct formula and bundle id. `Familiar` stays as the fallback name. |

## 10. Decisions
Defaults are in bold. The first is yours from this revision.

1. First face: **TUI.** The menu bar item follows as v1.1 and the full app as v1.2.
2. Bare `scry` **opens the TUI** on a TTY and prints a summary otherwise.
3. Name: **Scry** (fallback Familiar).
4. License: **MIT.**
5. Repo: **one repo per app.**
6. Minimum OS: **macOS 14+, Apple Silicon only.**
7. Code roots: **`~/sources`, `~/code`, `~/src`, `~/Developer`**, configurable.
8. Agents in v1: **Claude Code with hooks. Codex, OpenCode and Gemini CLI by
   stamp and tree,** with their hooks once payloads are checked.
9. v1 distribution: **Homebrew formula and `cargo install`.** Developer ID arrives
   with the app.

## 11. Where each assessment finding landed
| # | Finding | Section |
|---|---|---|
| 1 | Orphan rule replaced by the leftover rule | 5, M2, M4 |
| 2 | Write actions confirmed and audited (MCP dropped) | 7, M4 |
| 3 | Daemon installed from M0, one binary | 3, M0. The protocol handshake becomes a schema-version check until the socket exists. |
| 4 | Env stamps first, allowlisted | 5, M1 |
| 5 | Silent hooks, `SessionProcess` | 5, M2 |
| 6 | `pcblist_n`, bind-checked `find_free_port` | 4, M1, M3 |
| 7 | Reclaimable sizes, per-kind cleanup, Trash | M5 |
| 8 | Worktree discovery and creator | M5 |
| 9 | Tools ship with their collectors | M3, M5 |
| 10 | Measurable exit criteria | Every milestone |
| 11 | Privacy contract | 7. Peer-uid and frame limits return with the socket. |
| 12 | Trust wording | 3, M7 |
| 13 | Pulse trimmed to CPU, memory pressure, swap | 1, M1 |
| 14 | Read-tool caps, MCP exit on EOF | Dropped with MCP |
| 15 | Fewer crates | 4 |
