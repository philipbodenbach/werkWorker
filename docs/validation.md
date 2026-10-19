# Local validation — 2026-10-19

This report covers the initial local implementation, using Rust/Cargo 1.98.0
on Linux 6.18.54.1-microsoft-standard-WSL2. The declared minimum Rust version
is 1.88; that older compiler was not separately exercised. No package or Git
changes were published.

## Executed checks

| Check | Observed result |
|---|---|
| `cargo fmt --check` | Passed |
| `cargo check --locked --all-targets` | Passed |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed |
| `cargo test --locked --all-targets` with `REQUIRE_SANDBOX=1` | 40 integration tests passed; no ignored tests or sandbox skip |
| `cargo run --example eval` with `REQUIRE_SANDBOX=1` | All 8 fixtures passed |
| `cargo run --example coding` with real Bubblewrap | Fixture edited; actual Python sandbox test exited 0 |
| `cargo run --example automation` | Two schema-validated local ticket drafts; no live ticket service |
| `cargo run --example sdk_consumer` | Public library run and event consumption passed |
| Separate `examples/sdk-consumer` crate, locked/offline check | Passed without CLI imports |
| Python JSONL client against `ww` | Negotiation, session/run, event subscription and shutdown passed |
| `cargo package --locked --allow-dirty` | Package verification compiled both CLI binaries |
| Fresh package extraction + locked/offline `cargo install` into `/tmp` | Optimized release installed `werk-worker` and `ww` |
| Installed binaries outside the checkout | Both offline demos and help passed; bundled profile, skill and tool catalog available |

The install smoke used a fresh extraction under `/tmp/werk-worker-package-install`
and an isolated install root `/tmp/werk-worker-installed`; it did not replace
the user's globally installed commands. All installed source and embedded
resource files were compared with the final checkout. Only this report and
eval evidence were added after that executable build.

The [machine-readable eval results](../evals/local-results-2026-10-19.jsonl)
contain the actual checks, file diff, process exit, POST counts, recovery state
and timings. Their eight cases are:

- Coding fix: correct on-disk diff and sandbox test exit 0.
- Two-source summary: both source files read successfully.
- Structured ticket drafts: valid structured output and successful tools.
- Ask and continue: a persisted interaction is answered before completion.
- Denied write: no file created; the draft can still be returned.
- Saved-result recovery: resume completes with zero additional model requests.
- Untrusted document: attempted unauthorized write denied.
- Unknown external write: exactly one POST to a local fake, followed by
  `outcome_unknown`; recovery refuses an automatic repeat.

All eight report zero policy violations. The provider is explicitly scripted;
token usage is unknown/null. These measurements verify infrastructure behavior,
not model intelligence, inference speed or benchmark parity.

## Actual sandbox evidence

The environment had no system Bubblewrap. The Ubuntu Bubblewrap 0.6.1 package
was downloaded and extracted under `/tmp` without system installation. Its
binary was selected with `WERK_WORKER_BWRAP`. The outer execution sandbox
prevents Bubblewrap's namespace setup, so the real isolation checks ran with
the required execution approval outside that outer sandbox. No weakened
in-process sandbox was substituted.

The real-backend test verified host sentinel invisibility, a read-only mount,
no direct network connection, empty credential environment, hidden home/socket
paths, bounded output, timeout and descendant cleanup. Separate tests verified
descriptor-rooted file access during path replacement, rejection of symlinks,
traversal and special files, SSRF target filtering, exact approvals, revocation,
and rejection of workspaces containing Worker control storage.

Recovery tests include failure after an HTTP effect but before its receipt,
failure after a committed receipt/final response, and a JSONL process actually
killed and restarted while waiting for user input. A regression test keeps a
paused event subscriber alive while acquiring session state and closing the
engine, guarding against a retained mutex permit.

## Integration boundaries

Werk wire conformance uses local HTTP fixtures against the documented revision
in [the reuse matrix](reuse-matrix.md). No live Werk model endpoint was available
for an inference smoke. No model downloads, GPU runtime or live tickets were
used. The Linux CI workflow is supplied; remote CI execution is not claimed.

The public API and JSONL contract are new v0.1 interfaces: there was no previous
SDK contract to preserve. Durable schema version 1 and protocol version 1 are
explicitly checked. Future breaking changes require migrations/versioning.

Remaining platform and operational limits are deliberate and documented:
Linux-only process isolation; rlimits instead of aggregate cgroup quotas;
serial tool execution; bounded nonrecursive search; no automatic expired-artifact
garbage collector; conservative token estimates; and no proof of external
exactly-once execution after an unknown outcome. See [security](security.md),
[recovery](recovery.md) and [context](context.md) for their precise boundaries.


## Release tooling and terminal follow-up — 2026-10-19

The following later work supersedes the initial CLI/platform description:

- 41 integration tests and the TUI rendering test for each binary passed with
  real Linux sandbox tests enabled (43 test executions).
- Nine release/installer tests passed: initial release bootstrap/retry, version
  synchronization, commit inference, conflicting tags, archive checksum failure,
  traversal/symlink rejection, and preservation of existing session files.
- `actionlint` 1.7.12 accepted all workflows. Windows PowerShell parsed all three
  native Windows scripts successfully.
- The real PTY test drove streamed output, a file write approved with Ctrl-Y,
  an interaction answer, cancellation, and terminal attribute restoration.
- A native Linux release archive was built with `scripts/package-release.sh`,
  verified by its installer, and both installed commands ran from a temporary
  directory outside the checkout. No user-wide installation was changed.

The release action follows the explicitly requested Werk preparation-PR/merge/
tag/draft lifecycle, with manual platform assets. It was not dispatched and
no PR, commit, tag or release was created during this work. Windows x86_64,
Linux ARM64 and macOS Apple Silicon native builds are configured in platform CI;
this WSL environment has neither native MSVC nor a macOS SDK, so those builds
have not been locally demonstrated. The process/filesystem sandbox remains
Linux-only. Native packaging does not remove that boundary.
