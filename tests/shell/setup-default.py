"""Run with python3 tests/shell/setup-default.py /path/to/kei on Unix."""

import fcntl
import os
import pty
import select
import signal
import struct
import sys
import tempfile
import termios
from pathlib import Path


def wizard(binary, home, cancel):
    config = home / "my config.toml"
    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))
        os.chdir(home)
        os.execve(binary, [binary, "--config", config, "config", "setup"],
                  {"HOME": str(home), "TERM": "xterm", "PATH": os.defpath})
    pending = b""
    output = bytearray()

    def expect(prompt):
        nonlocal pending
        while prompt not in pending:
            if not select.select([fd], [], [], 10)[0]:
                raise AssertionError(f"waiting for {prompt!r}: {pending[-1000:]!r}")
            chunk = os.read(fd, 8192)
            assert chunk, pending
            output.extend(chunk)
            pending += chunk
        pending = pending.split(prompt, 1)[1]

    try:
        expect(b"Apple ID email")
        assert b"exact, case-sensitive names" in output
        os.write(fd, b"wizard@example.invalid\r")
        for prompt, answer in [
            (b"iCloud region", b"\r"),
            (b"credential store?", b"n"),
            (b"How should kei read the password?", b"\r"),
            (b"Password file path", b"/nonexistent/secret\r"),
            (b"Where should photos be saved?", b"\r"),
            (b"App data directory", b"\r"),
            (b"organized into folders?", b"\r"),
        ]:
            expect(prompt)
            os.write(fd, answer)
        expect(b"entire library or specific albums?")
        if cancel:
            os.write(fd, b"j\r")
            expect(b"Album name")
            os.write(fd, b"\r")
            expect(b"Enter at least one album name.")
            os.kill(pid, signal.SIGINT)
        else:
            os.write(fd, b"\r")
            for prompt, answer in [
                (b"shared or family libraries?", b"\r"),
                (b"Exclude components", b"\r"),
                (b"video part of live photos be named?", b"\r"),
                (b"fixed capture-date range?", b"\r"),
                (b"How will you run kei?", b"\r"),
                (b"Configure advanced options?", b"\r"),
                (b"Look good?", b"\r"),
                (b"first sync right now?", b"\r"),
            ]:
                expect(prompt)
                os.write(fd, answer)
            expect(b"No rush.")
            assert b"Libraries: all; albums: all" in output
            assert b"Destination: ~/Photos/iCloud" in output
            assert b"access not checked" in output
        _, status = os.waitpid(pid, 0)
        pid = None
        if cancel:
            assert not config.exists()
        else:
            assert status == 0 and config.is_file()
            assert not (home / "Photos").exists()
    finally:
        if pid is not None:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(fd)


if __name__ == "__main__":
    def timed_out(signum, frame):
        raise TimeoutError("setup did not exit")

    signal.signal(signal.SIGALRM, timed_out)
    signal.alarm(45)
    binary = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory() as home:
        wizard(binary, Path(home), cancel=True)
        wizard(binary, Path(home), cancel=False)
