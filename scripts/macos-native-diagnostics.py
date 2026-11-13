#!/usr/bin/env python3
"""Read bounded native CI failure evidence; never changes the test result."""

import json
from pathlib import Path
import subprocess
import sys
import time


PROCESSES = ("native entry", "native probe", "native-probe", "lifecycle-probe", "probe", "sandbox-exec")
MAX_OUTPUT = 64 * 1024
MAX_REPORT_SIZE = 2 * 1024 * 1024
WINDOW_SECONDS = 10 * 60
# Restrict system-log messages to these synthetic executables. No global log,
# environment, account configuration, crash images, or thread memory is printed.
_names = ", ".join(json.dumps(name) for name in PROCESSES)
_mentions = " OR ".join(
    f"eventMessage CONTAINS[c] {json.dumps(name)}" for name in PROCESSES if name != "probe"
)
PREDICATE = (
    f"process IN {{{_names}}} OR "
    "((process IN {\"kernel\", \"sandboxd\", \"amfid\", \"ReportCrash\"} OR "
    "subsystem == \"com.apple.sandbox\") AND ("
    f"{_mentions} OR eventMessage CONTAINS[c] \"Sandbox: probe(\"))"
)


def emit_bounded(value):
    value = value or ""
    if isinstance(value, bytes):
        value = value.decode("utf-8", errors="replace")
    print(value[:MAX_OUTPUT], flush=True)
    if len(value) > MAX_OUTPUT:
        print("[diagnostic output truncated]", flush=True)


def print_denials():
    print("macOS native test logs (last 10 minutes; filtered):", flush=True)
    command = [
        "/usr/bin/log", "show", "--last", "10m", "--style", "compact",
        "--info", "--debug", "--predicate", PREDICATE,
    ]
    try:
        result = subprocess.run(
            command, check=False, capture_output=True, text=True, timeout=20,
        )
    except subprocess.TimeoutExpired as error:
        emit_bounded(error.stdout)
        print("Native log collection timed out; test failure remains unchanged.", flush=True)
    except OSError as error:
        print(f"Native log collection unavailable: {error}", flush=True)
    else:
        emit_bounded(result.stdout)
        if result.returncode != 0:
            emit_bounded(result.stderr)
            print(f"Native log collection exited {result.returncode}.", flush=True)


def crash_summary(text):
    # Apple's IPS format has a metadata JSON object followed by a report object:
    # https://developer.apple.com/documentation/xcode/interpreting-the-json-format-of-a-crash-report
    decoder = json.JSONDecoder()
    first, end = decoder.raw_decode(text.lstrip())
    remainder = text.lstrip()[end:].lstrip()
    report = decoder.raw_decode(remainder)[0] if remainder else first
    if not isinstance(report, dict) or report.get("procName") not in PROCESSES:
        return None
    summary = {
        key: report[key]
        for key in ("procName", "pid", "captureTime", "cpuType", "translated", "faultingThread")
        if key in report
    }
    for key, fields in (
        ("exception", ("type", "signal", "subtype", "codes")),
        ("termination", ("namespace", "code", "indicator", "flags", "byProc", "byPid", "reasons")),
    ):
        source = report.get(key)
        if isinstance(source, dict):
            summary[key] = {field: source[field] for field in fields if field in source}
    return summary


def print_crashes(directories, now):
    print("Recent native test crash summaries:", flush=True)
    candidates = []
    for directory in directories:
        try:
            for path in directory.glob("*.ips"):
                if not any(path.name.startswith(name + "-") for name in PROCESSES):
                    continue
                if path.is_symlink():
                    continue
                info = path.stat()
                if now - WINDOW_SECONDS <= info.st_mtime <= now and info.st_size <= MAX_REPORT_SIZE:
                    candidates.append((info.st_mtime, path))
        except OSError as error:
            print(f"Crash report directory unavailable: {error}", flush=True)
    found = 0
    for _, path in sorted(candidates, reverse=True)[:20]:
        try:
            with path.open("r", encoding="utf-8") as report:
                summary = crash_summary(report.read(MAX_REPORT_SIZE))
        except (OSError, ValueError) as error:
            print(f"Cannot summarize {path.name}: {error}", flush=True)
            continue
        if summary is not None:
            found += 1
            emit_bounded(json.dumps(summary, ensure_ascii=True))
    if not found:
        print("No matching recent IPS report available; this does not imply successful execution.", flush=True)


def main():
    if sys.platform != "darwin":
        print("Native macOS diagnostics are only available on macOS.")
        return
    print_denials()
    print_crashes(
        [Path.home() / "Library/Logs/DiagnosticReports", Path("/Library/Logs/DiagnosticReports")],
        time.time(),
    )


if __name__ == "__main__":
    main()
