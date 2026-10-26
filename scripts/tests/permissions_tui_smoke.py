#!/usr/bin/env python3
"""Exercise chat permission changes while waiting and persistent approval scopes."""
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


def turn(name=None, args=None, text=""):
    deltas = [{"type": "text", "text": text}]
    if name:
        deltas.append({"type": "tool", "index": 0, "id": "call", "name": name,
                       "arguments": json.dumps(args)})
    deltas += [{"type": "finish", "reason": "tool_calls" if name else "stop"}, {"type": "done"}]
    return {"deltas": deltas, "error": None}


binary = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix="ww-permissions-test-") as temp:
    root = Path(temp)
    workspace = root / "workspace"
    workspace.mkdir()
    # Isolate permission UI behavior from coding acceptance tests.
    harness = root / "harness.json"
    harness.write_text(json.dumps({"coding_checks": False}))
    # Scripted requests use conservative bytes-as-tokens sizing. Give the
    # large-paste UI check room for tool/policy schemas and conversation history.
    limits = root / "limits.json"
    limits.write_text(json.dumps({"context_tokens": 65_536, "continuous": True}))
    script = root / "script.json"
    script.write_text(json.dumps([
        turn("interaction.ask", {"question": "May I write files?", "choices": ["continue"]}),
        turn("files.write", {"path": "one.txt", "content": "one"}),
        turn("files.write", {"path": "two.txt", "content": "two"}),
        turn(text="Both files written"),
        turn(text="Pasted message received"),
    ]))
    grants = root / "grants.json"
    grants.write_text(json.dumps({"workspace_writable": False, "grants": [
        {"tool": name, "version": "1", "resource": ".", "preapproved": False}
        for name in ["files.read", "files.list", "files.search"]
    ]}))
    env = dict(os.environ, TERM="xterm-256color", COLORTERM="truecolor",
               XDG_CONFIG_HOME=str(root / "config"))
    env.pop("NO_COLOR", None)
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 130, 0, 0))
    before = termios.tcgetattr(slave)
    screen = Screen()
    capture = bytearray()

    def start(store, extra):
        return subprocess.Popen(
            [str(binary), "--harness-config", str(harness), "--limits", str(limits),
             "--workspace", str(workspace), "--store", str(root / store)] + extra,
            stdin=slave, stdout=slave, stderr=slave, env=env)

    def wait(token, present=True):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if (token.encode() in screen.text()) == present:
                return
            if select.select([master], [], [], .05)[0]:
                data = os.read(master, 65536)
                capture.extend(data)
                screen.feed(data)
            if proc.poll() is not None:
                break
        raise AssertionError((token, screen.text().decode()))

    def state(store):
        with sqlite3.connect(root / store) as db:
            return json.loads(db.execute("SELECT data FROM sessions").fetchone()[0])

    def click(label):
        for y, row in enumerate(screen.rows):
            line = "".join(row)
            if label in line:
                x = line.index(label) + 1
                os.write(master, f"\x1b[<0;{x};{y+1}M\x1b[<0;{x};{y+1}m".encode())
                return
        raise AssertionError(("No clickable label", label, screen.text().decode()))

    proc = start("state.sqlite", ["--script", str(script), "--grants", str(grants), "tui", "create files",
                                   "--profile", "automation-basic"])
    try:
        wait("May I write files?")
        assert not state("state.sqlite")["policy"]["workspace_writable"]
        assert "process.run" not in state("state.sqlite")["profile"]["tools"]
        os.write(master, b"\x1b[15~")
        wait("Workspace writes: deny")
        click("Workspace writes:")
        wait("Workspace writes: ask")
        click("Sandbox processes:")
        wait("Sandbox processes: ask")
        assert not state("state.sqlite")["policy"]["workspace_writable"], "Draft must not apply silently"
        # A profile selection must not undo explicitly enabled process access.
        click("Profiles")
        wait("[x] automation-basic")
        click("[x] automation-basic")
        click("Permissions")
        wait("Sandbox processes: ask")
        os.write(master, b"\x1b")
        wait("Press Esc again to discard")
        # Ctrl-S must also work after clicking/selecting the details text.
        click("Draft profile:")
        os.write(master, b"\x13")
        wait("Saved: current session and project defaults.")
        assert state("state.sqlite")["policy"]["workspace_writable"]
        assert "process.run" in state("state.sqlite")["profile"]["tools"]
        click("Apply & close")
        wait("Settings · Saved:")
        assert state("state.sqlite")["policy"]["workspace_writable"]
        assert "process.run" in state("state.sqlite")["profile"]["tools"]
        policy_path = next((root / "config").glob("werkworker/workspace-access/*.json"))
        assert json.loads(policy_path.read_text())["policy"]["tool_permissions"]["process.run"] == "ask"
        os.write(master, b"/permissions\r")
        wait("Workspace writes: explicit approval required")
        wait("Process tool: sandboxed; ask")
        wait("Inspector ")
        os.write(master, b"\x1b")
        wait("Inspector ", present=False)
        # Repeat the reported keyboard-only path and verify both persisted values.
        os.write(master, b"\x1b[15~")
        wait("Workspace writes: ask")
        os.write(master, b"\x1b[B \x1b[B \x13")
        wait("Workspace writes: allow")
        wait("Sandbox processes: allow")
        wait("Saved: current session and project defaults.")
        assert state("state.sqlite")["policy"]["tool_permissions"]["process.run"] == "allow"
        assert json.loads(policy_path.read_text())["policy"]["tool_permissions"]["files.write"] == "allow"
        # Restore ask before testing per-action approvals.
        os.write(master, b"\x1b[A  \x1b[B  \x13")
        wait("Workspace writes: ask")
        wait("Sandbox processes: ask")
        wait("Saved: current session and project defaults.")
        os.write(master, b"\x1b")
        wait("Inspector ", present=False)
        os.write(master, b"/permissions writes allow session\r")
        wait("Saved: current session.")
        assert state("state.sqlite")["policy"]["tool_permissions"]["files.write"] == "allow"
        assert json.loads(policy_path.read_text())["policy"]["tool_permissions"]["files.write"] == "ask"
        # Changes are host commands, not an answer to the model's question.
        assert state("state.sqlite")["runs"][0]["status"] == "waiting_input"
        os.write(master, b"/permissions mode manual project\r")
        wait("Permissions · /permissions mode manual project")
        policy_path = next((root / "config").glob("werkworker/workspace-access/*.json"))
        saved = json.loads(policy_path.read_text())["policy"]
        assert saved["tool_permissions"]["files.write"] == "ask"
        assert saved["tool_permissions"]["files.read"] == "allow"
        assert saved["tool_permissions"]["process.run"] == "ask"
        os.write(master, b"continue\r")
        wait("Allow files.write in workspace")
        wait("save for project")
        assert not (workspace / "one.txt").exists()
        # Approval initially selects Deny; two Down keys select project.
        os.write(master, b"\x1b[B\x1b[B\r")
        wait("Both files written")
        assert (workspace / "one.txt").read_text() == "one"
        assert (workspace / "two.txt").read_text() == "two"
        policy = json.loads(policy_path.read_text())["policy"]
        assert policy["remembered_grants"][0]["tool"] == "files.write"
        assert policy["remembered_grants"][0]["resource"] == "."
        assert policy["remembered_grants"][0]["arguments_hash"] is None
        assert sum(i.get("approval") is not None for i in state("state.sqlite")["runs"][0]["invocations"]) == 1
        # Large paste blocks stay compact in the TUI, but the model receives all text.
        first = "\n".join(f"Large pasted line {n}: Grüße 世界" for n in range(24))
        # Editor unit tests cover the full 64 KiB input capacity.
        second = "very long single line " + "x" * 16_000
        os.write(master, b"Read: \x1b[200~" + first.encode() + b"\x1b[201~")
        wait("[Pasted text #1 +24 lines]")
        os.write(master, b" and \x1b[200~" + second.encode() + b"\x1b[201~")
        wait("[Pasted text #2 +1 lines]")
        assert b"Large pasted line 23" not in screen.text()
        assert not any(m["content"].startswith("Read: ") for m in state("state.sqlite")["messages"] if m["role"] == "user")
        os.write(master, b"\r")
        wait("Pasted message received")
        users = [m["content"] for m in state("state.sqlite")["messages"] if m["role"] == "user"]
        assert users[-1] == f"Read: {first} and {second}", "Collapsed markers must not replace the actual prompt"
        # Granular settings are keyboard-editable and visibly selected.
        os.write(master, b"\x1b[15~\x1b[B")
        wait("Workspace writes:")
        wait("Draft profile:")
        wait("Inspector ")
        os.write(master, b" ")
        os.write(master, b"\x1b[21~")  # F10 applies and closes without relying on Ctrl-S.
        wait("Inspector ", present=False)
        assert "files.write" not in [g["tool"] for g in json.loads(policy_path.read_text())["policy"]["remembered_grants"]]
        assert b"48;2;0;220;255" in capture, "Selection background must be visible"
        if os.environ.get("WERK_WORKER_COLOR_CAPTURE"):
            Path(os.environ["WERK_WORKER_COLOR_CAPTURE"]).write_bytes(capture)
        os.write(master, b"\x11")
        assert proc.wait(timeout=10) == 0
        assert termios.tcgetattr(slave) == before
        screen = Screen()
        proc = start("restart.sqlite", ["tui", "--demo"])
        wait("Conversation")
        restored = state("restart.sqlite")
        assert restored["policy"]["tool_permissions"] == json.loads(policy_path.read_text())["policy"]["tool_permissions"]
        assert "files.write" in restored["profile"]["tools"]
        os.write(master, b"\x11")
        assert proc.wait(timeout=10) == 0
        print("automation profile, mouse controls, Ctrl-S/F10, unsaved guard, complete collapsed pastes, saved permissions and restart: ok")
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        os.close(master)
        os.close(slave)
