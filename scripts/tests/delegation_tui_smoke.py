#!/usr/bin/env python3
"""Offline PTY coverage for delegation preferences and fixed project overrides."""
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


binary = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix="ww-delegation-test-") as temp:
    root = Path(temp)
    workspace = root / "workspace"
    workspace.mkdir()
    env = dict(os.environ, TERM="xterm-256color", COLORTERM="truecolor",
               XDG_CONFIG_HOME=str(root / "config"), XDG_STATE_HOME=str(root / "state"))
    env.pop("NO_COLOR", None)
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 130, 0, 0))
    before = termios.tcgetattr(slave)
    screen = Screen()
    proc = None

    def start(store):
        return subprocess.Popen(
            [str(binary), "--workspace", str(workspace), "--store", str(root / store),
             "tui", "--demo"], stdin=slave, stdout=slave, stderr=slave, env=env)

    def wait(token, present=True):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if (token.encode() in screen.text()) == present:
                return
            if select.select([master], [], [], .05)[0]:
                screen.feed(os.read(master, 65536))
            if proc.poll() is not None:
                break
        raise AssertionError((token, screen.text().decode()))

    def click(label):
        for y, row in enumerate(screen.rows):
            line = "".join(row)
            if label in line:
                x = line.index(label) + 1
                os.write(master, f"\x1b[<0;{x};{y+1}M\x1b[<0;{x};{y+1}m".encode())
                return
        raise AssertionError((label, screen.text().decode()))

    def metadata(store):
        with sqlite3.connect(root / store) as db:
            return json.loads(db.execute("SELECT data FROM sessions").fetchone()[0])["metadata"]

    def agents():
        os.write(master, b"\x1b[15~")
        wait("Workspace writes:")
        click("Agents")
        wait("Delegation preference:")

    def choose(level):
        click("[ ] " + level)
        wait("[x] " + level)
        wait("Draft changed.")
        wait("Saved: current session", present=False)
        os.write(master, b"\x13")
        wait("Saved: current session")

    def stop():
        os.write(master, b"\x1b")
        wait("Inspector ", present=False)
        os.write(master, b"\x11")
        assert proc.wait(timeout=10) == 0
        assert termios.tcgetattr(slave) == before

    try:
        proc = start("first.sqlite")
        wait("Conversation")
        agents()
        wait("[x] Active")
        choose("Max")
        assert metadata("first.sqlite")["_werkworker_delegation_preference"] == "max"
        config_path = workspace / ".werkworker/config.json"
        assert "delegation_level" not in json.loads(config_path.read_text())
        policy_path = next((root / "config").glob("werkworker/workspace-access/*.json"))
        assert json.loads(policy_path.read_text())["delegation_level"] == "max"
        os.write(master, b"p")
        wait("Save: current session only")
        choose("Off")
        assert metadata("first.sqlite")["_werkworker_delegation_preference"] == "off", (metadata("first.sqlite"), screen.text().decode())
        assert json.loads(policy_path.read_text())["delegation_level"] == "max"
        # A draft selection is not applied until saved, including after Esc.
        click("[ ] Conservative")
        wait("[x] Conservative")
        os.write(master, b"\x1b")
        wait("Press Esc again to discard")
        os.write(master, b"\x1b")
        wait("Inspector ", present=False)
        agents()
        wait("[x] Off")
        stop()

        # A new session restores the workspace preference, not session-only Off.
        screen = Screen()
        proc = start("second.sqlite")
        wait("Conversation")
        agents()
        wait("[x] Max")
        wait("Effective: Max")
        assert metadata("second.sqlite")["_werkworker_delegation_preference"] == "max"
        stop()

        # An explicit project value beats the restored preference and UI edits.
        config = json.loads(config_path.read_text())
        config["delegation_level"] = "off"
        config_path.write_text(json.dumps(config))
        screen = Screen()
        proc = start("fixed.sqlite")
        wait("Conversation")
        agents()
        wait("[x] Max")
        wait("Effective: Off (fixed by project config)")
        choose("Active")
        wait("Effective delegation: Off")
        assert json.loads(config_path.read_text())["delegation_level"] == "off"
        assert metadata("fixed.sqlite")["project_config"]["delegation_level"] == "off"
        assert metadata("fixed.sqlite")["_werkworker_delegation_preference"] == "active"
        stop()
        print("delegation defaults, draft/discard, session/project persistence and hard override: ok")
    finally:
        if proc is not None and proc.poll() is None:
            proc.kill()
            proc.wait()
        os.close(master)
        os.close(slave)
