# Scry

Scry shows which agent session left each process, port, worktree and gigabyte on
your Mac.

Coding agents such as Claude Code and Codex start dev servers, MCP servers and
headless browsers, create worktrees and fill caches. When a session ends, much of
that stays behind. Scry traces each of these back to the project and the agent
session that made it, so you can see what is safe to stop or delete. An MCP server
that gives agents the same answers is planned.

> **Status:** early development. The process scan and the terminal UI shell work.
> Ports, agents, disk and worktrees are being built. See the
> [roadmap](PLAN.md).

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

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
