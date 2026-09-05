"""Run against a TUI build: python3 crates/arsy-cli/tests/tui_smoke.py target/debug/arsy.

Uses a real PTY and a provider that always reports unavailable. No credentials,
provider traffic, or user configuration writes are required: HOME points at the
temporary workspace, so a remembered model can neither be read nor written.

The PTY is drained on a thread for as long as the child lives. A pseudo-terminal
holds about a kilobyte before it blocks its writer, and ARSY repaints its input
block on every keystroke, so a reader that pauses between writes stalls the
process it is testing rather than the test.
"""
import json
import os
from pathlib import Path
import pty
import select
import subprocess
import sys
import tempfile
import termios
import threading
import time


class Terminal:
    """A PTY whose output is drained continuously into one buffer."""

    def __init__(self, master, child):
        self.master = master
        self.child = child
        self.received = b""
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.reader = threading.Thread(target=self._drain, daemon=True)
        self.reader.start()

    def _drain(self):
        while not self.stop.is_set():
            if select.select([self.master], [], [], 0.05)[0]:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError:
                    return
                if not chunk:
                    return
                with self.lock:
                    self.received += chunk

    def send(self, keys):
        os.write(self.master, keys)

    def expect(self, text, timeout=10):
        deadline = time.monotonic() + timeout
        exited = None
        while time.monotonic() < deadline:
            with self.lock:
                received = self.received
            if text.encode() in received:
                return
            # A build without the `tui` feature refuses the bare invocation.
            # Any workspace-wide cargo command rebuilds target/debug/arsy
            # without it, so this is what a stale binary looks like and it is
            # worth naming instead of spending the timeout on it.
            if b"ARSY-SCH-1003" in received:
                raise AssertionError(
                    "the binary was built without the TUI: run "
                    "`cargo build -p arsy-cli --features tui` and retry"
                )
            # One more pass after the child exits, so its final bytes are read.
            if exited:
                break
            exited = self.child.poll() is not None
            time.sleep(0.02)
        with self.lock:
            tail = self.received[-2000:]
        raise AssertionError(f"missing {text!r}: {tail!r}")

    def close(self):
        self.stop.set()
        self.reader.join(timeout=2)


def workspace(root):
    (root / "bin").mkdir()
    (root / "bin/codex").symlink_to("/usr/bin/false")
    (root / ".claude").mkdir()
    (root / ".mcp.json").write_text(json.dumps({
        "mcpServers": {"docs": {"command": "never-execute-this", "args": ["--serve"]}}
    }))
    (root / ".claude/settings.json").write_text(json.dumps({
        "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "never-execute-this"}]}]}
    }))


def non_interactive(binary, root):
    listed = subprocess.run([str(binary), "--workspace", str(root), "mcp", "list", "--output", "json"], capture_output=True, text=True, timeout=10)
    assert listed.returncode == 0, listed.stderr
    records = [json.loads(line) for line in listed.stdout.splitlines()]
    assert len(records) == 1 and records[0]["type"] == "result"
    assert records[0]["payload"]["entries"][0]["name"] == "docs"
    missing = subprocess.run([str(binary), "--workspace", str(root), "mcp", "show", "missing", "--output", "json"], capture_output=True, text=True, timeout=10)
    assert missing.returncode == 2
    assert [json.loads(line)["type"] for line in missing.stdout.splitlines()] == ["diagnostic", "result"]


def main():
    binary = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix="arsy-tui-") as directory:
        root = Path(directory)
        workspace(root)
        non_interactive(binary, root)

        master, slave = pty.openpty()
        original = termios.tcgetattr(slave)
        environment = dict(
            os.environ,
            PATH=f"{root / 'bin'}:{os.environ['PATH']}",
            HOME=str(root),
            XDG_CONFIG_HOME=str(root / "config"),
        )
        child = subprocess.Popen(
            [str(binary), "--workspace", str(root), "--no-color"],
            stdin=slave, stdout=slave, stderr=slave, env=environment,
        )
        terminal = Terminal(master, child)
        try:
            terminal.expect("Provider unavailable")
            terminal.send(b"/help\r")
            terminal.expect("Up/Down: input history")

            # `/` opens the command menu; Down moves the marker and Enter takes
            # the highlighted command, which a second Enter then sends.
            terminal.send(b"/")
            terminal.expect("› /model")
            terminal.send(b"\x1b[B\x1b[B")
            terminal.expect("› /hooks")
            terminal.send(b"\r\r")
            terminal.expect("1 hook declared; none loaded")

            # Inspection reports what a connection would run, never the record.
            terminal.send(b"/mcp\r")
            terminal.expect("1 MCP server declared; none loaded")
            terminal.expect("docs · stdio · not loaded")
            terminal.expect("command: never-execute-this --serve")
            terminal.send(b"/hooks --event Stop\r")
            terminal.expect("Stop · * · not loaded")
            terminal.expect("lifecycle: after_turn")
            terminal.send(b"/hooks --event NoSuchEvent\r")
            terminal.expect("Filters applied: --event NoSuchEvent")
            terminal.send(b"/unknown\r")
            terminal.expect("Unknown command")

            # A mistyped answer never becomes the remembered model, and leaving
            # the picker returns to the task prompt instead of ending the session.
            terminal.send(b"/model\r")
            terminal.send(b"/model gpt-5\r")
            terminal.expect("is a command")
            terminal.send(b"\x03")
            terminal.expect("Model unchanged")
            assert not (root / "Library/Application Support/ARSY/model").exists()
            assert not (root / "config/arsy/model").exists()

            terminal.send(b"\x1b[200~/quit\n\x1b[201~")
            time.sleep(0.15)
            assert child.poll() is None, "pasted newline must not submit /quit"
            terminal.send(b"\x03/quit\r")
            assert child.wait(timeout=5) == 0
            assert termios.tcgetattr(slave) == original, "terminal modes were not restored"
            assert not (root / ".arsy/sessions.sqlite3").exists(), "inspection created a session"
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            terminal.close()
            os.close(master)
            os.close(slave)
    print("PASS: JSON success/failure, PTY inspection, filtering, help, model-picker rejection and cancel, safe paste, exit, terminal restoration")


if __name__ == "__main__":
    main()
