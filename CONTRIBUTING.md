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

Main's GitHub protection requires:

- A pull request, also for administrators, with all conversations resolved.
- Three GitHub Actions checks: `linux-real-sandbox` (including the Linux x86_64
  archive), `native (macos-14, macos, aarch64-apple-darwin, macos-aarch64.tar.gz)`
  and `native (windows-2022, windows, x86_64-pc-windows-msvc, windows-x86_64.zip)`.
  The macOS check keeps its existing name but runs on `macos-15`.
- An up-to-date branch before merging. Force pushes and deletion of main are blocked.

Mandatory approving reviews are currently set to zero because this is a
single-maintainer repository. PRs and CI are still required. If additional
maintainers join, raise the approval count to one. The single `worker` workflow
runs once per PR update with three native jobs. It also runs on pushes to main
and manual dispatch, but not on feature-branch pushes. A newer update cancels
the previous run for the same PR. Release/installer tests run inside these jobs;
there is no separate platform or release-test workflow. Linux retains the full
sandbox, browser, TUI, SDK and crate-installation suite and also validates its
native archive. Each successful job uploads one platform artifact containing
the tested archive and its checksum, retained for seven days.

Prefer squash merges for feature branches, using the PR title as the commit
subject. Merge commits stay available for release preparation, which verifies
both parents. Rebase merges and automatic merging are disabled. GitHub deletes
merged source branches automatically; it offers an Update branch button when
a PR needs the latest main.

## Releases

Passing CI on main does not publish a release. Run the manual Release action
on main to prepare the next version, validate and merge its preparation PR,
and create a tag and draft release. Its preparation branch must pass the same
required checks. Build platform archives from the tag and attach them before
publishing. See [build and release instructions](docs/releases.md).
