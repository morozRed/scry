#!/bin/sh
# Renders the TUI from fixtures into docs/screens/*.svg, then PNGs of them into
# target/shots/ with rsvg-convert (brew install librsvg) for viewing.
# Usage: scripts/shots.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
cargo run -q -p scry --example shots

if ! command -v rsvg-convert >/dev/null; then
  echo "rsvg-convert not found, so no PNGs. brew install librsvg"
  exit 0
fi
find docs/screens -name '*.svg' | while read -r svg; do
  png="target/shots/${svg#docs/screens/}"
  png="${png%.svg}.png"
  mkdir -p "$(dirname "$png")"
  rsvg-convert --zoom 2 -o "$png" "$svg"
done
echo "PNGs in $ROOT/target/shots"
