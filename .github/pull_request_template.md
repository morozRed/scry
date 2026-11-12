## Why

<!-- The problem this solves, or the issue it closes. -->

## What changed

## What it leaves out

## How I checked it

<!-- Commands you ran and what you looked at. For UI changes, the snapshots you
accepted with `cargo insta review`, and a screenshot from `scripts/shots.sh` if the
look changed. -->

- [ ] `scripts/local-ci.sh HEAD` passes on a clean checkout of this commit
- [ ] The title is a [Conventional Commit](https://www.conventionalcommits.org/), such as `fix(tui): restore the cursor after Ctrl-Z`
- [ ] Tests, fixtures and screenshots use synthetic data only
- [ ] Docs and `--help` text match the change
