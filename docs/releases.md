# Build, install and release

Current release: `v0.2.0`.

The workflow deliberately follows werk1112 at revision
`78254d7f709586576ec14d06a7f362a8185c2f36`: a manually dispatched release action
prepares versions/notes, creates and merges its preparation PR, tags the merge
and creates a GitHub release. Platform archives are built separately from that
tag and uploaded manually. The workflow defaults to a **draft**, allowing all
platform assets to be attached before publication. No package-registry upload.

## Release action

Run **Release** on the default branch. Inputs are `bump` (`auto`, `patch`, `minor`,
`major`) and `draft` (default true). `auto` uses Conventional Commit subjects and
BREAKING CHANGE trailers. Every bump choice first resumes an already prepared,
untagged version instead of incrementing it again. For example, if the v0.2.0
preparation PR merged but the workflow stopped before tagging, another `minor`
run finishes v0.2.0 with its existing date and notes. Retrying at the existing
release tag also reuses that version when there are no new commits; existing
tags are never moved. A new increment applies after the previous version is
tagged and new commits are present. The initial `auto` release uses the current
Cargo version. Unreleased changelog bullets take
precedence over generated commit-subject notes. Versions in Cargo.toml, both
Cargo lockfiles, README and this page are updated together. The CLI reference
is regenerated from the versioned binary before the preparation PR.

Actions' repository setting allows creation of pull requests. The workflow
requires contents/pull-requests write and actions read permissions and honors
branch protection; it does not use `--admin`. Release waits up to 60 minutes for
started CI and merge readiness on the exact preparation commit. When GitHub
reports `action_required`, the job stops immediately with the exact PR and CI
run links instead of spending an hour waiting for a human approval. The three
jobs of the real `pull_request` run must still pass before the merge.
It does not dispatch duplicate test runs: `workflow_dispatch` checks do not
satisfy required PR checks. See
[GitHub's required-check rules](https://docs.github.com/en/pull-requests/how-tos/merge-and-close-pull-requests/troubleshooting-required-status-checks).

With the built-in `GITHUB_TOKEN`, GitHub requires approval before running CI
on the generated PR. Open the PR linked in the action log and select
**Approve workflows to run**, then select **Re-run failed jobs** on the failed
Release run. Its summary contains the recovery steps. The retry reuses the
preparation PR and version, waits for the now-started tests, and proceeds only
after the required checks pass. Failed tests stop the release with a link to
the failing run; successful PR runs for the same commit are reused. If an
existing PR still awaits approval, approve it there; changing a token alone
does not retrigger that PR.

For one-click releases without that approval step, an owner can configure the
optional repository Actions secret **`RELEASE_PR_TOKEN`**: a fine-grained token
from a trusted bot/user, scoped only to this repository with **Pull requests:
read and write** and **Contents: read**. It is used only in the preparation-PR
step. Checkout/push, merge, tags and releases still use `GITHUB_TOKEN`. CI uses
its own read-only token. Do not place personal CLI credentials in the workflow
or disable required checks. GitHub also supports a dedicated GitHub App identity
for this purpose; see
[GitHub's workflow-trigger rules](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow#triggering-a-workflow-from-a-workflow).
Without this optional secret the approval-based flow remains available.

Unlike Werk1112's release setup, this repository requires three CI checks on
the preparation PR, so its `[skip ci]` shortcut cannot be reused here.
Keep merge commits enabled for the release PR's two-parent verification. Base/head/tree checks
reject races. Retries preserve existing releases/assets and never move a tag.
The first release needs no previous tag. No local script commits, tags or pushes.
Only the explicitly started GitHub release workflow performs those operations.

Development follows [short-lived feature branches into protected main](../CONTRIBUTING.md).
Main advances continuously; a tagged release remains an explicit action, with
platform assets attached to the draft before publication.

## Native platform builds

| Host | Build | Package | Archive suffix |
|---|---|---|---|
| Linux x86_64 / WSL2 | `./scripts/build-linux.sh` | `./scripts/package-release.sh linux` | `linux-x86_64.tar.gz` |
| Windows x86_64 | `./scripts/build-windows.ps1` | `./scripts/package-release.ps1` | `windows-x86_64.zip` |
| macOS Apple Silicon | `./scripts/build-macos.sh` | `./scripts/package-release.sh macos` | `macos-aarch64.tar.gz` |

The build entrypoints follow werk1112's native-build pattern: check the host,
native Rust toolchain, C compiler and Python before building; use the locked
release Cargo alias; run both command versions; and validate CLI reference
generation in a temporary directory without rewriting `docs/cli.md`. They work
from outside the checkout and respect `CARGO_TARGET_DIR`. Windows accepts the
`py -3` launcher as well as `python` or `python3` and requires the x64 MSVC
developer environment. Unlike the inference server, Worker needs no CUDA
compiler or GPU toolkit. Packaging invokes the same build entrypoints, so local
builds and the three CI jobs use the same validation path. Direct Cargo aliases
remain available when only compilation is needed.

Archives are `releases/werkWorker-v<VERSION>-<suffix>` with adjacent `.sha256`.
Each contains both commands (`werk-worker` and `ww`, `.exe` on Windows), README,
LICENSE and NOTICE. Skills/profiles are embedded. Build on the target OS/CPU;
Rust target installation alone is not a cross toolchain. Native Rust stable,
a C compiler and Python 3.11+ are needed (MSVC + Windows SDK on Windows,
Xcode command-line tools on macOS). These are the only three published binary
targets. There is no CUDA/toolkit/GPU dependency: Strix Halo uses the generic
Linux x86_64 archive. Linux ARM64 systems, including DGX Spark, require a source
build (`cargo install --path . --locked` from this checkout); the installer
reports this instead of requesting an unpublished ARM64 archive.

After checkout of the release tag, build each archive on the matching host,
then upload archives and their checksum files to the draft GitHub release.
Review assets before publishing. `latest` installers see published releases only.
The `worker` action has exactly three native jobs: Linux x86_64, macOS Apple
Silicon and Windows x86_64. It validates archive contents, installers and
offline startup on each target. The Linux job also runs the full sandbox,
browser, terminal, SDK and crate-installation suite. Release automation tests
run within those jobs rather than in an extra workflow.

Each job uploads one Actions artifact (`werkWorker-linux`, `werkWorker-macos`
or `werkWorker-windows`) containing its archive and adjacent `.sha256`, retained
for seven days. A successful run therefore offers three platform downloads.
These CI artifacts do not publish a GitHub release; public releases still use
archives built from the release tag and attached to the draft as above.

CI runs on PR updates, pushes to main and manual dispatch. Feature-branch pushes
do not start a duplicate run, and newer updates cancel an older run for the same
PR. The retired `platform-checks.yml` and `release-checks.yml` workflows must not
remain required in branch protection. The required checks are the existing
Linux sandbox, macOS native and Windows native names listed in
[CONTRIBUTING.md](../CONTRIBUTING.md); the separate Linux native and Linux ARM64
check contexts are removed. The Linux archive now belongs to the sandbox job.

## Installers

Linux / macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/philipbodenbach/werkWorker/main/scripts/install.sh -o /tmp/werkWorker-install.sh
sh /tmp/werkWorker-install.sh
ww
```

Native Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/philipbodenbach/werkWorker/main/scripts/install.ps1 | iex
ww
```

Options mirror Werk with a product-specific prefix:
`WERK_WORKER_VERSION` (latest or stable version), `WERK_WORKER_REPO`,
`WERK_WORKER_INSTALL_DIR`, and Windows `WERK_WORKER_ADD_TO_PATH=1`.
Default destinations are `~/.local/bin` and `%LOCALAPPDATA%/Programs/werkWorker/bin`.
For an already downloaded archive, set `WERK_WORKER_ARCHIVE` to its absolute
path and set `WERK_WORKER_VERSION`; the adjacent checksum is mandatory. This
also supports offline installation tests. Checksums and an exact archive member
allowlist are validated before replacing commands. Session state is untouched.

Installers do not install a Rust toolchain, models or Bubblewrap. File/process
sandbox tools currently require Linux and fail closed elsewhere. Native
Windows/macOS packages support the TUI, SDK, HTTP/Werk tools and sessions; their
presence is not a claim of a native process/filesystem sandbox.

Primary references: [Werk release workflow](https://github.com/philipbodenbach/werk1112/blob/78254d7f709586576ec14d06a7f362a8185c2f36/.github/workflows/release.yml),
[GitHub release CLI](https://cli.github.com/manual/gh_release_create),
[hosted runner platforms](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
