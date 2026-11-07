# Development workflow

`main` is the rolling integration branch and should remain releasable. Start
small, short-lived branches from current `main`, then open a pull request back
to `main`. There is no permanent `develop` branch.

Use `feature/<topic>` for features, `fix/<topic>` for fixes, and `chore/<topic>`
or `docs/<topic>` for maintenance. Release automation owns `release/auto-*`.
These names are conventions, not GitHub-enforced restrictions.

## Pull requests

Use a Conventional Commit title such as `feat(tui): add command completion` or
`fix(runtime): restore terminal on exit`; release version inference uses these
subjects. Explain the problem, resulting behavior and relevant validation.
Keep changes focused and bring the branch up to date with `main` before merging.

The branch policy for the werk1112 release flow retains:

- A pull request, also for administrators, with all conversations resolved.
- Blocked force pushes and deletion of main.

Required PR status checks (including the associated up-to-date-branch rule)
are disabled in this policy, as in werk1112. This lets the generated release
metadata PR skip duplicate CI while Release validates it before merging.
Repository settings are separate from workflow files: apply the
[one-time branch-policy migration](docs/releases.md#repository-configuration)
before using this release flow. Ordinary product changes should pass all three
platform jobs before merging; GitHub no longer enforces that through required
status checks.

Mandatory approving reviews are currently set to zero because this is a
single-maintainer repository. PRs remain mandatory and CI remains enabled. If additional
maintainers join, raise the approval count to one. The single `worker` workflow
runs once per PR update with three native jobs. It also runs on pushes to main
and manual dispatch, but not on feature-branch pushes. A newer update cancels
the previous run for the same PR. Release/installer tests run inside these jobs;
there is no separate platform or release-test workflow. Linux retains the full
sandbox, browser, TUI, SDK and crate-installation suite and also validates its
native archive. Each successful job uploads one platform artifact containing
the tested archive and its checksum, retained for seven days.
The job names remain `linux-real-sandbox`,
`native (macos-14, macos, aarch64-apple-darwin, macos-aarch64.tar.gz)` and
`native (windows-2022, windows, x86_64-pc-windows-msvc, windows-x86_64.zip)`.
The macOS job keeps its historical name but runs on `macos-15`.

Prefer squash merges for feature branches, using the PR title as the commit
subject. Merge commits stay available for release preparation, which verifies
both parents. Rebase merges and automatic merging are disabled. GitHub deletes
merged source branches automatically; it offers an Update branch button when
a PR needs the latest main.

## Releases

Passing CI on main does not publish a release. Run the manual Release action
on main after normal CI passes to prepare the next version, validate and merge
its preparation PR, and create a tag and draft release. Release runs its own
preparation tests, CLI build and reference validation. Only generated metadata
uses `[skip ci]`; the workflow uses the built-in token without waiting for PR
workflow approval. Build platform archives from the tag and attach them before
publishing. See [build and release instructions](docs/releases.md).
