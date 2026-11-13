#!/bin/sh
# Renders the TUI from fixtures into target/shots/*.svg, then PNGs beside them with
# rsvg-convert (brew install librsvg) for viewing. Nothing here is committed.
# Usage: scripts/shots.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
cargo run -q -p scry --example shots

if ! command -v rsvg-convert >/dev/null; then
  echo "rsvg-convert not found, so no PNGs. brew install librsvg"
  exit 0
fi
find target/shots -name '*.svg' | while read -r svg; do
  rsvg-convert --zoom 2 -o "${svg%.svg}.png" "$svg"
done
echo "PNGs in $ROOT/target/shots"
