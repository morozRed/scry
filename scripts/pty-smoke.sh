#!/bin/sh
# Runs the TUI in a real pty (tmux) and checks the terminal comes back cooked after
# quit, SIGTERM and Ctrl-Z. Usage: scripts/pty-smoke.sh [path/to/scry]
set -eu

BIN=$(cd "$(dirname "${1:-target/debug/scry}")" && pwd)/$(basename "${1:-target/debug/scry}")
FIX=$(cd "$(dirname "$0")/.." && pwd)/fixtures/typical.json
SOCK=scry-smoke-$$
RUN="$BIN tui --fixture $FIX; echo \"EXIT=\$? \$(stty -a | grep -o -- '-\{0,1\}icanon')\""
fails=0

t() { tmux -L "$SOCK" "$@"; }
screen() { t capture-pane -p -t smoke; }
wait_for() {
  i=0
  while ! screen | grep -q -- "$1"; do
    i=$((i + 1))
    [ "$i" -gt 50 ] && { echo "timeout waiting for: $1"; screen; return 1; }
    sleep 0.1
  done
}
# The shell resets tty modes itself after a job dies from a signal or stops, so a cooked
# terminal alone proves little. Each check also needs the TUI off the screen, which only
# Scry's own cleanup (leaving the alternate screen) achieves.
check() {
  if wait_for "$2" && ! screen | grep -q "Nothing needs attention"; then
    echo "ok   $1"
  else
    echo "FAIL $1"; screen; fails=$((fails + 1))
  fi
}
tui_pid() { pgrep -P "$(t display -p -t smoke '#{pane_pid}')" -f "tui --fixture"; }

trap 't kill-server 2>/dev/null || true' EXIT
t new-session -d -s smoke -x 100 -y 30 sh
sleep 0.3

t send-keys -t smoke "$RUN" Enter
wait_for "Nothing needs attention"
t send-keys -t smoke q
check "quit restores the terminal" "EXIT=0 icanon"
t send-keys -t smoke clear Enter

t send-keys -t smoke "$RUN" Enter
wait_for "Nothing needs attention"
kill -TERM "$(tui_pid)"
check "SIGTERM restores the terminal" "EXIT=0 icanon"
t send-keys -t smoke clear Enter

t send-keys -t smoke "$RUN" Enter
wait_for "Nothing needs attention"
t send-keys -t smoke C-z
check "Ctrl-Z leaves the alternate screen" "Stopped"
t send-keys -t smoke fg Enter
if wait_for "Nothing needs attention"; then echo "ok   fg redraws the TUI"; else echo "FAIL fg redraws the TUI"; fails=$((fails + 1)); fi
t send-keys -t smoke q
sleep 0.3
t send-keys -t smoke "stty -a | grep -o -- '-\{0,1\}icanon' | sed 's/^/RESUMED=/'" Enter
check "quit after resume restores the terminal" "RESUMED=icanon"

[ "$fails" -eq 0 ] && echo "pty smoke: all passed" || { echo "pty smoke: $fails failed"; exit 1; }
