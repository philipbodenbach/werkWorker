"""Drive a local Worker RPC process: events, addressed approval, and cancellation.

python3 examples/jsonl_client.py /path/to/werk-worker /path/to/script.json
No external services or credentials are required.
"""
import json
import os
import subprocess
import sys
import tempfile

with tempfile.TemporaryDirectory(prefix="werk-client-") as workspace:
    approved_workspace = workspace + "/workspace"
    os.mkdir(approved_workspace)
    process = subprocess.Popen(
        [sys.argv[1], "--workspace", approved_workspace, "--store", workspace + "/session.sqlite",
         "--script", sys.argv[2], "rpc"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
    )
    sequence = 0
    terminal_seen = False

    def send(method, **params):
        global sequence
        sequence += 1
        process.stdin.write(json.dumps({"id": sequence, "method": method, "params": params}) + "\n")
        process.stdin.flush()
        return sequence

    def response(request_id):
        while True:
            message = json.loads(process.stdout.readline())
            if message.get("id") == request_id:
                if "error" in message:
                    raise RuntimeError(message["error"])
                return message["result"]
            handle_event(message)

    def handle_event(message):
        global terminal_seen
        if message.get("method") == "resync_required":
            raise RuntimeError("Reconnect and subscribe from the last durable cursor")
        if message.get("method") != "event":
            return
        event = message["params"]
        print(json.dumps(event, ensure_ascii=False))
        kind = event["event"]["type"]
        data = event["event"].get("data", {})
        if kind == "run_state" and data["status"] in {"completed", "failed", "cancelled", "interrupted"}:
            terminal_seen = True
        if kind == "approval_requested":
            request = data["request"]
            # This sample client denies all mutations unless changed by its host.
            send("approval.respond", session_id=event["session_id"], run_id=event["run_id"],
                 request_id=request["request_id"], decision="deny", command_id="deny-" + request["request_id"])
        if kind == "interaction_requested":
            request = data["request"]
            send("interaction.respond", session_id=event["session_id"], run_id=event["run_id"],
                 request_id=request["request_id"], answer="Use a local draft", command_id="answer-" + request["request_id"])

    response(send("initialize", protocol_version=1))
    session = response(send("session.create", profile="general-readonly"))["session_id"]
    response(send("events.subscribe", session_id=session, after_cursor=0))
    run = response(send("run.submit", session_id=session, input="Explain the supplied task",
                        command_id="submit-1"))["run_id"]
    try:
        while not terminal_seen:
            line = process.stdout.readline()
            if not line:
                raise RuntimeError("Worker exited before a terminal event")
            message = json.loads(line)
            handle_event(message)
            event = message.get("params", {}).get("event", {})
            if event.get("type") == "run_state" and event["data"]["status"] in {"completed", "failed", "cancelled", "interrupted"}:
                break
    except KeyboardInterrupt:
        response(send("run.cancel", session_id=session, run_id=run, reason="client interrupt", command_id="cancel-1"))
    response(send("shutdown"))
    process.wait(timeout=10)
