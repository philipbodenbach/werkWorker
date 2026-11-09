#!/usr/bin/env python3
"""Run the Linux PTY smoke suite with at most two isolated script processes."""

import argparse
from dataclasses import dataclass
import math
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time


SMOKE_SCRIPTS = tuple(Path(__file__).with_name(name) for name in (
    # Start the longest fixtures first to keep both slots occupied.
    "setup_smoke.py",
    "selection_tui_smoke.py",
    "tui_smoke.py",
    "permissions_tui_smoke.py",
    "sessions_tui_smoke.py",
    "delegation_tui_smoke.py",
    "subagents_tui_smoke.py",
    "effort_tui_smoke.py",
))


@dataclass
class Result:
    script: Path
    returncode: int
    elapsed: float
    timed_out: bool = False

    @property
    def failed(self):
        return self.returncode != 0 or self.timed_out


class Interrupted(Exception):
    def __init__(self, signum):
        self.signum = signum


def stop_process(process):
    """Stop the whole smoke process group and reap its direct child."""
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        pass
    finally:
        # A failed script can leave its worker (or layout subprocess) running.
        # Kill those even when the script itself has already exited.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def run_smokes(binary, *, scripts=SMOKE_SCRIPTS, jobs=2, timeout=180):
    if os.name != "posix":
        raise ValueError("The PTY smoke suite requires a POSIX platform")
    if jobs not in (1, 2) or not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("Use one or two jobs and a positive finite timeout")

    binary = Path(binary).resolve()
    scripts = iter(scripts)
    results = []
    active = []
    interrupted = None
    started = time.monotonic()

    def on_signal(signum, frame):
        nonlocal interrupted
        # Record the signal without interrupting Popen before it is tracked.
        interrupted = signum

    handlers = {sig: signal.signal(sig, on_signal)
                for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        with tempfile.TemporaryDirectory(prefix="ww-tui-suite-") as temporary:
            pending = True
            try:
                while pending or active:
                    if interrupted is not None:
                        raise Interrupted(interrupted)
                    while pending and len(active) < jobs and interrupted is None:
                        script = next(scripts, None)
                        if script is None:
                            pending = False
                            break
                        script = Path(script).resolve()
                        root = Path(tempfile.mkdtemp(prefix=script.stem + "-", dir=temporary))
                        env = os.environ.copy()
                        # XDG_DATA_HOME/HOME locate installed sandbox runtimes;
                        # these fixtures write their stores/workspaces to temp.
                        for variable, directory in (("XDG_CONFIG_HOME", "config"),
                                                    ("XDG_STATE_HOME", "state")):
                            path = root / directory
                            path.mkdir()
                            env[variable] = str(path)
                        log = (root / "output.log").open("w+", encoding="utf-8", errors="replace")
                        try:
                            process = subprocess.Popen(
                                [sys.executable, "-u", str(script), str(binary)],
                                env=env, stdout=log, stderr=subprocess.STDOUT,
                                start_new_session=True,
                            )
                        except BaseException:
                            log.close()
                            raise
                        active.append((script, process, log, time.monotonic()))
                        print(f"START {script.name}", flush=True)
                    for item in active[:]:
                        script, process, log, start = item
                        returncode = process.poll()
                        timed_out = returncode is None and time.monotonic() - start >= timeout
                        if returncode is None and not timed_out:
                            continue
                        stop_process(process)
                        result = Result(script, process.returncode, time.monotonic() - start, timed_out)
                        results.append(result)
                        print(f"=== {script.name}: exit {result.returncode}, "
                              f"{result.elapsed:.2f}s{' (TIMEOUT)' if timed_out else ''} ===",
                              flush=True)
                        log.seek(0)
                        shutil.copyfileobj(log, sys.stdout)
                        print(f"=== END {script.name} ===", flush=True)
                        log.close()
                        active.remove(item)
                    if active:
                        time.sleep(.05)
                if interrupted is not None:
                    raise Interrupted(interrupted)
            finally:
                # Also covers launch errors, interrupted runs and log failures.
                for script, process, log, start in active:
                    stop_process(process)
                for script, process, log, start in active:
                    try:
                        print(f"=== {script.name}: interrupted, exit {process.returncode} ===",
                              flush=True)
                        log.seek(0)
                        shutil.copyfileobj(log, sys.stdout)
                        print(f"=== END {script.name} ===", flush=True)
                    finally:
                        log.close()
    finally:
        for sig, handler in handlers.items():
            signal.signal(sig, handler)

    failed = sum(result.failed for result in results)
    print(f"TUI smoke suite: {len(results) - failed}/{len(results)} passed, "
          f"{time.monotonic() - started:.2f}s total ({jobs} jobs)", flush=True)
    return results


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="Path to the built ww binary")
    parser.add_argument("--jobs", type=int, choices=(1, 2), default=2)
    parser.add_argument("--timeout", type=float, default=180,
                        help="Timeout in seconds per script (default: 180)")
    args = parser.parse_args(argv)
    if not args.binary.is_file():
        parser.error(f"Worker binary does not exist: {args.binary}")
    try:
        results = run_smokes(args.binary, jobs=args.jobs, timeout=args.timeout)
    except Interrupted as error:
        print(f"TUI smoke suite interrupted by signal {error.signum}", file=sys.stderr)
        return 128 + error.signum
    except (OSError, ValueError) as error:
        parser.error(str(error))
    return int(any(result.failed for result in results))


if __name__ == "__main__":
    sys.exit(main())
