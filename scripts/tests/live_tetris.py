#!/usr/bin/env python3
"""Live one-prompt coding acceptance with an existing local Werk connection.

Creates a fresh temporary workspace; grants file/process access only there.
Uses the source project's model, context and output settings without modifying
the project. Credentials stay in the child environment, never in test reports.
No scripted model responses or automatic answers to clarification questions.
"""
import argparse
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--project", type=Path, required=True,
                    help="Existing workspace with .werkworker connection settings")
parser.add_argument("--binary", type=Path, default=Path("target/debug/ww"))
parser.add_argument("--secret-env", help="Read the API key from this environment variable")
parser.add_argument("--timeout", type=int, default=950)
args = parser.parse_args()
node = shutil.which("node")
if not node:
    parser.error("Node.js is required to syntax-check the generated JavaScript")


class InlineScripts(HTMLParser):
    def __init__(self):
        super().__init__()
        self.scripts = []
        self.current = None

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "script" and attrs.get("type", "") in (
            "", "text/javascript", "application/javascript", "module"
        ):
            if attrs.get("src"):
                raise RuntimeError("The requested self-contained game uses an external script")
            self.current = [attrs.get("type") == "module", ""]

    def handle_data(self, data):
        if self.current is not None:
            self.current[1] += data

    def handle_endtag(self, tag):
        if tag == "script" and self.current is not None:
            self.scripts.append(self.current)
            self.current = None


root = Path(tempfile.mkdtemp(prefix="werkworker-tetris-acceptance-"))
workspace = root / "workspace"
workspace.mkdir()
print("ARTIFACT_ROOT", root, flush=True)
source = args.project.resolve() / ".werkworker"
project = json.loads((source / "config.json").read_text())
config = json.loads((source / "werk-config.json").read_text())
config["model"] = project["model"]
config["capabilities"].update(tool_calling="supported", streaming="supported",
                              context_tokens=project.get("context_tokens"))
(root / "werk.json").write_text(json.dumps(config))
tools = ["files.read", "files.list", "files.search", "files.write", "files.edit", "process.run"]
policy = {
    "workspace_writable": True,
    "grants": [{"tool": t, "version": "1", "resource": "workspace" if t == "process.run" else ".",
                "arguments_hash": None, "expires_at_ms": None, "preapproved": True} for t in tools],
    "hard_denied_tools": ["delegate_task", "agents.status", "agents.wait", "agents.cancel",
                          "agents.message", "process.start", "process.status", "process.cancel"],
}
(root / "grants.json").write_text(json.dumps(policy))
(root / "execution.json").write_text(json.dumps({"max_model_requests": 1, "max_subagents": 0}))
env = dict(os.environ)
env["WERK_TEST_API_KEY"] = (
    os.environ[args.secret_env] if args.secret_env
    else json.loads((source / "secrets.json").read_text())["api_key"]
)
proc = subprocess.Popen([
    str(args.binary.resolve()),
    "--workspace", str(workspace), "--store", str(root / "session.sqlite"),
    "--werk-config", str(root / "werk.json"), "--secret-env", "WERK_TEST_API_KEY",
    "--grants", str(root / "grants.json"), "--execution-config", str(root / "execution.json"), "rpc",
], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=(root / "stderr.log").open("w"),
    text=True, env=env)
del env["WERK_TEST_API_KEY"]
incoming = queue.Queue()
def read():
    for line in proc.stdout:
        incoming.put(json.loads(line))
    incoming.put(None)
threading.Thread(target=read, daemon=True).start()
sequence = 0
def rpc(method, params=None):
    global sequence
    sequence += 1
    request_id = sequence
    proc.stdin.write(json.dumps({"id": request_id, "method": method, "params": params or {}}) + "\n")
    proc.stdin.flush()
    while True:
        result = incoming.get(timeout=30)
        if result is None:
            raise RuntimeError("Worker exited: " + (root / "stderr.log").read_text())
        if result.get("id") == request_id:
            if "error" in result:
                raise RuntimeError(json.dumps(result["error"]))
            return result["result"]

started = time.monotonic()
try:
    rpc("initialize", {"protocol_version": 1})
    session = rpc("session.create", {"profile": "coding-basic"})["session_id"]
    prompt = (
        "Erstelle ein Retro-Tetris-Spiel in einer einzigen HTML-Datei. Während des Spielens "
        "soll sich animiertes Filmkorn über das Bild bewegen. Ergänze Spielgeräusche, optional "
        "zuschaltbare Musik sowie ansprechende Grafikeffekte und Animationen. "
        "Ein Referenzbild ist hier nicht verfügbar; verwende einen dunklen Neon-Retro-Stil. "
        "Speichere das fertige Spiel als tetris.html."
    )
    limits = {"reserve_output_tokens": project["output_tokens"]}
    if project.get("context_tokens") is not None:
        limits["context_tokens"] = project["context_tokens"]
    run = rpc("run.submit", {
        "session_id": session, "command_id": "tetris-acceptance", "input": prompt,
        "limits": limits,
    })["run_id"]
    cursor = 0
    failure = None
    while True:
        state = rpc("session.open", {"session_id": session})
        for item in state["events"]:
            if item["cursor"] <= cursor:
                continue
            cursor = item["cursor"]
            event = item["event"]
            kind = event["type"]
            if kind in ["model_request", "tool_prepared", "attempt_failed", "diagnostic", "run_state"]:
                print(round(time.monotonic() - started, 1), json.dumps(event), flush=True)
            if kind == "tool_completed":
                result = event["data"]["result"]
                print(round(time.monotonic() - started, 1), "tool result",
                      result["status"], result.get("error_code"),
                      str(result.get("data", {}))[:180], flush=True)
        status = next(r for r in state["runs"] if r["id"] == run)
        if status["status"] in ["completed", "failed", "cancelled", "interrupted"]:
            (root / "session.json").write_text(json.dumps(state, indent=2))
            print("RESULT", status["status"], status.get("error"), "metrics", status["metrics"], flush=True)
            print("FILES", [(p.name, p.stat().st_size) for p in workspace.iterdir()], flush=True)
            if status["status"] != "completed":
                failure = "The live coding run did not complete"
            elif not (workspace / "tetris.html").is_file():
                failure = "Run reported completion without the requested HTML file"
            elif set(workspace.rglob("*.html")) != {workspace / "tetris.html"}:
                failure = "Run split the game across multiple HTML files"
            break
        for invocation in status["invocations"]:
            question = invocation.get("interaction")
            if question and not question.get("answer"):
                (root / "session.json").write_text(json.dumps(state, indent=2))
                raise RuntimeError("Unexpected clarification: " + question["question"])
        if time.monotonic() - started > args.timeout:
            rpc("run.cancel", {"session_id": session, "run_id": run,
                              "command_id": "timeout", "reason": "Acceptance test deadline"})
            raise TimeoutError("Live acceptance exceeded deadline")
        time.sleep(1)
    rpc("shutdown")
    if failure:
        raise RuntimeError(failure)
    parsed = InlineScripts()
    parsed.feed((workspace / "tetris.html").read_text())
    if not parsed.scripts:
        raise RuntimeError("Generated game contains no inline JavaScript")
    for module, source_code in parsed.scripts:
        checked = subprocess.run(
            [node, "--check", "--input-type=" + ("module" if module else "commonjs")],
            input=source_code, text=True, capture_output=True,
        )
        if checked.returncode:
            (root / "syntax-error.txt").write_text(checked.stderr)
            raise RuntimeError("Generated JavaScript is invalid; see syntax-error.txt")
    print("PASS: one prompt completed; one HTML file; JavaScript syntax valid. "
          "Browser gameplay still requires verification.", flush=True)
finally:
    proc.stdin.close()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.terminate()
