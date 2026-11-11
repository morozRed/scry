#!/bin/sh
# Runs the CI steps on a clean worktree at one commit. With --post, records the result on
# GitHub as a `local-ci` commit status, which stands in for Actions while it cannot run.
# Usage: scripts/local-ci.sh <commit-ish> [--post]
set -eu

REF=${1:?usage: scripts/local-ci.sh <commit-ish> [--post]}
POST=${2:-}
ROOT=$(git rev-parse --show-toplevel)
SHA=$(git -C "$ROOT" rev-parse "$REF^{commit}")
DIR=$(mktemp -d "${TMPDIR:-/tmp}/scry-ci.XXXXXX")
LOG="$DIR.log"
git -C "$ROOT" worktree add --detach -q "$DIR" "$SHA"
trap 'git -C "$ROOT" worktree remove --force "$DIR"' EXIT

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/local-ci}"
cd "$DIR"
echo "local-ci at $SHA, log $LOG"
state=success
failed=""
for step in \
  "cargo fmt --check" \
  "cargo clippy --all-targets -- -D warnings" \
  "cargo test" \
  "cargo build" \
  "scripts/pty-smoke.sh $CARGO_TARGET_DIR/debug/scry" \
  "cargo run -q -- bench scan --runs 30"; do
  if sh -c "$step" >>"$LOG" 2>&1; then
    echo "ok   $step"
  else
    echo "FAIL $step"
    state=failure
    failed=$step
    break
  fi
done
[ "$state" = failure ] && tail -40 "$LOG"
bench=$(grep -o 'median [0-9.]* ms' "$LOG" | tail -1 || true)

if [ "$POST" = "--post" ]; then
  repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)
  if [ "$state" = success ]; then desc="fmt, clippy, tests, pty smoke pass; scan $bench"; else desc="failed: $failed"; fi
  gh api -X POST "repos/$repo/statuses/$SHA" -f state="$state" -f context=local-ci \
    -f description="$(printf '%s' "$desc" | cut -c1-140)" >/dev/null
  echo "posted local-ci=$state to $SHA"
fi
[ "$state" = success ]
