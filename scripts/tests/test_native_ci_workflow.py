"""Check CI error propagation; this does not simulate sandbox execution."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


WORKFLOW = (
    Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml"
).read_text()
BASH = shutil.which("bash")
if os.name == "nt":
    git_bash = Path(os.environ.get("ProgramFiles", "C:/Program Files")) / "Git/bin/bash.exe"
    if git_bash.is_file():
        BASH = str(git_bash)


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
    def test_native_checks_continue_after_a_failed_step(self):
        for name in ("Test native platform integration", "Check native SDK and lint"):
            step = step_for(name)
            self.assertEqual(
                step.splitlines()[0].strip(),
                "if: ${{ !cancelled() && runner.os != 'Linux' }}",
            )
            self.assertIn("        shell: bash\n", step)
        self.assertIn("--no-fail-fast", script_for("Test native platform integration"))

    @unittest.skipUnless(BASH, "Requires the CI Bash interpreter")
    def test_every_command_runs_and_each_failure_is_retained(self):
        cases = [
            ("Test native platform integration", 3, "--test windows_sandbox"),
            ("Check native SDK and lint", 4, "examples/sdk-consumer/Cargo.toml"),
        ]
        # Only this shell-orchestration test substitutes Cargo. Actual native
        # conformance always runs real Cargo and the real sandbox backend.
        prefix = """
count=0
cargo() {
    count=$((count + 1))
    printf '%s\\n' "$*" >> calls
    if [ "$count" = "$FAIL_AT" ]; then return 101; fi
    return 0
}
"""
        for name, count, final_check in cases:
            for fail_at in range(count + 1):
                with self.subTest(step=name, fail_at=fail_at):
                    with tempfile.TemporaryDirectory() as directory:
                        result = subprocess.run(
                            [BASH, "--noprofile", "--norc", "-e", "-o", "pipefail", "-c",
                             prefix + script_for(name)],
                            cwd=directory,
                            env=dict(os.environ, FAIL_AT=str(fail_at)),
                            text=True,
                            capture_output=True,
                        )
                        calls = (Path(directory) / "calls").read_text().splitlines()
                        self.assertEqual(len(calls), count, result.stderr)
                        self.assertIn(final_check, calls[-1])
                        self.assertEqual(result.returncode == 0, fail_at == 0)


if __name__ == "__main__":
    unittest.main()
