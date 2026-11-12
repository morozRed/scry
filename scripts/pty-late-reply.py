#!/usr/bin/env python3
"""Plays a terminal on a bare pty and checks the colour query and terminal setup end to
end: an answer that comes late, in pieces or after the wait never reaches the TUI as keys,
keys typed while Scry waits are kept whole, an unknown terminal is not asked, a setup that
fails leaves the terminal cooked, and SIGTERM ends Scry with the terminal cooked even
during the query or once the terminal stops reading.

tmux answers the query itself, so this drives a bare pty.
Usage: scripts/pty-late-reply.py [path/to/scry]
"""

import fcntl
import os
import re
import select
import signal
import struct
import sys
import termios
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/debug/scry")
FIX = os.path.join(ROOT, "fixtures", "typical.json")
DESIGN = os.path.join(ROOT, "fixtures", "design.json")
QUERY = b"\x1b]11;?"
REPLY = (
    b"\x1b]10;rgb:c5c5/c8c8/c6c6\x1b\\"
    b"\x1b]11;rgb:1d1d/1f1f/2121\x1b\\"
    b"\x1b[?62;22c"
)
DRAWN = b"not collected yet"
PORTS = b"Port scanning"
TRUECOLOR = b"\x1b[38;2;"
ROWS, COLS = 30, 100
# Scry waits this long after SIGTERM for its loop to stop before it restores and exits.
GRACE = 1.0
SEQUENCE = re.compile(r"\x1b(\[[0-9;?]*[ -/]*[@-~]|\][^\x07\x1b]*(\x07|\x1b\\)|.)")


def screen(out):
    """The text on screen after `out`, for the escape codes ratatui writes: cursor moves,
    clears and the alternate screen move or clear the grid, other sequences change nothing."""
    grid = [[" "] * COLS for _ in range(ROWS)]
    row = col = 0
    text = out.decode("utf-8", "replace")
    i = 0
    while i < len(text):
        m = SEQUENCE.match(text, i)
        if m:
            seq = m.group(0)
            if seq.startswith("\x1b[") and seq.endswith("H"):
                r, _, c = seq[2:-1].partition(";")
                row, col = int(r or 1) - 1, int(c or 1) - 1
            elif seq in ("\x1b[2J", "\x1b[?1049h", "\x1b[?1049l"):
                grid = [[" "] * COLS for _ in range(ROWS)]
            i = m.end()
            continue
        ch = text[i]
        if ch == "\r":
            col = 0
        elif ch == "\n":
            row += 1
        elif 0 <= row < ROWS and 0 <= col < COLS:
            grid[row][col] = ch
            col += 1
        i += 1
    return "\n".join("".join(r) for r in grid)


class Tui:
    def __init__(self, term_program="ghostty", fixture=FIX, full_output=False, left_non_blocking=False):
        drop = ("NO_COLOR", "SSH_CONNECTION", "SSH_TTY", "SCRY_COLOR_QUERY", "TERM_PROGRAM", "COLORFGBG", "TMUX")
        env = {k: v for k, v in os.environ.items() if k not in drop}
        env.update(TERM="xterm-256color", COLORTERM="truecolor")
        if term_program:
            env["TERM_PROGRAM"] = term_program
        self.fd, slave = os.openpty()
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        report, told = os.pipe()
        # A stand-in shell leads the session and owns the terminal, as in a real terminal.
        # Scry is its child: on macOS a session leader's exit waits for the terminal to read
        # its output, which is not Scry's to wait for.
        self.shell = os.fork()
        if self.shell == 0:
            os.close(self.fd)
            os.close(report)
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
            pid = os.fork()
            if pid == 0:
                self.exec_scry(slave, env, fixture, full_output, left_non_blocking)
            os.write(told, struct.pack("i", pid))
            os.write(told, struct.pack("i", os.waitpid(pid, 0)[1]))
            os._exit(0)
        os.close(slave)
        os.close(told)
        self.report = report
        self.pid = struct.unpack("i", os.read(report, 4))[0]
        self.out = b""
        self.status = None

    @staticmethod
    def exec_scry(slave, env, fixture, full_output, left_non_blocking):
        for fd in (0, 1, 2):
            os.dup2(slave, fd)
        os.close(slave)
        if full_output:
            # Fills the terminal's output queue, as a terminal that stopped reading leaves
            # it, so Scry's first write blocks.
            flags = fcntl.fcntl(1, fcntl.F_GETFL)
            fcntl.fcntl(1, fcntl.F_SETFL, flags | os.O_NONBLOCK)
            try:
                while True:
                    os.write(1, b"x" * 512)
            except BlockingIOError:
                pass
            if not left_non_blocking:
                fcntl.fcntl(1, fcntl.F_SETFL, flags)
        if left_non_blocking:
            # As a program that set the terminal non-blocking and exited leaves it: Scry's
            # first write fails with EAGAIN instead of blocking. The error report goes
            # elsewhere so it does not fail too.
            null = os.open(os.devnull, os.O_WRONLY)
            os.dup2(null, 2)
            os.close(null)
        os.execve(BIN, [BIN, "tui", "--fixture", fixture], env)

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
        if self.status is None and select.select([self.report], [], [], 0)[0]:
            self.status = struct.unpack("i", os.read(self.report, 4))[0]
        return self.status is not None

    def wait_exit(self, seconds, read=True):
        """Waits for Scry to exit. With `read` off nothing is read meanwhile, as a terminal
        that stopped reading does, so the output queue stays full."""
        end = time.monotonic() + seconds
        while not self.exited() and time.monotonic() < end:
            if read:
                self.pump(0.05)
            else:
                time.sleep(0.01)
        return self.exited()

    def shows(self, text, seconds=3):
        """Reads until `text` is on screen; true when it got there."""
        end = time.monotonic() + seconds
        while text not in screen(self.out) and time.monotonic() < end and not self.exited():
            self.pump(0.05)
        return text in screen(self.out)

    def code(self):
        return os.WEXITSTATUS(self.status) if self.status is not None and os.WIFEXITED(self.status) else None

    def quits(self, key=b"q"):
        """Presses `key` once the screen settled; true when that ends Scry with status 0."""
        self.pump(0.4)
        self.write(key)
        return self.wait_exit(3) and self.code() == 0

    def cooked(self):
        lflag = termios.tcgetattr(self.fd)[3]
        return bool(lflag & termios.ICANON) and bool(lflag & termios.ECHO)

    def close(self):
        if not self.exited():
            os.kill(self.pid, signal.SIGKILL)
        # Reads while the shell ends, since its exit waits for the output queue to drain.
        while not os.waitpid(self.shell, os.WNOHANG)[0]:
            if select.select([self.fd], [], [], 0.05)[0]:
                try:
                    os.read(self.fd, 65536)
                except OSError:
                    pass
        os.close(self.report)
        os.close(self.fd)


fails = 0


def check(name, ok, tui):
    global fails
    print(("ok   " if ok else "FAIL ") + name)
    if not ok:
        fails += 1
        tail = tui.out[-400:].decode("utf-8", "replace").encode("unicode_escape").decode()
        print("  last output: " + tail)
        print("  status: %s, cooked: %s, screen:" % (tui.status, tui.cooked()))
        for line in screen(tui.out).splitlines():
            if line.strip():
                print("  | " + line.rstrip())


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


def typed(name, timeline, text, fixture=FIX, quit_key=b"q"):
    """Writes each (seconds after the query, bytes) of `timeline`, then checks `text` is on
    screen and quits with `quit_key`."""
    t = Tui(fixture=fixture)
    asked = t.wait_for(QUERY)
    start = time.monotonic()
    for at, data in timeline:
        t.pump(max(0, start + at - time.monotonic()))
        t.write(data)
    check(name, asked and t.shows(text) and t.quits(quit_key), t)
    t.close()


# BackTab from Overview wraps to Worktrees.
typed(
    "BackTab typed while Scry waits for the answer reaches the TUI",
    [(0.1, b"\x1b[Z"), (0.2, REPLY)],
    "Worktree scanning",
)
# On Agents, h folds the first session and plain Right would unfold it; Ctrl-Right does
# nothing, so the fold marker stays.
typed(
    "Ctrl-Right typed while Scry waits keeps its modifier",
    [(0.1, b"3h\x1b[1;5C"), (0.2, REPLY)],
    "+4",
    fixture=DESIGN,
)
typed(
    "a key split across the 1 s deadline keeps its ESC [ and reaches the TUI",
    [(0.9, b"\x1b["), (1.3, b"Z"), (1.6, REPLY)],
    "Worktree scanning",
)
typed(
    "a character split where Scry hands the terminal to the event loop is typed whole",
    [(0.1, REPLY + b"/\xc3"), (0.11, b"\xa9")],
    "/é",
    quit_key=b"\x03",
)

# Raw mode goes on, then entering the alternate screen fails.
t = Tui(term_program=None, full_output=True, left_non_blocking=True)
failed = t.wait_exit(3, read=False) and t.code() == 1
check(
    "a setup whose first write fails exits 1 and leaves the terminal cooked",
    failed and t.cooked(),
    t,
)
t.close()


def within(seconds, done):
    end = time.monotonic() + seconds
    while not done() and time.monotonic() < end:
        time.sleep(0.01)
    return done()


# Setup blocks on its first write to the full output queue, with raw mode already on.
t = Tui(term_program=None, full_output=True)
raw = within(3, lambda: not t.cooked())
os.kill(t.pid, signal.SIGTERM)
check(
    "SIGTERM while setup waits on a full output queue exits 143 in the grace period, cooked",
    raw and t.wait_exit(GRACE + 0.5, read=False) and t.code() == 143 and t.cooked(),
    t,
)
t.close()

# The terminal stops reading, then Tab redraws the screen. A frame is several KB and the
# queue holds about 1 KB, so the frame write blocks.
t = Tui(term_program=None, fixture=DESIGN)
drawn = t.shows("fixture design.json")
t.write(b"\t\t\t")
time.sleep(0.5)
os.kill(t.pid, signal.SIGTERM)
check(
    "SIGTERM once the terminal stops reading exits 143 in the grace period, cooked",
    drawn and t.wait_exit(GRACE + 0.5, read=False) and t.code() == 143 and t.cooked(),
    t,
)
t.close()

if fails:
    print(f"pty late reply: {fails} failed")
    sys.exit(1)
print("pty late reply: all passed")
