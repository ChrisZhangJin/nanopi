"""Manual Linux PTY regression: python3 tests/inline_image_pty.py path/to/nanopi.

Plays the terminal: answers cursor, XTVERSION and DA1 queries the way a
given terminal would, and checks what nanopi draws.
"""

import base64
import errno
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import sys
import tempfile
import termios
import time


LOGO = (Path(__file__).resolve().parents[1] / "assets/nanopi-logo.png").read_bytes()
OSC = b"\x1b]1337;"
CPR = b"\x1b[6n"
XTVERSION = b"\x1b[>0q"
DA1 = b"\x1b[c"
ANSI = re.compile(rb"\x1b\[[0-9;?>]*[A-Za-z]|\x1b\][^\x07]*\x07|\x1b[78]")
GIF = base64.b64decode("R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7")


def image(width, height):
    return re.compile(
        rb"\x1b\]1337;File=inline=1;size=(\d+);width=" + str(width).encode()
        + rb";height=" + str(height).encode()
        + rb";preserveAspectRatio=1:([A-Za-z0-9+/=]+)\x07"
    )


class Tui:
    """`name`: what the terminal answers to XTVERSION (None: it doesn't)."""

    def __init__(self, binary, home, env_extra, name):
        env = dict(os.environ, NANOPI_HOME=str(home), TERM="xterm-256color",
                   NO_PROXY="127.0.0.1", no_proxy="127.0.0.1")
        for key in ("NANOPI_INLINE_IMAGES", "TERM_PROGRAM", "LC_TERMINAL", "TMUX",
                    "NEXUSTERM_INLINE_IMAGES", "SSH_CONNECTION", "SSH_TTY",
                    "http_proxy", "https_proxy", "all_proxy",
                    "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            env.pop(key, None)
        env.update(env_extra)
        self.name = name
        self.pid, self.fd = pty.fork()
        if not self.pid:
            os.chdir(home)
            os.execve(binary, [binary, "-a"], env)
        self.raw = b""
        self.answered = {CPR: 0, XTVERSION: 0, DA1: 0}
        self.resize(50, 200)

    def resize(self, rows, cols):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def answer(self):
        replies = {
            CPR: b"\x1b[1;1R",
            XTVERSION: (b"\x1bP>|" + self.name.encode() + b"\x1b\\") if self.name else b"",
            DA1: b"\x1b[?62;22c",
        }
        for query, reply in replies.items():
            count = self.raw.count(query)
            for _ in range(self.answered[query], count):
                if reply:
                    os.write(self.fd, reply)
            self.answered[query] = count

    def read(self, timeout):
        if select.select([self.fd], [], [], timeout)[0]:
            try:
                data = os.read(self.fd, 65536)
            except OSError as exc:
                if exc.errno != errno.EIO:
                    raise
                return False
            self.raw += data
            self.answer()
            return bool(data)
        return True

    def until(self, predicate, offset=0):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            if predicate(self.raw[offset:]):
                return
            if not self.read(0.1):
                break
        raise AssertionError(f"expected output not seen; tail: {self.raw[-1300:]!r}")

    def send(self, text):
        for ch in text:
            os.write(self.fd, ch.encode())
            time.sleep(0.015)

    def close(self):
        try:
            status = os.waitpid(self.pid, os.WNOHANG)
            if not status[0]:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
        finally:
            os.close(self.fd)


def text(data):
    return ANSI.sub(b"", data)


def decoded(match):
    return base64.b64decode(match[2], validate=True)


def check(binary, home, case):
    (home / "config.toml").write_text(
        'model = "mock"\nbase_url = "http://127.0.0.1:1/v1"\napi_key = "test"\n'
    )
    (home / "memory").mkdir()
    (home / "memory" / "schedule-choice").write_text("never\n")
    (home / "my image.gif").write_bytes(GIF)
    env, name, shows = {
        "forced under tmux": ({"NANOPI_INLINE_IMAGES": "iterm2", "TMUX": "test"}, None, True),
        "auto: NexusTerm answers XTVERSION": ({}, "NexusTerm 0.9.1", True),
        "auto: no XTVERSION answer": ({}, None, False),
        "tmux without override": ({"TMUX": "test"}, "tmux 3.4", False),
    }[case]
    tui = Tui(binary, home, env, name)
    input_line = re.compile(rb"\x1b\[(\d+);1H(?:\x1b\[[0-9;]*m)*>")
    dock = lambda data: input_line.search(data) and b"(auto)" in text(data)
    try:
        tui.until(dock)
        startup = tui.raw
        logo = image(36, 6).search(startup)
        if shows:
            assert logo and decoded(logo) == LOGO, "logo above the banner"
            assert startup.index(b"\x1b7") < logo.start() < startup.index(b"nanopi")
            print(f"PASS [{case}] logo drawn above the startup banner")
        else:
            assert OSC not in startup, "no image without support"
        if case == "tmux without override":
            assert XTVERSION not in startup, "tmux is decided without asking"
        cpr_at_start = tui.answered[CPR]

        before = len(tui.raw)
        tui.send("/image-test\r")
        if shows:
            tui.until(lambda d: image(40, 6).search(d), before)
            m = image(40, 6).search(tui.raw, before)
            assert decoded(m) == LOGO
            tui.until(dock, m.end())
            print(f"PASS [{case}] /image-test draws the logo above the redrawn dock")

            after = len(tui.raw)
            tui.send('/image "my image.gif"\r')
            tui.until(lambda d: image(4, 2).search(d), after)
            assert decoded(image(4, 2).search(tui.raw, after)) == GIF
            print(f"PASS [{case}] /image sends the named file")

            after = len(tui.raw)
            tui.send("/hotkeys\r")
            tui.until(lambda d: b"Ctrl" in text(d) and dock(d), after)
            after = len(tui.raw)
            tui.resize(32, 90)
            tui.until(dock, after)
            after = len(tui.raw)
            tui.send("Z")
            tui.until(lambda d: b"Z" in text(d), after)
            print(f"PASS [{case}] scrollback, resize and typing afterwards")
        else:
            tui.until(lambda d: b"inline images are off here" in text(d), before)
            print(f"PASS [{case}] stays off and says why")
        assert tui.raw.count(OSC) == (3 if shows else 0)
        assert tui.answered[CPR] == cpr_at_start, "cursor queried while the TUI reads input"
        assert tui.raw.count(XTVERSION) <= 1, "the terminal is asked once, at startup"
    finally:
        tui.close()


if __name__ == "__main__":
    binary = str(Path(sys.argv[1]).resolve())
    for case in ("forced under tmux", "auto: NexusTerm answers XTVERSION",
                 "auto: no XTVERSION answer", "tmux without override"):
        with tempfile.TemporaryDirectory(prefix="nanopi-inline-image-") as directory:
            check(binary, Path(directory), case)
