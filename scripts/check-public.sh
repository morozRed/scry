#!/bin/sh
# Fails if a tracked text file holds something personal: this machine's home path,
# user or host name, an email address, a private-network IP, or a credential.
# The machine-specific patterns come from whoever runs it, so it works for any contributor.
set -eu
cd "$(git rev-parse --show-toplevel)"

user=$(id -un)
host=$(hostname -s)
patterns=$(mktemp)
trap 'rm -f "$patterns"' EXIT
{
  printf '%s\n' "$HOME/"
  [ "$user" != dev ] && printf '%s\n' "/Users/$user"
  printf '%s\n' "$host"
} >"$patterns"

found=0
report() {
  echo "check-public: $1"
  echo "$2" | sed 's/^/  /'
  found=1
}

files=$(git ls-files -z | xargs -0 grep -I -l '' -- 2>/dev/null | grep -v '^scripts/check-public.sh$' || true)
[ -z "$files" ] && exit 0

hits=$(echo "$files" | tr '\n' '\0' | xargs -0 grep -n -F -f "$patterns" -- 2>/dev/null || true)
[ -n "$hits" ] && report "this machine's home path, user or host name" "$hits"

hits=$(echo "$files" | tr '\n' '\0' | xargs -0 grep -n -o -E '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' -- 2>/dev/null |
  grep -v -E '@(example\.(com|org|net)|users\.noreply\.github\.com)$' || true)
[ -n "$hits" ] && report "email addresses" "$hits"

hits=$(echo "$files" | tr '\n' '\0' | xargs -0 grep -n -E '(^|[^0-9.])(10\.[0-9]{1,3}|192\.168|172\.(1[6-9]|2[0-9]|3[01]))\.[0-9]{1,3}\.?[0-9]{0,3}([^0-9.]|$)' -- 2>/dev/null || true)
[ -n "$hits" ] && report "private-network IP addresses (use 192.0.2.0/24 or 2001:db8::/32)" "$hits"

hits=$(echo "$files" | tr '\n' '\0' | xargs -0 grep -n -E '(gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|sk-[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----)' -- 2>/dev/null || true)
[ -n "$hits" ] && report "credentials" "$hits"

[ "$found" -eq 0 ] && echo "check-public: clean"
exit "$found"
