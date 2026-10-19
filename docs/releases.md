# Build, install and release

Current release: `v0.1.0`.

The workflow deliberately follows werk1112 at revision
`78254d7f709586576ec14d06a7f362a8185c2f36`: a manually dispatched release action
prepares versions/notes, creates and merges its preparation PR, tags the merge
and creates a GitHub release. Platform archives are built separately from that
tag and uploaded manually. The workflow defaults to a **draft**, allowing all
platform assets to be attached before publication. No package-registry upload.

## Release action

Run **Release** on the default branch. Inputs are `bump` (`auto`, `patch`, `minor`,
`major`) and `draft` (default true). `auto` uses Conventional Commit subjects and
BREAKING CHANGE trailers, or accepts an already prepared version. The initial
release uses the current Cargo version. Unreleased changelog bullets take
precedence over generated commit-subject notes. Versions in Cargo.toml, both
Cargo lockfiles, README and this page are updated together. The CLI reference
is regenerated from the versioned binary before the preparation PR.

Enable Actions' repository setting allowing creation of pull requests.
The workflow requires contents/pull-requests write permissions and honors branch
protection; it does not use `--admin`. If protection requires checks that a
GITHUB_TOKEN-created PR cannot trigger, provide a repository-approved bot/App
workflow token or finish the reviewed PR before retrying. Base/head/tree checks
reject races. Retries preserve existing releases/assets and never move a tag.
The first release needs no previous tag. No local script commits, tags or pushes.
Only the explicitly started GitHub release workflow performs those operations.

## Native platform builds

| Host | Build | Package | Archive suffix |
|---|---|---|---|
| Linux x86_64 / WSL2 | `cargo build-linux` | `./scripts/package-release.sh linux` | `linux-x86_64.tar.gz` |
| Linux ARM64 | `cargo build-linux-aarch64` | `./scripts/package-release.sh linux-aarch64` | `linux-aarch64.tar.gz` |
| Windows x86_64 | `./scripts/build-windows.ps1` | `./scripts/package-release.ps1` | `windows-x86_64.zip` |
| macOS Apple Silicon | `cargo build-macos-apple-silicon` | `./scripts/package-release.sh macos` | `macos-aarch64.tar.gz` |

Archives are `releases/werkWorker-v<VERSION>-<suffix>` with adjacent `.sha256`.
Each contains both commands (`werk-worker` and `ww`, `.exe` on Windows), README,
LICENSE and NOTICE. Skills/profiles are embedded. Build on the target OS/CPU;
Rust target installation alone is not a cross toolchain. Native Rust stable,
a C compiler and Python 3.11+ are needed (MSVC + Windows SDK on Windows,
Xcode command-line tools on macOS). There is no CUDA/toolkit/GPU dependency:
Strix Halo and DGX Spark use the corresponding generic Linux CPU archive.
These hardware-specific Werk builds need no separate Worker binary.

After checkout of the release tag, build each archive on the matching host,
then upload archives and their checksum files to the draft GitHub release.
Review assets before publishing. `latest` installers see published releases only.
The `Platform build checks` action validates native builds, archive contents,
installers and offline startup on each target; it does not publish artifacts.

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
