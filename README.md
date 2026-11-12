# Scry

Scry shows which agent session left each process, port, worktree and gigabyte on
your Mac.

Coding agents such as Claude Code and Codex start dev servers, MCP servers and
headless browsers, create worktrees and fill caches. When a session ends, much of
that stays behind. Scry traces each of these back to the project and the agent
session that made it, so you can see what is safe to stop or delete. An MCP server
that gives agents the same answers is planned.

> **Status:** early development. The process scan, the terminal UI shell and the
> `ports`, `why` and `stop` commands work. Agents, disk and worktrees are being
> built. See the [roadmap](PLAN.md).

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
synthetic fixture with `cargo run -p scry --example shots`; the Agents, Disk and
Worktrees views show sample data until those parts are built. The design notes are
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

## Background recorder

Disk history needs something running while Scry is closed. A small daemon records
free space on `/` every 10 minutes.

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
refresh_secs = 2                      # 1 to 60 seconds between live refreshes; 0 only on r
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
