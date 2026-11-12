# scry-core

The engine behind [Scry](https://github.com/morozRed/scry), a macOS system monitor that
traces every process, port, git worktree and gigabyte back to the project and the coding
agent session that made it.

This crate reads the machine (processes, sockets, disk, git worktrees), ties processes to
agent sessions, decides which ones are leftovers, and turns all of it into plain view
models. It holds no terminal or UI types. The `scry` crate is the command-line tool and
terminal UI built on it.

It runs on macOS today; Linux support is on the
[roadmap](https://github.com/morozRed/scry/blob/main/docs/roadmap.md#linux). Its API follows the needs of the `scry` binary and is not yet
stable. See [docs/architecture.md](https://github.com/morozRed/scry/blob/main/docs/architecture.md)
for how it works.

License: MIT
