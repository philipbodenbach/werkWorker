"""Check CI orchestration; native tests themselves always use the real backend."""

from contextlib import redirect_stderr, redirect_stdout
import io
import os
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch


WORKFLOW = (
    Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml"
).read_text()
STEPS = (
    ("Test native platform integration", 3, "windows_sandbox"),
    ("Check native SDK and lint", 4, "examples/sdk-consumer/Cargo.toml"),
)


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


if __name__ == "__main__":
    unittest.main()
