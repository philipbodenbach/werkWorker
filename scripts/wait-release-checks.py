"""Read-only release gate: wait for real PR CI and GitHub's merge readiness."""

import argparse
import json
import subprocess
import time


WORKFLOWS = ('.github/workflows/ci.yml', '.github/workflows/platform-checks.yml')


def gh(*args):
    result = subprocess.run(
        ['gh', *args], capture_output=True, text=True, timeout=60,
    )
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or 'GitHub API request failed')
    return result.stdout


def release_status(repo, pr_number, branch, head, base, default_branch):
    pr = json.loads(gh('api', f'repos/{repo}/pulls/{pr_number}'))
    if pr['state'] != 'open' or pr['draft']:
        raise RuntimeError('Release PR must be open and ready for review.')
    if (pr['head']['sha'] != head or pr['head']['ref'] != branch
            or pr['head']['repo']['full_name'].lower() != repo.lower()):
        raise RuntimeError('Release PR head changed. Restart Release from the default branch.')
    if pr['base']['sha'] != base or pr['base']['ref'] != default_branch:
        raise RuntimeError('Default branch changed. Restart Release from the latest branch.')
    if pr['mergeable_state'] in ('dirty', 'behind'):
        raise RuntimeError('Release PR is conflicting or outdated. Restart Release.')

    # Pagination also covers retries. Only runs attached to this PR and exact
    # preparation commit count; green manual runs cannot satisfy this gate.
    output = gh(
        'api', '--method', 'GET', f'repos/{repo}/actions/runs',
        '-f', f'branch={branch}', '-f', f'head_sha={head}',
        '-f', 'event=pull_request', '-f', 'per_page=100', '--paginate',
        '--jq', '.workflow_runs[] | @json',
    )
    latest = {}
    for line in output.splitlines():
        run = json.loads(line)
        workflow = run['path'].split('@', 1)[0]
        if (workflow not in WORKFLOWS or run['event'] != 'pull_request'
                or run['head_sha'] != head or run['head_branch'] != branch
                or run['head_repository']['full_name'].lower() != repo.lower()
                or not any(p['number'] == pr_number for p in run['pull_requests'])):
            continue
        if workflow not in latest or run['id'] > latest[workflow]['id']:
            latest[workflow] = run

    pending = []
    for workflow in WORKFLOWS:
        name = workflow.rsplit('/', 1)[1]
        run = latest.get(workflow)
        if run is None:
            pending.append(f'{name}: waiting for the PR workflow to appear')
            continue
        url = f'https://github.com/{repo}/actions/runs/{run["id"]}'
        if run['conclusion'] == 'action_required' or run['status'] == 'action_required':
            pending.append(f'{name}: workflow approval required ({url})')
        elif run['status'] != 'completed':
            pending.append(f'{name}: {run["status"]} ({url})')
        elif run['conclusion'] != 'success':
            raise RuntimeError(f'{name}: {run["conclusion"]}. Inspect {url} and retry the failed PR run.')

    if pending:
        return '; '.join(pending)
    # PR checks can finish before GitHub updates its branch-policy evaluation.
    # CLEAN also verifies any additional required checks/reviews/conversations.
    if pr['mergeable_state'] != 'clean' or pr['mergeable'] is not True:
        return f'PR tests passed; waiting for GitHub merge readiness ({pr["mergeable_state"]})'
    return None


def wait_for_release(repo, pr_number, branch, head, base, default_branch,
                     timeout=3600, interval=30):
    url = f'https://github.com/{repo}/pull/{pr_number}'
    print(f'Waiting for PR checks: {url}', flush=True)
    print('If GitHub requests approval, select "Approve workflows to run" in this PR. '
          'Release will continue automatically after the checks pass.', flush=True)
    deadline = time.monotonic() + timeout
    previous = None
    while True:
        status = release_status(repo, pr_number, branch, head, base, default_branch)
        if status is None:
            print('PR tests passed and GitHub reports the release PR ready to merge.', flush=True)
            return
        if status != previous:
            print(status, flush=True)
            previous = status
        if time.monotonic() >= deadline:
            raise RuntimeError(f'Timed out waiting for {url}: {status}. '
                               'Check workflow approval and branch requirements, then retry Release.')
        time.sleep(interval)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', required=True)
    parser.add_argument('--pr', type=int, required=True)
    parser.add_argument('--branch', required=True)
    parser.add_argument('--head', required=True)
    parser.add_argument('--base', required=True)
    parser.add_argument('--default-branch', required=True)
    args = parser.parse_args()
    try:
        wait_for_release(args.repo, args.pr, args.branch, args.head, args.base, args.default_branch)
    except (RuntimeError, subprocess.TimeoutExpired) as error:
        parser.exit(1, f'{error}\n')


if __name__ == '__main__':
    main()
