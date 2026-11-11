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
`docker exec … mysql …`. A label holds only the program, common subcommand words
such as `run` and `dev`, flag names, files that exist and ports the process listens
on. Any other argument becomes `…`, whatever it holds, so passwords and tokens
given as values stay out of tables, `--json` and `scry snapshot`. `--show-command`
prints the full command line as text on request; it is never part of `--json`.

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
