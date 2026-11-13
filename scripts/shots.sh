#!/bin/sh
# Renders the TUI from fixtures into target/shots/*.svg, then copies the screens the README
# shows into docs/screens/, which is committed. With rsvg-convert (brew install librsvg) it
# also writes PNGs beside the SVGs in target/shots for viewing; those are not committed.
# Usage: scripts/shots.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
cargo run -q -p scry --example shots

# The README's screens, light and dark. Every name in them comes from fixtures/design.json.
README="agents-120x40 overview-80x24 ports-80x24 agents-80x24 disk-80x24 worktrees-80x24
top-120x40 sheet-80x24"
mkdir -p docs/screens
for shot in $README; do
  view=${shot%-*}
  size=${shot##*-}
  for p in light dark; do
    if [ "$view" = sheet ]; then name="sheet-$p-$size"; else name="$view-$p-$size"; fi
    cp "target/shots/$name.svg" "docs/screens/$name.svg"
  done
done
echo "README screens in $ROOT/docs/screens"

if ! command -v rsvg-convert >/dev/null; then
  echo "rsvg-convert not found, so no PNGs. brew install librsvg"
  exit 0
fi
find target/shots -name '*.svg' | while read -r svg; do
  rsvg-convert --zoom 2 -o "${svg%.svg}.png" "$svg"
done
echo "PNGs in $ROOT/target/shots"
