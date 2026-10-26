#!/usr/bin/env python3
"""Run one opt-in live test with resource admission, sampling and bounded cleanup.

Never changes or terminates an existing model server. Only the newly started test
process group is terminated on pressure. This cannot diagnose historical peaks
or guarantee that a disconnected inference has stopped inside a remote server.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def sample(group=None):
    memory = {}
    for line in Path('/proc/meminfo').read_text().splitlines():
        key, value = line.split(':', 1)
        memory[key] = int(value.strip().split()[0])
    result = {
        'time': time.time(),
        'available_ram_mib': memory['MemAvailable'] // 1024,
        'used_swap_mib': (memory['SwapTotal'] - memory['SwapFree']) // 1024,
        'test_rss_mib': 0,
        'gpus': [],
    }
    if group is not None:
        pages = 0
        for stat in Path('/proc').glob('[0-9]*/stat'):
            try:
                text = stat.read_text()
                fields = text[text.rfind(')') + 2:].split()
                if int(fields[2]) == group:
                    pages += max(0, int(fields[21]))
            except (OSError, ValueError, IndexError):
                continue
        result['test_rss_mib'] = pages * os.sysconf('SC_PAGE_SIZE') // (1024 * 1024)
    try:
        query = subprocess.run([
            'nvidia-smi', '--query-gpu=index,memory.total,memory.used,memory.free,utilization.gpu',
            '--format=csv,noheader,nounits',
        ], capture_output=True, text=True, timeout=3, check=True)
        for line in query.stdout.splitlines():
            index, total, used, free, usage = [int(v.strip()) for v in line.split(',')]
            result['gpus'].append(dict(index=index, total_mib=total, used_mib=used,
                                       free_mib=free, utilization=usage))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        result['gpu_metrics_error'] = type(error).__name__
    return result


def pressure(state, min_ram, min_vram, max_rss, require_gpu=False):
    if state['available_ram_mib'] < min_ram:
        return f"Only {state['available_ram_mib']} MiB RAM available; need {min_ram}"
    if state['test_rss_mib'] > max_rss:
        return f"Test process group uses {state['test_rss_mib']} MiB RSS; limit {max_rss}"
    if require_gpu and (not state['gpus'] or state.get('gpu_metrics_error')):
        return 'GPU metrics unavailable; refusing an unmonitored GPU test'
    for gpu in state['gpus']:
        if gpu['free_mib'] < min_vram:
            return f"GPU {gpu['index']} has {gpu['free_mib']} MiB free; need {min_vram}"
    return None


def stop_test(process):
    # start_new_session gives this test its own group, never the user's server.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--min-free-ram-mib', type=int, default=8192)
    parser.add_argument('--min-free-vram-mib', type=int, default=2048)
    parser.add_argument('--max-test-rss-mib', type=int, default=2048)
    parser.add_argument('--require-gpu-metrics', action='store_true')
    parser.add_argument('--timeout', type=int, default=1200)
    parser.add_argument('--interval', type=float, default=5)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if sys.platform != 'linux':
        parser.error('Live resource monitoring requires Linux/WSL and /proc; no command started')
    # Keep pure resource admission usable by the Windows/macOS test suites.
    # Only executing this Linux-specific monitor needs the POSIX lock module.
    import fcntl

    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command or args.interval < 1 or args.timeout <= 0 or min(
        args.min_free_ram_mib, args.min_free_vram_mib, args.max_test_rss_mib
    ) < 0:
        parser.error('Supply a command and nonnegative resource limits; interval >= 1 second')
    lock = os.open(f'/tmp/werk-worker-live-{os.getuid()}.lock',
                   os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    process = None
    try:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print('BLOCKED: another guarded live test is active', flush=True)
            return 78
        with args.report.open('w') as report:
            started = time.monotonic()
            while True:
                state = sample(process.pid if process else None)
                state['elapsed_seconds'] = round(time.monotonic() - started, 2)
                reason = pressure(state, args.min_free_ram_mib, args.min_free_vram_mib,
                                  args.max_test_rss_mib, args.require_gpu_metrics)
                if state['elapsed_seconds'] >= args.timeout:
                    reason = 'Live-test deadline reached'
                state['stop_reason'] = reason
                report.write(json.dumps(state) + '\n')
                report.flush()
                if reason:
                    print('BLOCKED: ' + reason, flush=True)
                    return 78
                if process is None:
                    process = subprocess.Popen(command, start_new_session=True)
                try:
                    return process.wait(timeout=args.interval)
                except subprocess.TimeoutExpired:
                    pass
    finally:
        if process is not None:
            stop_test(process)
        os.close(lock)


if __name__ == '__main__':
    raise SystemExit(main())
