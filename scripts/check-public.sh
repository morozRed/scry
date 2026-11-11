#!/bin/sh
# Fails if tracked files, or lines added on this branch's commits, hold something
# personal: a home path, this machine's host name, an email address, a private IP, or a
# credential in a common format. Branch history counts because pull requests stay public.
# Usage: scripts/check-public.sh [base-ref]   (base defaults to origin/main when it exists)
set -eu
cd "$(git rev-parse --show-toplevel)"

base=${1:-}
if [ -z "$base" ] && git rev-parse -q --verify origin/main >/dev/null; then base=origin/main; fi

placeholders='dev|Shared|ann|anna|alice|bob|example|you|me|name|user|USER'
home_re="/(Users|home)/([A-Za-z0-9._-]+)"
email_re='[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'
ipv4_re='(^|[^0-9.])(10\.[0-9]{1,3}|192\.168|172\.(1[6-9]|2[0-9]|3[01]))\.[0-9]{1,3}\.[0-9]{1,3}([^0-9.]|$)'
ipv6_re='(^|[^0-9a-fA-F:])(f[cd][0-9a-fA-F]{2}|fe80):[0-9a-fA-F]{0,4}:[0-9a-fA-F:]*'
cred_re='(gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|sk-[A-Za-z0-9_-]{20,}|(AKIA|ASIA)[0-9A-Z]{16}|xox[abpr]-[A-Za-z0-9-]{10,}|-----BEGIN [A-Z ]*PRIVATE KEY-----)'
host=$(hostname -s)

added=$(mktemp)
trap 'rm -f "$added"' EXIT
if [ -n "$base" ]; then
  git log -p --no-color --format='commit %h' "$base..HEAD" -- . ':!scripts/check-public.sh' |
    awk '/^commit /{c=$2} /^\+\+\+ b\//{f=substr($0,7)} /^\+[^+]/{print c ":" f ": " substr($0,2)}' >"$added"
fi

found=0
# Prints "file:line:text" for tracked files, then "commit:file: text" for added lines.
search() {
  git ls-files -z -- . ':!scripts/check-public.sh' | xargs -0 grep -I -n -H "$@" -- 2>/dev/null || true
  grep "$@" "$added" 2>/dev/null || true
}
report() {
  [ -z "$2" ] && return 0
  echo "check-public: $1"
  printf '%s\n' "$2" | sed 's/^/  /' | head -20
  found=1
}

report "home paths (use /Users/dev)" "$(search -E "$home_re" | grep -v -E "/(Users|home)/($placeholders)([^A-Za-z0-9._-]|$)" || true)"
[ "${#host}" -ge 4 ] && report "this machine's host name" "$(search -w -F "$host")"
report "email addresses" "$(search -o -E "$email_re" | grep -v -E '@(example\.(com|org|net)|users\.noreply\.github\.com)$' || true)"
report "private-network IPv4 (use 192.0.2.0/24)" "$(search -E "$ipv4_re")"
report "private IPv6 (use 2001:db8::/32)" "$(search -E "$ipv6_re")"
report "credentials" "$(search -E "$cred_re")"

[ "$found" -eq 0 ] && echo "check-public: clean${base:+ (files and commits since $base)}"
exit "$found"
