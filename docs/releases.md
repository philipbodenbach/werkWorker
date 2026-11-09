# Build, install and release

Current release: `v0.3.1`.

Release preparation follows werk1112 at revision
`3d33f7c5ccb3785a450310b5af9268077c5c9683`: a manually dispatched release action
prepares versions/notes, creates and merges its preparation PR, and tags the
merge. werkWorker extends that pattern with three native platform builds and
asset upload in the same workflow. A new GitHub release is always created as
a **draft** first. It receives all three archives and their checksums before
optional publication. There is no package-registry upload.

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

Release uses the built-in `GITHUB_TOKEN` with contents/pull-requests write
permissions, just like werk1112. It runs release-tooling tests, builds the
versioned CLI and validates the generated reference before creating the
preparation PR. Only that generated metadata commit carries `[skip ci]`.
Normal product changes must pass the three-platform `worker` workflow before
starting Release. The full Linux sandbox/browser/TUI/SDK suite remains in
normal CI. Release builds and smoke-tests the three native packages from its
tagged commit after preparation succeeds.

There is no PR-check polling, manual CI dispatch or separate bot-token secret.
The flow does not depend on approving the generated PR's workflow. This requires
the branch policy below: skipped PR CI cannot satisfy required status checks.
See [GitHub's required-check rules](https://docs.github.com/en/pull-requests/how-tos/merge-and-close-pull-requests/troubleshooting-required-status-checks).

Release still honors branch protection and never uses `--admin`. Keep merge
commits enabled for the preparation PR's two-parent verification. Base/head/tree
checks reject races before tagging. A retry reuses an identical preparation
branch and PR; conflicting generated content stops the run rather than
replacing the branch. Retries retain valid existing assets and never move a
tag. The first release needs no previous tag. No local build or preparation
script commits, tags or pushes. Only the explicitly started GitHub release
workflow performs those operations.

### Automatic platform artifacts

After preparation, three native build jobs check out the exact commit emitted
for the release tag. They use the same package scripts as local builds, then
run the native installer smoke test for both commands. No checkout of a moving
branch is used for these packages.

| Release build | Runner | Archive suffix |
|---|---|---|
| Linux x86_64 | `ubuntu-22.04` | `linux-x86_64.tar.gz` |
| macOS Apple Silicon | `macos-15` | `macos-aarch64.tar.gz` |
| Windows x86_64 | `windows-2022` | `windows-x86_64.zip` |

The Linux release runner uses glibc 2.35 to avoid the `GLIBC_2.39` warnings
observed with Ubuntu 24.04 builds. This does not change the normal Linux CI
runner or its sandbox tests.

Successful jobs retain their archive and `.sha256` as Actions artifacts for
30 days. A final upload job depends on all three builds, validates the package
set and attaches the three archives plus their three checksum files to the
selected GitHub release. These jobs are connected with `needs` inside this
Release workflow; there is no second dispatch or tag-triggered workflow.

- `draft=true` (the default) leaves a new release as a draft with all six assets.
- `draft=false` publishes the release only after every native build, smoke test
  and asset upload succeeds. It never publishes a new release before packaging.

If a build or upload fails, the new release stays a draft. Use **Re-run failed
jobs** to retry the failing jobs; already successful build artifacts remain
available during their 30-day retention period. Upload retries preserve an
existing valid archive/checksum pair, fill in missing files and stop on
inconsistent existing assets. They do not use `--clobber` or replace existing
files. A retry does not turn an already published release back into a draft.

If the failure requires a workflow fix, start a new Release run from the updated
default branch once its normal CI passes. **Re-run failed jobs** retains the
original workflow revision; it does not pick up that fix. If the failed run
already created a tag, the new fix is a subsequent commit and the next `patch`
run prepares the next patch version. See
[GitHub's rerun behavior](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/re-run-workflows-and-jobs).

Version selection still belongs to `scripts/prepare-release.py` and the
`bump` input. Do not edit Cargo versions or create a tag just to rebuild
artifacts. For example, after the v0.3.0 release and subsequent changes,
starting Release with `bump=patch` prepares v0.3.1 through the normal workflow;
an already prepared version is resumed under the rules above.

### Repository configuration

Workflow files do not change GitHub branch protection. To migrate an existing
werkWorker repository to the werk1112 policy, under **Settings → Branches →
main**, disable **Require status checks to pass before merging**, including its
up-to-date-branch requirement. Remove all former required contexts, including
Linux sandbox, macOS and Windows. Keep PRs required (also for administrators),
conversation resolution, and the restrictions on force pushes and deletion.
Mandatory approving reviews remain zero for this single-maintainer repository.
Also retain **Allow GitHub Actions to create and approve pull requests** under
**Settings → Actions → General → Workflow permissions** and enable merge commits.

This removes the merge requirement, not the CI jobs: ordinary PRs and main
still run all three platform jobs and produce three artifacts. Maintainers
must check these results before merging product changes; GitHub no longer
blocks a merge for a failing or missing status check. If required status checks
are retained, this release design cannot be used without a different CI/token
flow. Do not add a rule requiring workflows/status checks to the release PR.

An early, read-only check in Release detects classic branch-protection status
requirements before any build or preparation PR. It reports the incompatible
contexts immediately; it never changes protection settings. Rulesets and other
merge requirements are still enforced by GitHub at merge time.

After the workflow change reaches the default branch and settings are aligned,
start a **new Release run** there. Re-running a failed job from the old workflow
still uses its old workflow definition. Old unmerged preparation PRs are not
modified automatically; the new run derives its branch from the current base.

Development follows [short-lived feature branches into protected main](../CONTRIBUTING.md).
Main advances continuously; starting a tagged release remains an explicit
action. Platform assets are built and attached automatically before publication.

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
builds, the three CI jobs and the three Release build jobs use the same
validation path. Direct Cargo aliases remain available when only compilation
is needed.

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

The Release workflow builds and uploads these archives automatically. The
manual commands below remain available for native local builds and controlled
uploads to an existing version. `latest` installers see published releases only.

### Optional manual build and upload

Choose an existing GitHub release and build from its tag in a fresh worktree.
The examples use `v0.3.0`; change the version variable to the release you want
to populate. Do not upload binaries from a `0.2.0` checkout to `v0.3.0`, even
if the filenames are changed. The package script reads the version from
`Cargo.toml` and already runs the build script, so no separate build is needed.
Authenticate `gh` for `philipbodenbach/werkWorker` before uploading.

On Linux x86_64 / WSL2, run this from an existing werkWorker checkout. For
macOS Apple Silicon, set `PLATFORM=macos` and `SUFFIX=macos-aarch64.tar.gz` and
run the same commands on that Mac:

```bash
set -e
VERSION=v0.3.0
PLATFORM=linux
SUFFIX=linux-x86_64.tar.gz

gh release view "$VERSION" --repo philipbodenbach/werkWorker
git fetch origin tag "$VERSION"
git worktree add --detach "../werkWorker-$VERSION" "$VERSION"
cd "../werkWorker-$VERSION"

./scripts/package-release.sh "$PLATFORM"
ARCHIVE="releases/werkWorker-$VERSION-$SUFFIX"
python3 scripts/smoke-install.py "$ARCHIVE"
if command -v sha256sum >/dev/null; then
  (cd releases && sha256sum -c "${ARCHIVE##*/}.sha256")
else
  (cd releases && shasum -a 256 -c "${ARCHIVE##*/}.sha256")
fi
gh release upload "$VERSION" "$ARCHIVE" "$ARCHIVE.sha256" --repo philipbodenbach/werkWorker
```

On Windows x86_64, use x64 Developer PowerShell with Python 3.11+ and PowerShell
7 (`pwsh`, needed by the installer smoke test):

```powershell
$ErrorActionPreference = 'Stop'
$Version = 'v0.3.0'
gh release view $Version --repo philipbodenbach/werkWorker
if ($LASTEXITCODE -ne 0) { throw 'Release lookup failed' }
git fetch origin tag $Version
if ($LASTEXITCODE -ne 0) { throw 'Tag fetch failed' }
git worktree add --detach "../werkWorker-$Version" $Version
if ($LASTEXITCODE -ne 0) { throw 'Release checkout failed' }
Set-Location "../werkWorker-$Version"

.\scripts\package-release.ps1
$Archive = "releases/werkWorker-$Version-windows-x86_64.zip"
py -3 scripts/smoke-install.py $Archive
if ($LASTEXITCODE -ne 0) { throw 'Installer smoke test failed' }
$ExpectedHash = ((Get-Content "$Archive.sha256" -Raw).Trim() -split '\s+')[0]
if ((Get-FileHash $Archive -Algorithm SHA256).Hash -ne $ExpectedHash) {
    throw 'Archive checksum mismatch'
}
gh release upload $Version $Archive "$Archive.sha256" --repo philipbodenbach/werkWorker
if ($LASTEXITCODE -ne 0) { throw 'Release asset upload failed' }
```

These uploads attach each archive and its checksum to the selected version.
They preserve the release's existing draft or published status. No `--clobber`
is used: an existing asset with the same name causes an error instead of being
replaced. Inspect the release assets after uploading; publish a draft only
after all three platform archives and their checksum files are present.

### Platform CI artifacts

The `worker` action has exactly three native jobs: Linux x86_64, macOS Apple
Silicon and Windows x86_64. It validates archive contents, installers and
offline startup on each target. The Linux job also runs the full sandbox,
browser, terminal, SDK and crate-installation suite. Release automation tests
run within those jobs rather than in an extra workflow.

Each job uploads one Actions artifact (`werkWorker-linux`, `werkWorker-macos`
or `werkWorker-windows`) containing its archive and adjacent `.sha256`, retained
for seven days. A successful run therefore offers three platform downloads.
These ordinary CI artifacts do not publish a GitHub release. The Release
workflow produces its own packages from the exact tag commit, retains them for
30 days and uploads them after all three native builds pass.

CI runs on PR updates, pushes to main and manual dispatch. Feature-branch pushes
do not start a duplicate run, and newer updates cancel an older run for the same
PR. No platform status context is required by the werk1112 branch policy;
this also applies to contexts from the retired `platform-checks.yml` and
`release-checks.yml` workflows. The Linux archive belongs to the sandbox job.

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

Primary references: [Werk release preparation pattern](https://github.com/philipbodenbach/werk1112/blob/3d33f7c5ccb3785a450310b5af9268077c5c9683/.github/workflows/release.yml),
[GitHub release CLI](https://cli.github.com/manual/gh_release_create),
[hosted runner platforms](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
