# How Scry works

This page is for people who want to read or change Scry's code. It covers the two crates,
how one reading of the machine is made, what the background recorder adds, how a process
is tied to an agent session, how a leftover is decided, and how stopping and cleaning up
stay safe. The [README](../README.md) covers using Scry; [design.md](design.md) covers how
the terminal UI looks and why.

## The big picture

```
  agent hooks ──▶ spool (one JSON line per event)
                      │
        ┌─────────────▼──────────────┐
        │ scry daemon  (LaunchAgent) │  records only:
        │ disk space samples         │  disk history, session events,
        │ agent-tree ancestry        │  ancestry seen before a parent died
        │ spool ingest               │
        └─────────────┬──────────────┘
                      ▼
              SQLite (WAL) store ◀── audit rows, size cache
                      ▲
          ┌───────────┴───────────┐
      scry (TUI)            scry <command>
     scan in-process        scan in-process
```

- **One engine, two hosts.** The terminal UI and every CLI command scan the machine
  in-process with the same code, and read the same store. They agree because nothing else
  decides what they show.
- **The daemon records; it does not serve.** It keeps what a live scan cannot recover:
  free-space history for the disk forecast, session start and end events from hooks, and
  which session each process belonged to while its agent was alive. There is no socket and
  no protocol between the daemon and its clients.
- **Everything works without the daemon,** minus that history. Environment stamps and the
  live process tree still tie most processes to their session. `scry doctor` says what is
  missing.
- **No network code and no telemetry.** Scry reads the kernel's socket table; it never
  opens a connection.

## Crates

| Crate | What it holds |
|---|---|
| `crates/scry-core` | Reading the system, the data model, attribution, the store, the daemon, cleanup and the view models. It has no terminal or UI types, so a future menu bar app can draw the same view models. |
| `crates/scry` | The `scry` binary: argument parsing, text and JSON output for each command, and the terminal UI (`src/tui`). |

Inside `scry-core`:

| Module | Job |
|---|---|
| `sys` | Raw macOS calls that need no root: the process table (`sysctl kern.proc.all`, `proc_pidinfo`), arguments and environment stamps of your own processes (`KERN_PROCARGS2`), the socket table for all users (`net.inet.{tcp,udp}.pcblist_n`), kqueue process watches, and the pulse (CPU, memory pressure, swap). |
| `engine`, `model` | One `Snapshot`: processes, sockets, command labels, stamps, project roots and the pulse, as plain serializable data. |
| `label` | Command labels, such as `next dev --port 3000`, built only from parts known to be safe. |
| `catalog` | Which executables are agents, and which processes Scry never reaps. |
| `attribution`, `session` | `Lineage`: every session with its processes, and one verdict per process. |
| `live` | One reading for a client: a snapshot, plus what the store and the spool know, turned into a `Lineage`. |
| `view` | The view models every skin draws: rows, state words, the sentence that explains each row, and its evidence. |
| `control`, `reap`, `keep` | Stopping a process, reaping leftovers, and editing the keep list. |
| `worktree`, `git` | Finding linked git worktrees, labelling each, and guessing which agent made it. Runs the `git` CLI and never writes to a repository. |
| `disk` | The disk catalog, sizes and their cache, free space, the forecast, the Trash, and cleanup. |
| `store`, `spool`, `recorder`, `daemon`, `launch_agent` | The SQLite store, the hook spool, the daemon's recorder, and installing the LaunchAgent. |
| `config`, `paths`, `home`, `protected` | The config file, where state lives, hiding your home and account name, and the folders Scry never enters. |
| `claude_hooks` | Installing and removing Scry's Claude Code hooks. |
| `doctor` | The checks behind `scry doctor`. |

## One reading

1. `Engine::snapshot()` reads the process table and the socket table. For your own
   processes it also reads arguments, the working directory and a few environment keys
   (below). Arguments are turned into a command label right away and the raw arguments are
   dropped; every other environment value is dropped in memory.
2. `live::Collector` adds history: sessions and recorded ancestry from the store, and hook
   events from the spool that the daemon has not taken yet. Without a store it reads the
   spool in memory and writes nothing.
3. `attribution::Lineage::build` decides which session owns each process and whether it is
   a leftover. Views, the CLI and the daemon all read these verdicts; none decides on its
   own.
4. `view` turns the lineage into view models. The TUI draws them; `--json` prints them.

A full scan of about a thousand processes takes a few milliseconds in a release build;
`scry bench scan` measures it on your Mac.

The TUI is a model, update, view loop on plain threads: a collector thread sends readings,
`tui/app.rs` handles keys and returns commands that run off the UI thread, and a frame is
drawn only when something changed.

## Identity

A process is identified by `(pid, start time)`, never by pid alone. Every stored reference
and every signal checks both, so a pid that macOS hands to a new process cannot redirect an
action.

## Which session owns a process

Scry recognises Claude Code, Codex, OpenCode and Gemini CLI by their executable path, in
each install style it has a fixture for (`fixtures/agents/install-styles.json`). One agent
process can host several sessions over time (`/clear`, `resume`), so a session does not own
a process outright: a link between them carries a time range and its evidence.

Evidence, strongest first:

1. **`stamp` (exact).** The process environment carries the agent's session id:
   `CLAUDE_CODE_SESSION_ID`, `CODEX_SESSION_ID`, or `SCRY_SESSION`, which Scry's own
   SessionStart hook sets. `CLAUDE_PID` is read too. These are the only environment keys
   Scry keeps. A stamp counts only if the session had not ended before the process started.
2. **`tree` (exact while observed).** A live parent walk reaches an agent process, or the
   daemon recorded that ancestry while the agent was alive.
3. **`guess` (inferred).** The process started in the session's directory while the session
   ran. A guess is shown, never acted on by a rule.

A process with no session evidence is still tied to a project, by walking its working
directory up to a `.git`, `package.json`, `Cargo.toml`, `go.mod` or `pyproject.toml`.

## Leftovers

A process is a leftover only if all of these hold:

- a stamp or the process tree ties it to a session;
- every session that claims it has lost its agent process (an agent process that still
  runs counts as running, even one Scry does not recognise);
- neither it nor anything it runs under is on the never-reap list in `catalog.rs`
  (agent daemons and pty hosts, terminal multiplexers, `gpg-agent`, `ssh-agent`, ssh
  control masters, and helpers inside app bundles, among others);
- neither it nor any process below it has a terminal;
- it is older than the grace period (`leftover_grace_secs`, 300 s by default);
- it is not on your keep list.

A process re-parented to launchd that nothing ties to a session is never a leftover. While
the config cannot be read, nothing is a leftover. Scry excludes its own process tree.

Scry cannot tell abandoned work from a job you meant to outlive its session, such as one
started with `nohup`. Both look the same once the agent has exited, which is what the keep
list is for.

## The background recorder

`scry daemon run`, kept alive by a user LaunchAgent, does three things:

- samples available space on `/` every `sample_interval_secs` (600 s by default) for the
  disk forecast;
- takes hook events from the spool into the store, once each, by event id;
- watches known agent processes with kqueue (`NOTE_FORK`, `NOTE_EXIT`) and records which
  session each of their processes belongs to, so a child re-parented to launchd after its
  agent exits keeps its `tree` evidence.

One instance runs per state directory, guarded by a lock. Its log, `daemon.log`, rotates at
1 MB.

## The store and the spool

The store is one SQLite file in WAL mode, `scry.db`, in a state directory only you can
open (mode 0700).
Each table has one writer: the daemon writes disk samples, sessions, recorded ancestry and
taken spool events; clients append audit rows, cache folder sizes and remember what they
moved to the Trash. `PRAGMA user_version` is the schema version, and a client refuses a
store newer than it knows before it changes anything in it.

Hooks append one JSON line per event to `spool/events.jsonl` under `flock`. The daemon
renames the file under the same lock before reading it, so nothing writes to a file it is
reading.

## Stopping a process safely

`scry stop`, `scry reap`, `x` and `X` in the TUI all go through `control.rs`. Right before
each signal, SIGKILL included:

1. a kqueue watch is put on the pid, for its exit and for an exec;
2. a target named by `:port` must still hold that port;
3. the machine and the config are read again: stopping must still be allowed, the process
   must still be yours and not a system process, and for a reap still a leftover of the
   session it was listed under;
4. an audit row saying what is about to be sent is written and synced, and nothing is sent
   when it cannot be written;
5. `destructive` and the keep list are read once more;
6. the identity is read again, and the watch is asked whether the process exited or exec'd
   at any point since step 1;
7. the signal.

What remains is the few microseconds between the last question to the watch and `kill(2)`.
macOS has no way to signal a process by anything but its pid, so that window cannot be
closed; it would take the pid counter wrapping all the way round inside it.

SIGTERM comes first, with up to 3 s to exit. SIGKILL follows only with `--kill`, or after a
second confirmation in the TUI, with the same checks. A reap goes children before parents.

System processes are never targets: pids below 100, launchd, and anything that runs from
`/System/`, `/usr/libexec/`, `/usr/sbin/` or `/sbin/`.

## Cleaning up disk safely

The disk catalog in `disk/catalog.rs` is a reviewed table of places developer tools fill.
Nothing outside it and the code roots is sized, and Desktop, Documents and Downloads are
never entered. Each entry allows one cleanup:

- **trash**: moved to the Trash through `NSFileManager`, so Put Back in Finder works;
- **delete**: removed for good, only for caches a tool fills again, after you type the
  item's name;
- **command**: the tool's own cleanup, shown for you to run; Scry runs nothing;
- **never**: shown for its size only.

A worktree can be moved to the Trash once it is clean and its branch is merged or pushed.
`prune` removes the git record of one worktree whose folder is gone, never
`git worktree prune`, which would drop the record of every missing worktree.

Right before acting, `disk/clean.rs` reads `destructive` again, walks the folder for
repositories, worktrees and mounted volumes inside it, reads a worktree's label again, and
opens the folder from `/` one component at a time without following a symlink, checking
each folder's device and inode against the plan it showed you. It writes an intent row to
the audit log, refusing when it cannot, then acts and writes what happened. A delete works
through the folder's descriptor and never crosses into another volume. The home folder,
Desktop, Documents, Downloads, Scry's own config and state folders and every folder above
one of them are out of reach whatever the plan says.

Moving to the Trash is the one step macOS takes by path. If something else takes that path
between Scry's last check and the move, Scry compares what landed in the Trash with the
folder it checked and moves anything else back, never over what stands there now.

## Tests

- View models and attribution are pure functions over fixtures in `fixtures/`, tested
  directly.
- Screens are rendered with ratatui's `TestBackend` and compared with `insta` snapshots in
  `crates/scry/src/tui/snapshots/`.
- `crates/scry/tests/` runs the binary end to end: ports against `netstat`, stop and reap
  against real fixture processes, cleanup against throwaway folders, hooks against a
  throwaway Claude config, and the daemon under a throwaway `SCRY_HOME`.
- `scripts/pty-smoke.sh` runs the TUI in a real terminal through `tmux` and checks that the
  terminal is restored after quit, SIGTERM, Ctrl-Z and a closed terminal.
