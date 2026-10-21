#!/usr/bin/env python3
"""A click/selection during inference must not hide the next approval."""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import sqlite3
import struct
import subprocess
import sys
import tempfile
import termios
import time

from tui_support import Screen


def turn(name=None, args=None, text="", delay=0):
    deltas = [{"type": "text", "text": text}]
    if name:
        deltas.append({"type": "tool", "index": 0, "id": "call", "name": name,
                       "arguments": json.dumps(args)})
    deltas += [{"type": "finish", "reason": "tool_calls" if name else "stop"},
               {"type": "done"}]
    return {"deltas": deltas, "error": None, "delay_ms": delay}


binary = Path(sys.argv[1]).resolve()
if len(sys.argv) == 2:
    for layout in ("compact", "cockpit"):
        subprocess.run([sys.executable, __file__, str(binary), layout], check=True)
    sys.exit(0)
layout = sys.argv[2]
with tempfile.TemporaryDirectory(prefix="ww-selection-test-") as temp:
    root = Path(temp)
    workspace = root / "workspace"
    workspace.mkdir()
    config = root / "config" / "werkworker"
    config.mkdir(parents=True)
    (config / "ui.json").write_text(json.dumps({"layout": layout}))
    script = root / "script.json"
    script.write_text(json.dumps([
        turn("files.list", {"path": "."}, delay=500),
        turn("files.list", {"path": "."}, delay=500),
        turn(text="Completed after visible approvals"),
    ]))
    grants = root / "grants.json"
    grants.write_text(json.dumps({"mode": "ask", "grants": [
        {"tool": "files.list", "version": "1", "resource": ".", "preapproved": False}
    ]}))
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 130, 0, 0))
    env = dict(os.environ, TERM="xterm-256color", COLORTERM="truecolor",
               XDG_CONFIG_HOME=str(root / "config"))
    env.pop("NO_COLOR", None)
    proc = subprocess.Popen([
        str(binary), "--workspace", str(workspace), "--store", str(root / "state.sqlite"),
        "--script", str(script), "--grants", str(grants),
        "tui", "selection regression task", "--profile", "coding-basic",
    ], stdin=slave, stdout=slave, stderr=slave, env=env)
    screen = Screen()

    def wait(token, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if token.encode() in screen.text():
                return
            if select.select([master], [], [], .05)[0]:
                screen.feed(os.read(master, 65536))
            if proc.poll() is not None:
                break
        raise AssertionError((token, screen.text().decode()))

    def state():
        with sqlite3.connect(root / "state.sqlite") as db:
            return json.loads(db.execute("SELECT data FROM sessions").fetchone()[0])["runs"][0]

    try:
        wait("waiting for model")
        # A normal click in an empty part of the conversation used to freeze
        # the complete frame forever, even after the engine requested approval.
        os.write(master, b"\x1b[<0;30;12M\x1b[<0;30;12m")
        wait("Allow files.list in workspace")
        wait("WaitingApproval")
        assert state()["status"] == "waiting_approval"
        assert state()["invocations"][0]["result"] is None
        os.write(master, b"\x19")  # Explicitly approve once, not for the session.
        wait("waiting for model")
        # A real drag must retain copyable text but expose the next request.
        y = next(i for i, row in enumerate(screen.rows)
                 if "selection regression task" in "".join(row))
        x = "".join(screen.rows[y]).index("selection regression task")
        os.write(master, (f"\x1b[<0;{x+1};{y+1}M"
                          f"\x1b[<32;{x+10};{y+1}M"
                          f"\x1b[<0;{x+10};{y+1}m").encode())
        wait("Allow files.list in workspace")
        assert state()["invocations"][1]["result"] is None
        os.write(master, b"\x19")
        wait("Completed after visible approvals")
        wait("[Completed]")
        assert state()["status"] == "completed"
        os.write(master, b"\x11")
        proc.wait(timeout=5)
        assert proc.returncode == 0
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        os.close(master)
        os.close(slave)
print(f"PTY ({layout}): clicks and selections preserve live status and show both approvals; no implicit approval.")
