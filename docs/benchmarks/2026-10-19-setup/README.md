# TUI setup and project-configuration experiments — 2026-10-19

Scope: interactive connection/authentication, persisted non-secret preferences,
project rules/profiles and separation from host authority. This is a functional
regression study, not an inference throughput or model-quality benchmark.

Environment: Linux `6.18.54.1-microsoft-standard-WSL2`, Rust
`1.98.0 (88d9e12ae 2026-08-18)`, Cargo `1.98.0 (797e8a9bc 2026-08-05)`.
Debug binaries, 130×36-cell real pseudoterminals, isolated temporary workspaces,
config/state directories and SQLite stores. The fixture binds to a dynamically
allocated literal-loopback port and accepts only a synthetic Bearer credential.
No actual Werk credential was read and no live Qwen request was sent.

## Experiment series

| Experiment | Evidence / outcome |
|---|---|
| First setup HTTP attempt inside the execution sandbox | Socket creation failed with `PermissionError: Operation not permitted`; rerun with explicitly approved loopback/PTY access |
| Initial wrong-key test | Exposed `protocol_mismatch` for a bare HTTP 401 without a Werk envelope; fixed to return `auth_error` before parsing middleware response bodies |
| First rerun during compilation | Still used the older binary and reproduced that failure; subsequent tests waited for build completion |
| Rust project fixtures | 4 tests pass: rule priority/scoped chains, immutable source hashes, custom global/local profiles, tool/skill choices, no implicit grants, path/size/host-field rejection, concurrent-write conflicts |
| Setup render unit test | Key is absent from field strings, serialization and rendered buffers at 110×36, 30×10 and 1×1; runs for both CLI binary targets |
| Authenticated PTY setup | Wrong key → masked corrected paste → model discovery → 32,768 context / 4,096 output → connected session |
| Authenticated task | Fixture requests `files.read`; Ctrl-Y grants the displayed read once; next inference receives the file result and finishes |
| Secret separation | Key absent from raw terminal capture, saved settings, SQLite session JSON and request message/tool bodies |
| Persistence/reconnect | Global JSON mode 0600, `/setup` opens a new connection without replaying the task, key must be reentered, new process reloads preferences and can save only the environment reference |
| Project save and permissions | Project defaults override model/output settings; project saving preserves suggested commands and leaves global bytes unchanged; `requested_access: process` leaves all three sessions read-only with non-preapproved file grants |
| Existing terminal controls | Approval/answer, slash preview/completion, Ctrl-C reset/cancel/double-exit and terminal restoration pass |
| Full Rust regression | 72 passed, 0 failed, 0 ignored across all targets; real namespace sandbox test required and passed |
| Python release/install tooling | 9 tests passed |
| Static/document checks | `cargo fmt --check`, Clippy all targets with warnings denied, generated CLI-reference check and `git diff --check` passed |

## Reproduction and raw results

```sh
cargo build --locked --bins
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
REQUIRE_SANDBOX=1 WERK_WORKER_BWRAP=/path/to/bwrap \
  cargo test --locked --all-targets -- --nocapture
python3 -m unittest discover -s scripts/tests
python3 scripts/tests/tui_smoke.py target/debug/ww
python3 scripts/tests/setup_smoke.py target/debug/ww
python3 scripts/generate-cli-reference.py target/debug/werk-worker --check
```

The local full run used
`/tmp/werk-worker-study/bubblewrap/usr/bin/bwrap`. CI uses `/usr/bin/bwrap` and
runs both PTY scripts in its Linux job. The checked-in fixtures are in
[setup_smoke.py](../../../scripts/tests/setup_smoke.py),
[tui_smoke.py](../../../scripts/tests/tui_smoke.py),
[project.rs](../../../tests/project.rs) and [werk.rs](../../../tests/werk.rs).

Raw outputs: [Rust suite](cargo-test.txt), [Python suite](python-tests.txt),
[terminal tests](terminal-tests.txt), [Clippy](clippy.txt).
Cargo's per-test-group durations are recorded as test-run observations; they
include harness/fixture scheduling and are not user-facing setup latency claims.
The setup fixture performs exactly two inference POSTs for its one tool-use task;
opening, saving and reopening setup do not themselves cause inference.

## Limits

These experiments establish UI/state/policy behavior against a controlled Werk
wire fixture. They do not establish Qwen tool quality, CUDA/Metal availability,
real server concurrency or a particular per-slot context allocation. Those stay
subject to live deployment discovery and the existing context/admission checks.
No Windows/macOS execution was performed in this study. Their release CI remains
configured, while secure project metadata and native file/process tools currently
fail closed outside Linux. Additional directory rules are explicitly selected
startup snapshots; arbitrary progressive directory discovery is not implemented.
