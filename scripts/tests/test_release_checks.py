import copy
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('release_checks', ROOT / 'scripts/wait-release-checks.py')
checks = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checks)


class ReleaseChecksTests(unittest.TestCase):
    def setUp(self):
        self.repo = 'owner/worker'
        self.args = (self.repo, 5, 'release/auto-v0.2.0-base', 'release-sha', 'base-sha', 'main')
        self.pr = {
            'state': 'open', 'draft': False, 'mergeable': True, 'mergeable_state': 'clean',
            'head': {'sha': self.args[3], 'ref': self.args[2], 'repo': {'full_name': self.repo}},
            'base': {'sha': self.args[4], 'ref': 'main'},
        }
        self.runs = [self.run_for(workflow, i + 1) for i, workflow in enumerate(checks.WORKFLOWS)]

    def run_for(self, workflow, run_id, **overrides):
        return {
            'id': run_id, 'path': workflow, 'event': 'pull_request',
            'status': 'completed', 'conclusion': 'success',
            'head_sha': self.args[3], 'head_branch': self.args[2],
            'head_repository': {'full_name': self.repo}, 'pull_requests': [{'number': 5}],
            **overrides,
        }

    def status(self):
        responses = [json.dumps(self.pr), '\n'.join(json.dumps(run) for run in self.runs)]
        with patch.object(checks, 'gh', side_effect=responses) as gh:
            result = checks.release_status(*self.args)
        self.assertEqual(gh.call_args_list[0].args, ('api', 'repos/owner/worker/pulls/5'))
        self.assertIn('event=pull_request', gh.call_args_list[1].args)
        self.assertIn('head_sha=release-sha', gh.call_args_list[1].args)
        self.assertIn('--paginate', gh.call_args_list[1].args)
        return result

    def test_both_pr_workflows_and_merge_readiness_allow_release(self):
        self.assertIsNone(self.status())

    def test_green_manual_runs_do_not_satisfy_pr_checks(self):
        self.runs = [dict(run, event='workflow_dispatch') for run in self.runs]
        self.assertIn('waiting for the PR workflow', self.status())

    def test_green_manual_run_does_not_hide_pending_pr_run(self):
        manual = dict(self.runs[0], id=99, event='workflow_dispatch')
        self.runs[0].update(status='in_progress', conclusion=None)
        self.runs.append(manual)
        self.assertIn('ci.yml: in_progress', self.status())

    def test_green_tests_wait_for_branch_policy_propagation(self):
        for state in ('blocked', 'unknown', 'unstable'):
            with self.subTest(state=state):
                self.pr['mergeable_state'] = state
                self.assertIn('waiting for GitHub merge readiness', self.status())
        self.pr.update(mergeable_state='clean', mergeable=None)
        self.assertIn('waiting for GitHub merge readiness', self.status())

    def test_absent_checks_and_workflow_approval_keep_waiting(self):
        self.runs = []
        self.assertIn('waiting for the PR workflow', self.status())
        self.runs = [self.run_for(checks.WORKFLOWS[0], 1, conclusion='action_required')]
        self.assertIn('workflow approval required', self.status())

    def test_failed_cancelled_and_timed_out_runs_stop_release(self):
        for conclusion in ('failure', 'cancelled', 'timed_out', 'skipped', 'neutral'):
            with self.subTest(conclusion=conclusion):
                self.runs[0]['conclusion'] = conclusion
                with self.assertRaisesRegex(RuntimeError, f'{conclusion}.*actions/runs/1'):
                    self.status()

    def test_only_latest_matching_pr_run_counts(self):
        self.runs.insert(0, self.run_for(checks.WORKFLOWS[0], 0, conclusion='failure'))
        self.assertIsNone(self.status())
        self.runs.append(self.run_for(checks.WORKFLOWS[0], 10, status='queued', conclusion=None))
        self.assertIn('ci.yml: queued', self.status())

    def test_unrelated_runs_cannot_allow_release(self):
        for change in (
            {'head_sha': 'old-sha'}, {'head_branch': 'different-branch'},
            {'head_repository': {'full_name': 'fork/worker'}},
            {'pull_requests': [{'number': 99}]}, {'pull_requests': []},
            {'path': '.github/workflows/unrelated.yml'},
        ):
            with self.subTest(change=change):
                self.runs = [self.run_for(workflow, i, **change)
                             for i, workflow in enumerate(checks.WORKFLOWS)]
                self.assertIn('waiting for the PR workflow', self.status())

    def test_changed_pr_or_base_stops_before_reading_checks(self):
        original = copy.deepcopy(self.pr)
        for field, value in (('state', 'closed'), ('draft', True), ('mergeable_state', 'dirty')):
            with self.subTest(field=field):
                self.pr = {**copy.deepcopy(original), field: value}
                with patch.object(checks, 'gh', return_value=json.dumps(self.pr)) as gh:
                    with self.assertRaises(RuntimeError):
                        checks.release_status(*self.args)
                    self.assertEqual(gh.call_count, 1)
        for side, field, value in (
            ('head', 'sha', 'different-sha'), ('head', 'ref', 'different-branch'),
            ('head', 'repo', {'full_name': 'fork/worker'}),
            ('base', 'sha', 'new-base'), ('base', 'ref', 'different-base'),
        ):
            with self.subTest(side=side, field=field):
                self.pr = copy.deepcopy(original)
                self.pr[side][field] = value
                with patch.object(checks, 'gh', return_value=json.dumps(self.pr)) as gh:
                    with self.assertRaises(RuntimeError):
                        checks.release_status(*self.args)
                    self.assertEqual(gh.call_count, 1)

    def test_wait_resumes_after_approval_tests_and_policy_update(self):
        states = ['workflow approval required', 'ci.yml: in_progress',
                  'waiting for GitHub merge readiness (blocked)', None]
        with patch.object(checks, 'release_status', side_effect=states), \
                patch.object(checks.time, 'sleep') as sleep, \
                patch('sys.stdout', new_callable=io.StringIO) as stdout:
            checks.wait_for_release(*self.args)
        self.assertEqual(sleep.call_count, 3)
        self.assertIn('Approve workflows to run', stdout.getvalue())
        self.assertIn('ready to merge', stdout.getvalue())

    def test_wait_timeout_is_actionable_and_never_authorizes_merge(self):
        with patch.object(checks, 'release_status', return_value='workflow approval required'), \
                patch.object(checks.time, 'monotonic', side_effect=[0, 60]), \
                patch.object(checks.time, 'sleep') as sleep, \
                patch('sys.stdout', new_callable=io.StringIO):
            with self.assertRaisesRegex(RuntimeError, 'Timed out.*pull/5.*workflow approval required'):
                checks.wait_for_release(*self.args, timeout=60)
        sleep.assert_not_called()

    def test_api_failure_is_not_interpreted_as_success(self):
        with patch.object(checks.subprocess, 'run', return_value=subprocess.CompletedProcess(
            ['gh'], 1, stdout='', stderr='API access denied',
        )):
            with self.assertRaisesRegex(RuntimeError, 'API access denied'):
                checks.release_status(*self.args)


if __name__ == '__main__':
    unittest.main()
