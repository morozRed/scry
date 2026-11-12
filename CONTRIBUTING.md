# Contributing to Scry

Thanks for helping. This guide covers how to build Scry, run its checks and send a
change. To learn how the code fits together, start with
[docs/architecture.md](docs/architecture.md).

## Before you start

- For a bug, open an issue with the bug report template, including `scry doctor` output.
- For a new feature or a larger change, open an issue first so we can agree on the shape
  before you write the code. [docs/roadmap.md](docs/roadmap.md) says what is planned and
  what is not.

## What you need

- macOS 14 or later on Apple Silicon. Scry reads macOS-only kernel interfaces, so it does
  not build or run anywhere else.
- Rust 1.89 or later, with `rustfmt` and `clippy` (`rustup component add rustfmt clippy`).
- The Xcode Command Line Tools (`xcode-select --install`).
- `tmux` for the terminal smoke test (`brew install tmux`).
- Optional: `librsvg` for PNGs of the screenshots (`brew install librsvg`).

## Layout

| Path | What it holds |
|---|---|
| `crates/scry-core` | Reading the system, the data model, attribution, the store, the daemon, cleanup and the view models. No terminal or UI types, so a future menu bar app can reuse it. |
| `crates/scry` | The `scry` binary: the CLI commands and the terminal UI. |
| `docs/` | Architecture, design and roadmap notes, and the screenshots in `docs/screens/`. |
| `fixtures/` | Synthetic snapshots that tests and screenshots draw from. |
| `packaging/` | The Homebrew formula. |
| `scripts/` | Checks that tests alone cannot run, such as the terminal smoke test. |

## Build and test

```sh
cargo build
cargo test
cargo run -- tui --fixture fixtures/typical.json   # the UI on synthetic data
```

UI tests compare rendered frames with snapshots stored in
`crates/scry/src/tui/snapshots/`. When you change the UI on purpose, review and accept
the new frames:

```sh
cargo install cargo-insta
cargo insta review
```

`scripts/shots.sh` renders every screenshot in `docs/screens/` from the fixtures. Run it
when a change alters how a view looks, and commit the new SVGs with the change.

`scripts/pty-smoke.sh` runs the UI in a real terminal through `tmux` and checks that the
terminal is restored after quit, SIGTERM, Ctrl-Z and a closed terminal.

Set `SCRY_HOME` to a throwaway folder while you try your change by hand, so it never
touches your real config, data or LaunchAgent.

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
  you checked it. The pull request template asks for these.
- Update the README, the docs and `--help` text when behaviour changes, and add a line
  to the `Unreleased` section of [CHANGELOG.md](CHANGELOG.md).
- Keep code comments for a reason the code cannot show.

## License

By contributing, you agree that your contributions are licensed under the
[MIT License](LICENSE).
