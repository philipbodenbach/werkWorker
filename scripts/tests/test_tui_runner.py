import contextlib
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

import run_tui_smokes as runner


@unittest.skipUnless(os.name == "posix", "The PTY smoke suite requires POSIX")
class TuiRunnerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ww-runner-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.output = io.StringIO()

    def script(self, name, source):
        path = self.root / name
        path.write_text(source)
        return path

    def run_scripts(self, scripts, **kwargs):
        with contextlib.redirect_stdout(self.output):
            return runner.run_smokes(sys.executable, scripts=scripts, **kwargs)

    def test_bounded_parallelism_isolation_and_complete_logs(self):
        script = self.script("fixture.py", """
import json, os, sys, time
from pathlib import Path
values = {key: os.environ.get(key) for key in
          ['HOME', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME', 'XDG_DATA_HOME']}
for key, value in values.items():
    if key in ('XDG_CONFIG_HOME', 'XDG_STATE_HOME'):
        assert Path(value).is_dir()
        assert not (Path(value) / 'marker').exists()
        (Path(value) / 'marker').touch()
print(json.dumps(dict(event='start', at=time.monotonic(), env=values)))
print('stderr fixture', file=sys.stderr)
time.sleep(.2)
print(json.dumps(dict(event='end', at=time.monotonic())))
""")
        with mock.patch.dict(os.environ, {"XDG_CONFIG_HOME": str(self.root / "host-config"),
                                          "XDG_STATE_HOME": str(self.root / "host-state"),
                                          "XDG_DATA_HOME": str(self.root / "host-data")}):
            results = self.run_scripts([script] * 5)
        self.assertEqual([result.returncode for result in results], [0] * 5)
        events = [json.loads(line) for line in self.output.getvalue().splitlines()
                  if line.startswith('{')]
        active = peak = 0
        for event in sorted(events, key=lambda event: event['at']):
            active += 1 if event['event'] == 'start' else -1
            peak = max(peak, active)
        self.assertEqual((active, peak), (0, 2))
        environments = [event['env'] for event in events if event['event'] == 'start']
        for key in ('XDG_CONFIG_HOME', 'XDG_STATE_HOME'):
            paths = [env[key] for env in environments]
            self.assertEqual(len(set(paths)), 5)
            self.assertTrue(all(not Path(path).exists() for path in paths))
        self.assertTrue(all(env['HOME'] == os.environ.get('HOME') for env in environments))
        self.assertTrue(all(env['XDG_DATA_HOME'] == str(self.root / 'host-data')
                            for env in environments))
        self.assertFalse((self.root / 'host-config').exists())
        self.assertEqual(self.output.getvalue().count('stderr fixture'), 5)

    def test_failures_preserve_codes_and_do_not_skip_other_scripts(self):
        scripts = [self.script(f"exit_{code}.py", f"print('fixture {code}'); exit({code})")
                   for code in (7, 0, 3, 0)]
        scripts.append(self.script("self_signal.py", "import os, signal; os.kill(os.getpid(), signal.SIGTERM)"))
        results = self.run_scripts(scripts)
        self.assertCountEqual([result.returncode for result in results], [7, 0, 3, 0, -signal.SIGTERM])
        self.assertEqual(sum(result.failed for result in results), 3)
        self.assertIn('2/5 passed', self.output.getvalue())
        for code in (7, 0, 3):
            self.assertIn(f'fixture {code}', self.output.getvalue())
        with mock.patch.object(runner, 'run_smokes', return_value=results):
            self.assertEqual(runner.main([sys.executable]), 1)

    def test_timeout_kills_stubborn_child_and_still_runs_remaining_scripts(self):
        pid_file = self.root / 'pid'
        script = self.script("timeout.py", f"""
import os, signal, time
from pathlib import Path
signal.signal(signal.SIGTERM, signal.SIG_IGN)
Path({str(pid_file)!r}).write_text(str(os.getpid()))
while True: time.sleep(.1)
""")
        success = self.script('success.py', 'print("after timeout")')
        results = self.run_scripts([script, success], timeout=.3, jobs=1)
        self.assertTrue(results[0].timed_out)
        self.assertEqual(results[0].returncode, -signal.SIGKILL)
        self.assertFalse(results[1].failed)
        self.assertIn('TIMEOUT', self.output.getvalue())
        with self.assertRaises(ProcessLookupError):
            os.kill(int(pid_file.read_text()), 0)

    def test_sigterm_cleans_up_and_reaps_script_and_its_child(self):
        pid_file = self.root / 'pids'
        script = self.script('tree.py', f"""
import os, signal, subprocess, sys, time
from pathlib import Path
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
def stop(signum, frame):
    child.wait(timeout=1)
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
Path({str(pid_file)!r}).write_text(f'{{os.getpid()}} {{child.pid}}')
while True: time.sleep(.1)
""")
        harness = self.script('harness.py', f"""
import sys
from pathlib import Path
sys.path.insert(0, {str(Path(runner.__file__).parent)!r})
import run_tui_smokes as runner
runner.SMOKE_SCRIPTS = (Path({str(script)!r}),)
try:
    runner.run_smokes(sys.executable, scripts=runner.SMOKE_SCRIPTS)
except runner.Interrupted as error:
    sys.exit(128 + error.signum)
""")
        process = subprocess.Popen([sys.executable, str(harness)],
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        try:
            deadline = time.monotonic() + 5
            while not pid_file.exists() and time.monotonic() < deadline:
                time.sleep(.02)
            self.assertTrue(pid_file.exists(), 'Fixture child did not start')
            process.terminate()
            output, _ = process.communicate(timeout=5)
            self.assertEqual(process.returncode, 128 + signal.SIGTERM, output)
            self.assertIn('interrupted, exit 0', output)
            for pid in map(int, pid_file.read_text().split()):
                with self.assertRaises(ProcessLookupError):
                    os.kill(pid, 0)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()

    def test_invalid_concurrency_or_timeout_does_not_launch(self):
        with mock.patch.object(runner.subprocess, 'Popen') as launch:
            for kwargs in ({'jobs': 0}, {'jobs': 3}, {'timeout': 0},
                           {'timeout': float('nan')}, {'timeout': float('inf')}):
                with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                    self.run_scripts([], **kwargs)
            launch.assert_not_called()

    def test_launch_error_stops_and_reaps_already_running_script(self):
        script = self.script('waiting.py', 'import time; time.sleep(60)')
        popen = subprocess.Popen
        launched = []

        def launch(*args, **kwargs):
            if launched:
                raise OSError('fixture launch error')
            process = popen(*args, **kwargs)
            launched.append(process)
            return process

        with mock.patch.object(runner.subprocess, 'Popen', side_effect=launch):
            with self.assertRaisesRegex(OSError, 'fixture launch error'):
                self.run_scripts([script, script])
        self.assertIsNotNone(launched[0].returncode)
        with self.assertRaises(ProcessLookupError):
            os.kill(launched[0].pid, 0)

    def test_timeout_is_failure_even_if_script_handles_signal_with_success(self):
        script = self.script('handles_signal.py', """
import signal, sys, time
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
while True: time.sleep(.1)
""")
        results = self.run_scripts([script], timeout=.3)
        self.assertEqual(results[0].returncode, 0)
        self.assertTrue(results[0].failed)


if __name__ == '__main__':
    unittest.main()
