#!/usr/bin/env python3
"""Plays a terminal on a bare pty and checks the colour query end to end: an answer that
comes late, in pieces or after the wait never reaches the TUI as keys, keys typed while
Scry waits are kept, an unknown terminal is not asked, and SIGTERM during the query leaves
the terminal cooked.

tmux answers the query itself, so this drives a bare pty.
Usage: scripts/pty-late-reply.py [path/to/scry]
"""

import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/debug/scry")
FIX = os.path.join(ROOT, "fixtures", "typical.json")
QUERY = b"\x1b]11;?"
REPLY = (
    b"\x1b]10;rgb:c5c5/c8c8/c6c6\x1b\\"
    b"\x1b]11;rgb:1d1d/1f1f/2121\x1b\\"
    b"\x1b[?62;22c"
)
DRAWN = b"not collected yet"
PORTS = b"Port scanning"
TRUECOLOR = b"\x1b[38;2;"


class Tui:
    def __init__(self, term_program="ghostty"):
        drop = ("NO_COLOR", "SSH_CONNECTION", "SSH_TTY", "SCRY_COLOR_QUERY", "TERM_PROGRAM", "COLORFGBG", "TMUX")
        env = {k: v for k, v in os.environ.items() if k not in drop}
        env.update(TERM="xterm-256color", COLORTERM="truecolor")
        if term_program:
            env["TERM_PROGRAM"] = term_program
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execve(BIN, [BIN, "tui", "--fixture", FIX], env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        self.out = b""
        self.status = None

    def pump(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if self.exited():
                return
            r, _, _ = select.select([self.fd], [], [], 0.005)
            if r:
                try:
                    self.out += os.read(self.fd, 65536)
                except OSError:
                    return

    def wait_for(self, needle, seconds=5):
        end = time.monotonic() + seconds
        while needle not in self.out and time.monotonic() < end and not self.exited():
            self.pump(0.005)
        return needle in self.out

    def write(self, data):
        if not self.exited():
            os.write(self.fd, data)

    def exited(self):
        if self.status is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = status
        return self.status is not None

    def wait_exit(self, seconds):
        end = time.monotonic() + seconds
        while not self.exited() and time.monotonic() < end:
            self.pump(0.05)
        return self.exited()

    def code(self):
        return os.WEXITSTATUS(self.status) if self.status is not None and os.WIFEXITED(self.status) else None

    def quits(self):
        """Presses q once the screen settled; true when that ends Scry with status 0."""
        self.pump(0.4)
        self.write(b"q")
        return self.wait_exit(3) and self.code() == 0

    def cooked(self):
        lflag = termios.tcgetattr(self.fd)[3]
        return bool(lflag & termios.ICANON) and bool(lflag & termios.ECHO)

    def close(self):
        if not self.exited():
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
        os.close(self.fd)


fails = 0


def check(name, ok, tui):
    global fails
    print(("ok   " if ok else "FAIL ") + name)
    if not ok:
        fails += 1
        tail = tui.out[-400:].decode("utf-8", "replace").encode("unicode_escape").decode()
        print("  last output: " + tail)


def answer(name, timeline, look=None):
    """Writes each (seconds after the query, bytes) of `timeline`, then presses q."""
    t = Tui()
    asked = t.wait_for(QUERY)
    start = time.monotonic()
    for at, data in timeline:
        t.pump(max(0, start + at - time.monotonic()))
        t.write(data)
    drawn = t.wait_for(DRAWN)
    ok = asked and drawn and t.quits()
    if look is not None:
        ok = ok and (TRUECOLOR in t.out) == look
    check(name, ok, t)
    t.close()


answer("a reply 400 ms late is read before the TUI starts and sets the tinted look", [(0.4, REPLY)], look=True)
answer("a reply 1.5 s late is not read as keys", [(1.5, REPLY)])
answer(
    "a reply 1.5 s late whose ESC comes in a read of its own is not read as keys",
    [(1.5, b"\x1b"), (1.53, REPLY[1:])],
)
answer(
    "a reply cut at the 1 s deadline keeps its framing and is not read as keys",
    [(0.9, REPLY[:40]), (1.3, REPLY[40:])],
)

t = Tui()
asked = t.wait_for(QUERY)
t.write(b"2")
t.pump(0.1)
t.write(REPLY)
check("a key typed while Scry waits for the answer is kept", asked and t.wait_for(PORTS) and t.quits(), t)
t.close()

t = Tui(term_program=None)
t.write(b"2")
drawn = t.wait_for(PORTS)
t.pump(1.5)
check(
    "an unknown terminal is not asked, and a key typed at once is kept",
    drawn and QUERY not in t.out and t.quits(),
    t,
)
t.close()

t = Tui()
asked = t.wait_for(QUERY)
raw = not t.cooked()
os.kill(t.pid, signal.SIGTERM)
check(
    "SIGTERM during the colour query leaves the terminal cooked and exits 143",
    asked and raw and t.wait_exit(3) and t.cooked() and t.code() == 143,
    t,
)
t.close()

if fails:
    print(f"pty late reply: {fails} failed")
    sys.exit(1)
print("pty late reply: all passed")
