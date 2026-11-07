"""Run the actual release shell steps with inert Git/GitHub command doubles.

No real commits, merges, tags, network requests or releases are made, even in
fixtures. Unexpected commands fail closed. Preparation contents are tested in
test_release.py; these tests cover orchestration and retry safety.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = (ROOT / ".github/workflows/release.yml").read_text()
BASE = "a" * 40
HEAD = "b" * 40
MERGED = "c" * 40
TAG = "v0.3.0"
BRANCH = f"release/auto-{TAG}-{BASE[:12]}"
POLICY_QUERY = ('[.protection.required_status_checks.contexts[]?, '
                '.protection.required_status_checks.checks[]?.context] '
                '| unique | join(", ")')


def step_script(name):
    step = WORKFLOW.split(f"      - name: {name}\n", 1)[1].split("\n      - name:", 1)[0]
    return textwrap.dedent(step.split("        run: |\n", 1)[1])


@unittest.skipUnless(os.name == "posix" and shutil.which("bash"), "Linux/macOS shell steps")
class ReleaseWorkflowTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.state = self.root / "commands.json"
        self.output = self.root / "output"
        self.summary = self.root / "summary"
        self.commands = []
        stub = f"#!{sys.executable}\n" + textwrap.dedent('''\
            import json
            import os
            from pathlib import Path
            import sys

            path = Path(os.environ["RELEASE_TEST_COMMANDS"])
            state = json.loads(path.read_text())
            call = [Path(sys.argv[0]).name, *sys.argv[1:]]
            if not state["pending"] or call != state["pending"][0]["args"]:
                print("UNEXPECTED COMMAND: " + repr(call), file=sys.stderr)
                sys.exit(97)
            expected = state["pending"].pop(0)
            state["calls"].append(call)
            path.write_text(json.dumps(state))
            if expected["out"]:
                print(expected["out"])
            sys.exit(expected["code"])
            ''')
        for name in ("git", "gh", "python", "cargo"):
            executable = self.root / name
            executable.write_text(stub)
            executable.chmod(0o755)
        self.env = dict(
            os.environ, PATH=f"{self.root}:{os.environ['PATH']}",
            RELEASE_TEST_COMMANDS=str(self.state), RUNNER_TEMP=str(self.root),
            GITHUB_OUTPUT=str(self.output), GITHUB_STEP_SUMMARY=str(self.summary),
            GH_REPO="owner/worker", DEFAULT_BRANCH="main", RELEASE_TAG=TAG,
            RELEASE_PR="12", RELEASE_SHA=HEAD, BASE_SHA=BASE, DRAFT="true",
            GH_TOKEN="test-placeholder",
        )

    def expect(self, *args, out="", code=0):
        self.commands.append(dict(args=list(args), out=out, code=code))

    def run_step(self, name, success=True, **env):
        self.state.write_text(json.dumps(dict(pending=self.commands, calls=[])))
        result = subprocess.run(
            ["bash", "-e", "-o", "pipefail", "-c", step_script(name)],
            cwd=self.root, env=dict(self.env, **env), text=True, capture_output=True,
        )
        self.commands = []
        self.assertNotIn("UNEXPECTED COMMAND", result.stderr)
        self.assertEqual(json.loads(self.state.read_text())["pending"], [])
        self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
        return result

    def prepare_prefix(self, changed=True, existing=False):
        self.expect("git", "config", "user.name", "github-actions[bot]")
        self.expect("git", "config", "user.email",
                    "41898282+github-actions[bot]@users.noreply.github.com")
        self.expect("git", "diff", "--check")
        self.expect("git", "add", "--update")
        self.expect("git", "diff", "--cached", "--quiet", code=int(changed))
        if changed:
            self.expect("git", "rev-parse", "HEAD", out=BASE)
            self.expect("git", "ls-remote", "--heads", "origin", f"refs/heads/{BRANCH}",
                        out=f"{HEAD}\trefs/heads/{BRANCH}" if existing else "")

    def pr_lookup(self, existing=False):
        self.expect("gh", "pr", "list", "--head", BRANCH, "--base", "main", "--state", "open",
                    "--json", "number", "--jq", ".[0].number // empty", out="12" if existing else "")
        if not existing:
            self.expect("gh", "pr", "create", "--head", BRANCH, "--base", "main",
                        "--title", f"release(): {TAG}", "--body-file", str(self.root / "release-pr.md"),
                        out="12")
        self.expect("gh", "pr", "view", "12", "--json", "number", "--jq", ".number", out="12")

    def test_default_branch_required(self):
        self.run_step("Require the default branch", SELECTED_REF="refs/heads/main")
        result = self.run_step("Require the default branch", success=False,
                               SELECTED_REF="refs/heads/feature/test")
        self.assertIn("Run this workflow on the default branch", result.stdout)

    def test_branch_policy_allows_no_required_status_checks(self):
        self.expect("gh", "api", "repos/owner/worker/branches/main", "--jq", POLICY_QUERY)
        self.run_step("Check release branch policy")

    def test_branch_policy_reports_incompatible_checks_before_preparation(self):
        self.expect("gh", "api", "repos/owner/worker/branches/main", "--jq", POLICY_QUERY,
                    out="linux-real-sandbox, native (macos)")
        result = self.run_step("Check release branch policy", success=False)
        self.assertIn("Still required: linux-real-sandbox, native (macos)", result.stdout)
        self.assertIn("retaining its other protections", result.stdout)

    def test_branch_policy_lookup_failure_stops_release(self):
        self.expect("gh", "api", "repos/owner/worker/branches/main", "--jq", POLICY_QUERY, code=1)
        self.run_step("Check release branch policy", success=False)

    def test_generated_metadata_skips_ci_and_emits_exact_head(self):
        self.prepare_prefix()
        self.expect("git", "commit", "-m", f"release(): {TAG} [skip ci]")
        self.expect("git", "rev-parse", "HEAD", out=HEAD)
        self.expect("git", "push", "origin", f"HEAD:refs/heads/{BRANCH}")
        self.pr_lookup()
        self.run_step("Create the release preparation PR")
        self.assertEqual(self.output.read_text(), f"number=12\nhead={HEAD}\nbase={BASE}\n")
        self.assertIn("must pass their normal CI", (self.root / "release-pr.md").read_text())

    def test_retry_reuses_identical_preparation_without_commit_or_pr_creation(self):
        self.prepare_prefix(existing=True)
        self.expect("git", "fetch", "origin", f"refs/heads/{BRANCH}")
        self.expect("git", "write-tree", out="prepared-tree")
        self.expect("git", "rev-parse", "FETCH_HEAD^{tree}", out="prepared-tree")
        self.expect("git", "rev-parse", "FETCH_HEAD", out=HEAD)
        self.expect("git", "reset", "--hard", HEAD)
        self.pr_lookup(existing=True)
        self.run_step("Create the release preparation PR")
        self.assertEqual(self.output.read_text(), f"number=12\nhead={HEAD}\nbase={BASE}\n")

    def test_conflicting_preparation_is_not_overwritten(self):
        self.prepare_prefix(existing=True)
        self.expect("git", "fetch", "origin", f"refs/heads/{BRANCH}")
        self.expect("git", "write-tree", out="new-tree")
        self.expect("git", "rev-parse", "FETCH_HEAD^{tree}", out="previous-tree")
        result = self.run_step("Create the release preparation PR", success=False)
        self.assertIn("Existing release branch differs", result.stdout)
        self.assertFalse(self.output.exists())

    def test_unchanged_metadata_does_not_create_another_pr(self):
        self.prepare_prefix(changed=False)
        self.run_step("Create the release preparation PR")
        self.assertFalse(self.output.exists())

    def test_changed_default_branch_blocks_merge(self):
        self.expect("git", "ls-remote", "--heads", "origin", "refs/heads/main", out="changed\trefs/heads/main")
        result = self.run_step("Merge the release preparation PR", success=False)
        self.assertIn("Default branch advanced", result.stdout)

    def merge_prefix(self):
        self.expect("git", "ls-remote", "--heads", "origin", "refs/heads/main", out=f"{BASE}\trefs/heads/main")
        self.expect("gh", "pr", "merge", "12", "--merge", "--match-head-commit", HEAD)
        self.expect("gh", "pr", "view", "12", "--json", "mergeCommit", "--jq", ".mergeCommit.oid", out=MERGED)
        self.expect("git", "fetch", "origin", "main")
        self.expect("git", "checkout", "--detach", MERGED)

    def test_merge_verifies_both_parents_and_content(self):
        self.merge_prefix()
        self.expect("git", "rev-parse", "HEAD^1", out=BASE)
        self.expect("git", "rev-parse", "HEAD^2", out=HEAD)
        self.expect("git", "rev-parse", "HEAD^{tree}", out="release-tree")
        self.expect("git", "rev-parse", f"{HEAD}^{{tree}}", out="release-tree")
        self.run_step("Merge the release preparation PR")

    def test_merge_races_fail_before_tagging(self):
        for failure in ("base", "head", "tree"):
            with self.subTest(failure=failure):
                self.merge_prefix()
                self.expect("git", "rev-parse", "HEAD^1", out="changed" if failure == "base" else BASE)
                if failure != "base":
                    self.expect("git", "rev-parse", "HEAD^2", out="changed" if failure == "head" else HEAD)
                if failure == "tree":
                    self.expect("git", "rev-parse", "HEAD^{tree}", out="changed")
                    self.expect("git", "rev-parse", f"{HEAD}^{{tree}}", out="release-tree")
                self.run_step("Merge the release preparation PR", success=False)

    def test_existing_tag_is_never_moved(self):
        for matches in (True, False):
            with self.subTest(matches=matches):
                self.expect("python", "scripts/generate-cli-reference.py",
                            "target/cli-reference-neutral/debug/werk-worker", "--check")
                self.expect("git", "show-ref", "--verify", "--quiet", f"refs/tags/{TAG}")
                self.expect("git", "rev-parse", f"{TAG}^{{commit}}", out=MERGED if matches else "different")
                self.expect("git", "rev-parse", "HEAD", out=MERGED)
                if matches:
                    self.expect("git", "push", "origin", f"refs/tags/{TAG}")
                self.run_step("Validate and tag the merged release", success=matches)

    def test_metadata_validation_failure_prevents_new_tag(self):
        self.expect("python", "scripts/generate-cli-reference.py",
                    "target/cli-reference-neutral/debug/werk-worker", "--check")
        self.expect("git", "show-ref", "--verify", "--quiet", f"refs/tags/{TAG}", code=1)
        self.expect("python", "scripts/prepare-release.py", "check", "--notes-file",
                    str(self.root / "release-notes.md"), code=1)
        self.run_step("Validate and tag the merged release", success=False)

    def test_new_release_is_a_draft_and_existing_release_is_preserved(self):
        for existing in (False, True):
            with self.subTest(existing=existing):
                self.expect("gh", "release", "view", TAG, "--json", "url", "--jq", ".url", code=0 if existing else 1)
                if not existing:
                    self.expect("gh", "release", "create", TAG, "--verify-tag", "--title",
                                f"werkWorker {TAG}", "--notes-file", str(self.root / "release-notes.md"), "--draft")
                self.expect("gh", "release", "view", TAG, "--json", "url,isDraft", "--jq",
                            '"Release: " + .url + " (draft: " + (.isDraft | tostring) + ")"')
                self.run_step("Create GitHub release")


class ReleaseWorkflowContractTests(unittest.TestCase):
    def test_validation_precedes_publication_without_extra_credentials_or_ci_runs(self):
        self.assertIn("GH_TOKEN: ${{ github.token }}", WORKFLOW)
        for removed in ("secrets.", "wait-release-checks", "gh workflow run", "continue-on-error"):
            self.assertNotIn(removed, WORKFLOW)
        steps = [
            "Check release branch policy", "Test release preparation",
            "Build and refresh the CLI reference", "Create the release preparation PR",
            "Merge the release preparation PR", "Validate and tag the merged release",
            "Create GitHub release",
        ]
        positions = [WORKFLOW.index(f"- name: {name}\n") for name in steps]
        self.assertEqual(positions, sorted(positions))
        self.assertIn("python -m unittest discover -s scripts/tests -p 'test*release*.py'", WORKFLOW)
        self.assertIn("cargo +stable build --locked --bin werk-worker", WORKFLOW)


if __name__ == "__main__":
    unittest.main()
