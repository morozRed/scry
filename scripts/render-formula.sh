#!/bin/sh
# Fills packaging/homebrew/scry.rb with a release's version and the sha256 of its
# scry-<version>-darwin-arm64.tar.gz, for the tap. The Release workflow runs it.
# Usage: scripts/render-formula.sh <version> <sha256> <out>
set -eu

[ $# -eq 3 ] || { echo "usage: $0 <version> <sha256> <out>" >&2; exit 1; }
version=$1 sha256=$2 out=$3
root=$(cd "$(dirname "$0")/.." && pwd)

printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' ||
  { echo "not a version: $version" >&2; exit 1; }
printf '%s\n' "$sha256" | grep -Eq '^[0-9a-f]{64}$' ||
  { echo "not a sha256: $sha256" >&2; exit 1; }

sed -e "s|__VERSION__|$version|g" -e "s|__SHA256__|$sha256|g" \
  "$root/packaging/homebrew/scry.rb" >"$out"
if grep -q '__[A-Z0-9]*__' "$out"; then
  echo "the formula has an unfilled placeholder" >&2
  exit 1
fi
echo "$out"
