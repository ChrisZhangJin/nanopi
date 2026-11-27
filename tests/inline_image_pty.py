"""Manual Linux PTY regression: python3 tests/inline_image_pty.py path/to/nanopi."""

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


ASSET = (Path(__file__).resolve().parents[1] / "assets/image-test.png").read_bytes()
OSC = b"\x1b]1337;"
CPR = b"\x1b[6n"
ANSI = re.compile(rb"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07")


class Tui:
    def __init__(self, binary, home, force):
        env = dict(os.environ, NANOPI_HOME=str(home), TERM="xterm-256color",
                   NO_PROXY="127.0.0.1", no_proxy="127.0.0.1", TMUX="test")
        for key in ("NANOPI_INLINE_IMAGES", "TERM_PROGRAM", "LC_TERMINAL",
                    "NEXUSTERM_INLINE_IMAGES", "http_proxy", "https_proxy",
                    "all_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            env.pop(key, None)
        if force:
            env["NANOPI_INLINE_IMAGES"] = "iterm2"
        self.pid, self.fd = pty.fork()
        if not self.pid:
            os.chdir(home)
            os.execve(binary, [binary, "-a"], env)
        self.raw = b""
        self.queries = 0
        self.resize(50, 200)

    def resize(self, rows, cols):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def read(self, timeout):
        if select.select([self.fd], [], [], timeout)[0]:
            try:
                data = os.read(self.fd, 65536)
            except OSError as exc:
                if exc.errno != errno.EIO:
                    raise
                return False
            self.raw += data
            count = self.raw.count(CPR)
            for _ in range(self.queries, count):
                os.write(self.fd, b"\x1b[1;1R")
            self.queries = count
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


def check(binary, home, force):
    (home / "config.toml").write_text(
        'model = "mock"\nbase_url = "http://127.0.0.1:1/v1"\napi_key = "test"\n'
    )
    (home / "memory").mkdir()
    (home / "memory" / "schedule-choice").write_text("never\n")
    gif = base64.b64decode("R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7")
    (home / "my image.gif").write_bytes(gif)
    tui = Tui(binary, home, force)
    input_line = re.compile(rb"\x1b\[(\d+);1H(?:\x1b\[[0-9;]*m)*>")
    dock = lambda data: input_line.search(data) and b"(auto)" in text(data)
    try:
        tui.until(dock)
        startup_queries = tui.queries
        before = len(tui.raw)
        tui.send("/image-test\r")
        if force:
            sequence = re.compile(
                rb"\x1b\[(\d+);3H\x1b\]1337;File=inline=1;size=(\d+);"
                rb"width=48;height=12;preserveAspectRatio=1:([A-Za-z0-9+/=]+)\x07"
            )
            tui.until(sequence.search, before)
            image = sequence.search(tui.raw, before)
            assert int(image[2]) == len(ASSET)
            assert base64.b64decode(image[3], validate=True) == ASSET
            tui.until(dock, image.end())
            assert int(input_line.search(tui.raw, image.end())[1]) >= int(image[1]) + 12
            print("PASS one exact PNG sequence above the redrawn input and footer")

            after = len(tui.raw)
            tui.send('/image "my image.gif"\r')
            local = re.compile(rb"\x1b\]1337;File=inline=1;size=(\d+);width=4;height=2;"
                               rb"preserveAspectRatio=1:([A-Za-z0-9+/=]+)\x07")
            tui.until(local.search, after)
            image = local.search(tui.raw, after)
            assert int(image[1]) == len(gif)
            assert base64.b64decode(image[2], validate=True) == gif
            tui.until(dock, image.end())
            print("PASS /image sends the specified file with spaces in its name")

            # Drive ordinary scrollback after the image without contacting a provider.
            after = len(tui.raw)
            tui.send("/hotkeys\r")
            tui.until(lambda data: b"Ctrl" in text(data) and dock(data), after)
            print("PASS subsequent scrollback and dock output")

            after = len(tui.raw)
            tui.resize(32, 90)
            tui.until(dock, after)
            after = len(tui.raw)
            tui.send("Z")
            tui.until(lambda data: b"Z" in text(data), after)
            print("PASS dock redraw on resize and typing afterwards")
        else:
            tui.until(lambda data: b"inline images are off here (tmux" in text(data)
                      and b"NANOPI_INLINE_IMAGES=iterm2 forces them" in text(data), before)
            print("PASS no override under tmux reports why and how to force")
        assert tui.raw.count(OSC) == 2 * int(force)
        assert tui.queries == startup_queries, "cursor queried while EventStream was active"
    finally:
        tui.close()


if __name__ == "__main__":
    binary = str(Path(sys.argv[1]).resolve())
    for force in (True, False):
        with tempfile.TemporaryDirectory(prefix="nanopi-inline-image-") as directory:
            check(binary, Path(directory), force)
