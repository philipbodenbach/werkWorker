"""Check CI orchestration; native tests themselves always use the real backend."""

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


WORKFLOW = (
    Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml"
).read_text()
STEPS = (
    ("Test native platform integration", 3, "windows_sandbox"),
    ("Check native SDK and lint", 4, "examples/sdk-consumer/Cargo.toml"),
)
DIAGNOSTICS_PATH = Path(__file__).resolve().parents[1] / "macos-native-diagnostics.py"
_spec = importlib.util.spec_from_file_location("macos_native_diagnostics", DIAGNOSTICS_PATH)
DIAGNOSTICS = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(DIAGNOSTICS)


def step_for(name):
    return WORKFLOW.split(f"      - name: {name}\n", 1)[1].split("\n      - name:", 1)[0]


def script_for(name):
    lines = []
    for line in step_for(name).split("        run: |\n", 1)[1].splitlines():
        if line.strip() and not line.startswith("          "):
            break
        lines.append(line[10:])
    return "\n".join(lines)


class NativeCiFailureCollection(unittest.TestCase):
    def test_native_checks_use_python_and_continue_after_a_failed_step(self):
        for name, _, _ in STEPS:
            step = step_for(name)
            self.assertEqual(
                step.splitlines()[0].strip(),
                "if: ${{ !cancelled() && runner.os != 'Linux' }}",
            )
            self.assertIn("        shell: python\n", step)
            self.assertNotIn("        shell: bash\n", step)
        self.assertIn("--no-fail-fast", script_for("Test native platform integration"))
        # Include nested authentication tests as well as the backend's direct tests.
        self.assertIn('"runtime::macos::"', script_for("Test native platform integration"))
        self.assertIn('          REQUIRE_SANDBOX: "1"', step_for("Test native platform integration"))
        self.assertIn(
            "WERK_REQUIRE_CUSTOM_MACH_HOST_PORT: ${{ runner.os == 'macOS' && '1' || '0' }}",
            step_for("Test native platform integration"),
        )

    def check_script(self, name, count, final_check, failed_calls, spawn_error=False):
        calls = []
        environment = dict(os.environ)

        def run(command, **kwargs):
            # No shell expansion or PATH override: preserve the MSVC environment.
            self.assertIsInstance(command, list)
            self.assertEqual(kwargs, {"check": False})
            self.assertEqual(dict(os.environ), environment)
            calls.append(command)
            if len(calls) in failed_calls:
                if spawn_error:
                    raise FileNotFoundError("missing Cargo fixture")
                return subprocess.CompletedProcess(command, 101)
            return subprocess.CompletedProcess(command, 0)

        # Substitute Cargo only to test error collection, not sandbox enforcement.
        with patch("subprocess.run", side_effect=run):
            with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as result:
                    exec(compile(script_for(name), name, "exec"), {"__name__": "__main__"})
        self.assertEqual(len(calls), count)
        self.assertIn(final_check, calls[-1])
        self.assertTrue(all(command[0] == "cargo" for command in calls))
        self.assertEqual(result.exception.code, 1 if failed_calls else 0)

    def test_every_command_runs_and_each_failure_is_retained(self):
        for name, count, final_check in STEPS:
            failures = [set(), *({index} for index in range(1, count + 1)), set(range(1, count + 1))]
            for failed_calls in failures:
                with self.subTest(step=name, failed_calls=failed_calls):
                    self.check_script(name, count, final_check, failed_calls)

    def test_failed_spawn_does_not_hide_later_checks(self):
        for name, count, final_check in STEPS:
            with self.subTest(step=name):
                self.check_script(name, count, final_check, {1}, spawn_error=True)

    def test_native_diagnostics_run_only_after_failed_macos_integration(self):
        integration = step_for("Test native platform integration")
        self.assertIn("        id: native_integration\n", integration)
        self.assertNotIn("continue-on-error", integration)
        diagnostics = step_for("Report macOS native denial evidence")
        self.assertIn(
            "if: ${{ !cancelled() && runner.os == 'macOS' && steps.native_integration.outcome == 'failure' }}",
            diagnostics,
        )
        self.assertIn("run: python scripts/macos-native-diagnostics.py", diagnostics)


class NativeMacosDiagnostics(unittest.TestCase):
    def test_only_crash_cause_fields_from_matching_process_are_printed(self):
        report = {
            "procName": "native probe", "pid": 42, "procPath": "do-not-print-path",
            "exception": {"type": "EXC_CRASH", "signal": "SIGKILL", "private": "secret"},
            "termination": {"namespace": "CODESIGNING", "code": 2},
            "threads": [{"memory": "secret"}], "usedImages": ["secret"],
        }
        ips = json.dumps({"incident_id": "secret"}) + "\n" + json.dumps(report)
        summary = DIAGNOSTICS.crash_summary(ips)
        self.assertEqual(summary["termination"], {"namespace": "CODESIGNING", "code": 2})
        self.assertEqual(summary["exception"], {"type": "EXC_CRASH", "signal": "SIGKILL"})
        self.assertNotIn("secret", json.dumps(summary))
        self.assertNotIn("do-not-print-path", json.dumps(summary))
        report["procName"] = "unrelated-app"
        self.assertIsNone(DIAGNOSTICS.crash_summary(json.dumps(report)))

    def test_crash_collection_filters_old_unrelated_and_malformed_reports(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, contents, age in (
                ("native probe-current.ips", '{"procName":"native probe","pid":42}', 0),
                ("native probe-stale.ips", '{"procName":"native probe","pid":99}', 601),
                ("unrelated-current.ips", '{"procName":"native probe","pid":98}', 0),
                ("native probe-malformed.ips", "invalid json", 0),
            ):
                path = root / name
                path.write_text(contents)
                os.utime(path, (1000 - age, 1000 - age))
            output = io.StringIO()
            with redirect_stdout(output):
                DIAGNOSTICS.print_crashes([root], now=1000)
            value = output.getvalue()
            self.assertIn('"pid": 42', value)
            self.assertNotIn('"pid": 99', value)
            self.assertNotIn('"pid": 98', value)
            self.assertIn("Cannot summarize native probe-malformed.ips", value)

    def test_log_failure_or_timeout_still_collects_crash_evidence(self):
        for failure in (
            FileNotFoundError("unavailable log command"),
            subprocess.TimeoutExpired(["log"], 20, output=b"partial native log"),
        ):
            with self.subTest(failure=type(failure).__name__):
                with patch.object(DIAGNOSTICS.sys, "platform", "darwin"):
                    with patch.object(DIAGNOSTICS.subprocess, "run", side_effect=failure) as run:
                        with patch.object(DIAGNOSTICS, "print_crashes") as crashes:
                            with redirect_stdout(io.StringIO()):
                                DIAGNOSTICS.main()
                        crashes.assert_called_once()
                    args, kwargs = run.call_args
                    self.assertEqual(args[0][:2], ["/usr/bin/log", "show"])
                    self.assertIn("--predicate", args[0])
                    self.assertEqual(kwargs["timeout"], 20)
                    self.assertFalse(kwargs["check"])
                    self.assertNotIn("shell", kwargs)

    def test_diagnostic_output_is_bounded(self):
        output = io.StringIO()
        with redirect_stdout(output):
            DIAGNOSTICS.emit_bounded("x" * (DIAGNOSTICS.MAX_OUTPUT + 100))
        self.assertIn("[diagnostic output truncated]", output.getvalue())
        self.assertLess(len(output.getvalue()), DIAGNOSTICS.MAX_OUTPUT + 100)


if __name__ == "__main__":
    unittest.main()
