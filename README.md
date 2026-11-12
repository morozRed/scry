# Scry

Scry shows which agent session left each process, port, worktree and gigabyte on
your Mac.

Coding agents such as Claude Code and Codex start dev servers, MCP servers and
headless browsers, create worktrees and fill caches. When a session ends, much of
that stays behind. Scry traces each of these back to the project and the agent
session that made it, so you can see what is safe to stop or delete.

> **Status:** early development. The process scan, the terminal UI shell and the
> `ports`, `why`, `stop`, `agents`, `leftovers`, `worktrees`, `disk` and `clean`
> commands work. See the [roadmap](PLAN.md).

## Screenshots

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screens/agents-dark-120x40.svg">
  <img alt="The Agents view: agent sessions as a tree, with leftover processes flagged and the selected one explained" src="docs/screens/agents-light-120x40.svg">
</picture>

The Agents view groups processes under the agent session that started them and
flags what a finished session left behind. The pane below the list explains the
selected row in one sentence, with its evidence.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screens/ports-dark-120x40.svg">
  <img alt="The Ports view: every listening port with its process, project, session, age and state" src="docs/screens/ports-light-120x40.svg">
</picture>

The Ports view lists every listening port on the Mac, yours and the system's.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screens/sheet-long-dark.svg">
  <img alt="The reap confirmation sheet listing each process it would stop and the memory it frees" src="docs/screens/sheet-long-light.svg">
</picture>

Nothing is stopped on one key. Every action opens a sheet that lists each target
and what it frees.

Scry follows your terminal's light or dark theme. These screens are drawn from a
synthetic fixture with `cargo run -p scry --example shots`, so every view shows sample
data. The design notes are
in [docs/design.md](docs/design.md).

## Requirements

- macOS 14 or later on Apple Silicon
- Rust 1.90 or later to build

Scry needs no root access.

## Build and run

```sh
cargo install --path crates/scry
scry                 # open the terminal UI
scry overview        # print a summary and exit
scry overview --json
```

Run without a terminal, as an agent does, bare `scry` prints the summary instead of
opening the UI.

## Ports

```sh
scry ports                     # every listening port: process, owner, project, age
scry ports --json
scry why 3000                  # who holds :3000, in one sentence, with the evidence
scry why :3000 --json
scry why 3000 --show-command   # also print the full command line
scry stop :3000                # stop what holds :3000, after you confirm
scry stop 4821 --yes --kill    # by pid, no prompt, SIGKILL if SIGTERM is not enough
scry stop :3000 --json         # the outcome, or why nothing was stopped, as JSON
```

Scry names a process by a command label such as `next dev --port 3000` or
`docker exec … mysql …`. A label shows only:

- the program, or for an interpreter the script it runs from a `bin` directory, so
  `node …/.bin/next dev` reads `next dev`;
- subcommand words from a fixed list, such as `run`, `dev` and `serve`, unless they
  follow a flag;
- flag names, never their values, and `--`;
- the last part of an argument that names an existing file or directory on a local
  volume, with your home directory as `~`;
- the name of a program on your `PATH`;
- a number equal to a port the process itself holds open as a listener;
- for Python, the module after `-m`.

Anything else becomes `…`, whatever it holds, so passwords and tokens given as
values stay out of tables, `--json` and `scry snapshot`. A file, script or program
named like your account is left out too. A Python multiprocessing worker, such as
the server `uvicorn --reload` starts, is shown by the label of the process it works
for, marked as a worker. `scry snapshot` writes your home directory as `~` in paths
and replaces your account name anywhere else in them. `--show-command` prints the
full command line as text on request; it is never part of `--json`.

`scry stop` acts only on your own processes, never on system ones. Right before
each signal it checks that the pid still belongs to the process it chose. It exits
0 when the process stopped, 1 when nothing was signalled and 2 when the process is
still running. Every command exits 64 when its arguments are wrong.

## Agents

```sh
scry agents                    # agent sessions running now, with project, RAM and ports
scry agents --tree --json      # each session with the processes it started
scry leftovers                 # what ended sessions left running, with the evidence
scry leftovers --json
scry watch --json              # one JSON line now and on every change, for programs
scry hook install claude       # exact Claude Code sessions; shows the diff, backs up
scry hook uninstall claude
```

Scry recognises Claude Code, Codex, OpenCode and Gemini CLI by their executable
path, in every install style it has seen (native installer, npm, the desktop apps,
Homebrew). A process belongs to a session by its environment stamp
(`CLAUDE_CODE_SESSION_ID`, `CODEX_SESSION_ID`, or `SCRY_SESSION` from Scry's own
hook), by a parent chain up to the agent, or, shown but never acted on, by having
started in the session's directory while it ran.

A process is a **leftover** only if all of these hold: a stamp or the process tree
ties it to a session, and a stamp counts only if the session had not ended before
the process started; every session that claims it has lost its agent process, and
an agent process that still runs counts as running even when Scry does not
recognise its executable; it is not on the never-reap list (`claude daemon`, Claude
Code's background pty hosts, `codex app-server`, `tmux`, `screen` and `zellij`,
`gpg-agent`, `keyboxd`, `scdaemon`, `dirmngr` and `ssh-agent`, ssh control masters,
and anything whose executable is inside an `*.app/Contents/` bundle except a
framework Python's interpreter, a file named `Python…` under `Python.framework/`)
and runs under nothing on that list; neither it nor anything below it has a
terminal; it is older than five minutes; and it is not on your keep list. A process
re-parented to launchd that nothing ties to a session is never a leftover. While
the config cannot be read, nothing is a leftover, and `scry leftovers` and `scry
doctor` say why. Nothing is stopped by this release; the list is for reading.

Scry cannot tell abandoned work from a job you meant to outlive its session, such
as one started with `nohup` or set up as a launchd job: both look the same once the
agent has exited. Every leftover says so. Add such a job to `keep` in the config
and no rule selects it.

`scry hook install claude` adds exec-form SessionStart and SessionEnd hooks to
`~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR`), writing through a symlinked
file to its target. It prints the diff, backs the file up beside it, keeps every
other hook and the file's indentation, and gives up without writing when the file
changes while it works. It refuses when Claude Code's managed settings (MDM
preferences for `com.anthropic.claudecode`, then `managed-settings.json` and its
drop-ins, merged as Claude Code merges them) would stop user hooks from running,
and when `claude --version` is older than 2.1.139, which runs a hook's `command`
without its `args`. The hook prints nothing, always exits 0 and appends one line to
Scry's spool. It never installs a `WorktreeCreate` hook.

## Disk

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screens/disk-dark-120x40.svg">
  <img alt="The Disk view: free space and its forecast, then caches, node_modules and worktrees ranked by what deleting them frees" src="docs/screens/disk-light-120x40.svg">
</picture>

```sh
scry disk                       # free space, forecast, and the 15 largest items
scry disk --top 40 --json
scry clean npm-cache            # delete a cache after you type its name
scry clean worktree:~/sources/acme-wt-old --yes
```

Scry sizes a reviewed list of places developer tools fill: Xcode, simulators,
Docker's disk, package caches, agent session logs, app caches, every `node_modules`
under your code roots, and linked worktrees. It never looks inside Desktop,
Documents or Downloads. Each item shows the space it holds and at least what
deleting it frees at once, which is less when its files are clones or hard links of
others, as pnpm and uv make them. Sizes are in decimal units, as Finder shows them
(1 GB is 1,000,000,000 bytes). Free space is what Finder calls available; purgeable
space and local snapshots are listed apart. A forecast of when that available space
runs out appears once the background recorder has 48 hours of samples, counts the
days from now, and is withheld while the newest sample is more than a day old.

Each item allows one cleanup: **trash** (Put Back in Finder restores it; space
returns when the Trash is emptied), **delete** for caches a tool fills again, after
you type the item's name, **command**, which shows the tool's own cleanup for you to
run, or **never**. Scry never removes the home folder, a folder that Desktop,
Documents, Downloads or Scry's own config or state lies inside, or a cache that
holds a git repository (a bare one or a separate git directory included), a
worktree or a mounted volume.

A worktree labelled "can go" goes to the Trash as a folder only: git keeps its record
in `.git/worktrees/<name>`, so Put Back gives a working checkout with its history,
and until then the worktree shows as "can prune". **prune** removes that one record
and nothing else, never `git worktree prune`, which would drop the record of every
worktree whose folder is missing. It refuses a record that is locked, stages changes
its HEAD lacks, or has a detached HEAD no branch or tag holds; the record's reflog
goes with it, and a folder of it still in the Trash then comes back without its
link to git.

Right before acting, Scry reads `destructive` from the config again, walks the folder
for repositories, worktrees and mounted volumes, reads a worktree's label again with
`git status`, and checks that the folder and every folder above it are the ones it
listed, opening each without following a symlink. A delete goes through the folder
Scry opened, never into another volume, and stops untouched at a protected folder
moved inside while it runs. A prune checks the record again after the audit row
is written. The move to the Trash is the one step macOS takes by path: if something
else takes that path, or a folder above it becomes a symlink, in the moment between
Scry's last check and the move, macOS moves that instead. Nothing is deleted that
way: Scry compares what landed in the Trash with the folder it checked and moves
anything else back, never over what stands there now; when it cannot, it says
where the item is in the Trash and where it belongs. Cleanup is off until you set
`destructive = true`. Every
attempt writes two rows to the audit log in Scry's store: what it is about to do,
then what happened, including exactly what changed if it stopped partway. When the
log cannot be written, nothing is done.

Sizes are kept in Scry's store and checked against FSEvents, so a second run walks
only what changed since the first.

## Background recorder

Disk history and agent ancestry need something running while Scry is closed. A
small daemon records free space on `/` every 10 minutes, takes hook events from the
spool, and records which session each process belongs to while its agent lives, so a
child re-parented to launchd after its agent exits keeps its owner. It rescans when
a watched agent forks or exits.

```sh
scry daemon install     # install the LaunchAgent and start the daemon
scry daemon status      # installed, running, and how fresh the samples are
scry doctor             # check everything, with a fix for each problem
scry daemon uninstall   # stop it and remove the LaunchAgent; the data stays
```

`install` writes `~/Library/LaunchAgents/com.morozred.scry.daemon.plist`. macOS then
shows a "Background Items Added" notice, and Scry appears under System Settings >
General > Login Items & Extensions. If launchd has the daemon disabled,
`scry doctor` says so and `scry daemon install` enables it again. Run `install`
again after you change the config or upgrade Scry: it restarts the daemon when
either changed, and `scry daemon status` says when a restart is needed.

The daemon keeps its data and its log, `daemon.log`, in
`~/Library/Application Support/Scry/`. The log rotates at 1 MB.

The config file is `~/.config/scry/config.toml`. Every key is optional:

```toml
code_roots = ["~/sources", "~/work"]  # where your projects and worktrees live
destructive = false                   # false turns off every stop and delete action
keep = ["postgres"]                   # processes no cleanup rule ever selects

[daemon]
sample_interval_secs = 600            # how often the daemon samples disk space

[tui]
refresh_secs = 5                      # 1 to 60 seconds between live refreshes; 0 only on r
```

Without the file, `code_roots` is whichever of `~/sources`, `~/code`, `~/src` and
`~/Developer` exist. A mistake in the file is an error that names the line.

Set `SCRY_HOME=<dir>` to try Scry without touching your real setup. The config moves
to `<dir>/config.toml`, the data to `<dir>/state/`, and the LaunchAgent gets its own
label.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
