# Contributing to Scry

Thanks for helping. This guide covers how to build Scry, run its checks and send a
change.

## Layout

| Path | What it holds |
|---|---|
| `crates/scry-core` | Reading the system, the data model and the view models. No terminal or UI types, so a future menu bar app can reuse it. |
| `crates/scry` | The `scry` binary: the terminal UI and the CLI commands. |
| `fixtures/` | Synthetic snapshots that tests and screenshots draw from. |
| `scripts/` | Checks that tests alone cannot run, such as the terminal smoke test. |

## Build and test

```sh
cargo build
cargo test
```

UI tests compare rendered frames with snapshots stored in
`crates/scry/src/snapshots/`. When you change the UI on purpose, review and accept
the new frames:

```sh
cargo install cargo-insta
cargo insta review
```

`scripts/pty-smoke.sh` runs the UI in a real terminal through `tmux` and checks
that the terminal is restored after quit, SIGTERM, Ctrl-Z and a closed terminal.
It needs `tmux` (`brew install tmux`).

## Before you open a pull request

Run every check on a clean checkout of your commit:

```sh
scripts/local-ci.sh HEAD
```

It runs `scripts/check-public.sh`, `cargo fmt --check`, `cargo clippy` with
warnings as errors, the tests, the smoke test and a scan benchmark.

## Fixtures and privacy

Tests, fixtures and screenshots use synthetic data only. Never commit a snapshot
recorded from your own machine. `scry snapshot` redacts your home directory, but
process names and paths can still identify you. Recorded snapshots belong in
`fixtures/recorded/`, which git ignores. `scripts/check-public.sh` checks tracked
files and every line your branch adds, because pull requests stay public even after
a fix. It fails on home paths other than placeholders such as `/Users/dev`, your
host name, email addresses, private IPv4 and IPv6 addresses, and credentials in
common formats. It cannot read binary files, so check those by hand.

## Commits and pull requests

- Write commit titles as [Conventional Commits](https://www.conventionalcommits.org/),
  for example `fix(tui): restore the cursor after Ctrl-Z`.
- Keep a pull request to one change. Say why it exists, what it leaves out and how
  you checked it.
- Keep code comments for a reason the code cannot show.
