#!/usr/bin/env python3
"""Plays a terminal that answers Scry's colour query late, as one over ssh does, and
checks the answer never reaches the TUI as keys. Also checks that SIGTERM during the
query leaves the terminal cooked.

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
DRAWN = b"Nothing needs attention"
TRUECOLOR = b"\x1b[38;2;"


class Tui:
    def __init__(self):
        env = {k: v for k, v in os.environ.items() if k not in ("NO_COLOR", "SSH_CONNECTION", "SCRY_COLOR_QUERY")}
        env.update(TERM="xterm-256color", COLORTERM="truecolor")
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
            r, _, _ = select.select([self.fd], [], [], 0.02)
            if r:
                try:
                    self.out += os.read(self.fd, 65536)
                except OSError:
                    return

    def wait_for(self, needle, seconds=5):
        end = time.monotonic() + seconds
        while needle not in self.out and time.monotonic() < end and not self.exited():
            self.pump(0.01)
        return needle in self.out

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


def late(delay, name):
    """Answers `delay` seconds after the query, then presses q once the TUI is drawn."""
    t = Tui()
    asked = t.wait_for(QUERY)
    t.pump(delay)
    if not t.exited():
        os.write(t.fd, REPLY)
    drawn = t.wait_for(DRAWN)
    t.pump(0.4)
    if not t.exited():
        os.write(t.fd, b"q")
    quit_ok = t.wait_exit(3) and os.WIFEXITED(t.status) and os.WEXITSTATUS(t.status) == 0
    check(name, asked and drawn and quit_ok, t)
    truecolor = TRUECOLOR in t.out
    t.close()
    return truecolor


used = late(0.4, "a reply 400 ms late is read before the TUI starts, and q quits")
print(("ok   " if used else "FAIL ") + "a reply 400 ms late still sets the tinted look")
fails += 0 if used else 1
late(1.5, "a reply 1.5 s late, after the TUI started, is not read as keys, and q quits")

t = Tui()
asked = t.wait_for(QUERY)
raw = not t.cooked()
os.kill(t.pid, signal.SIGTERM)
check(
    "SIGTERM during the colour query leaves the terminal cooked",
    asked and raw and t.wait_exit(3) and t.cooked(),
    t,
)
t.close()

if fails:
    print(f"pty late reply: {fails} failed")
    sys.exit(1)
print("pty late reply: all passed")
