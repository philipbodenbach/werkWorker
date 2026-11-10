# Testing and evals

Normal checks require no GPU, network service, API key or live Werk instance.
Local HTTP fakes use loopback listeners. Run:

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo check --locked --no-default-features --lib
cargo check --locked --manifest-path examples/sdk-consumer/Cargo.toml
cargo run --example eval
```

The native CI jobs must set `REQUIRE_SANDBOX=1`; they test real backends.
A local missing backend prints an explicit SKIP reason. A green unit suite with
that skip is not isolation proof. Example:

```sh
WERK_WORKER_BWRAP=/usr/bin/bwrap REQUIRE_SANDBOX=1 cargo test --test isolation
WERK_WORKER_BWRAP=/usr/bin/bwrap REQUIRE_SANDBOX=1 cargo run --example eval
```

The existing macOS/Windows jobs also run:

```sh
cargo test --locked --lib parallel::tests
cargo test --locked --test platform --test native_workspace --test sandbox_contract --test native_coding --test native_lifecycle --test native_policy --test contracts --test macos_process --test windows_sandbox
```

Run this command on the target OS, not merely with a cross-compilation target.
`native_workspace` checks pinned-object file operations, atomic publication,
aliases, links and root/parent replacement. `sandbox_contract` checks approval
binding and legacy rejection. `native_coding` runs the shared scripted engine
through file reads, an approved edit, a real process and current-file verification.
The OS-specific suites compile the std-only `native_process_probe.rs` fixture
and use synthetic files/listeners/handles with host-side positive controls.
They test read-only access, network/IPC denial, argv/stdin, bounded output,
cancellation and process lifetime. Windows tests explicitly provision new empty
workspaces; macOS tests explicitly enable its constrained single-process mode.
macOS fixtures use a finite 1 TiB virtual-address bound to accommodate Darwin
mappings; that is not a resident-memory containment measurement. A separate
tiny-limit test must fail before any untrusted code executes.

`native_lifecycle` drives background jobs through the real engine and checks
status/log/wait/cancel, lease retention and shutdown cleanup. `native_policy`
uses SQLite restart and actual process approvals, checks protected store/home
aliases, and requires conservative rejection where durable identity is missing.
Linux inode-reuse tests distinguish immutable creation identity from live lease
identity. Existing CLI configuration tests also run on native CI.

The local [validation record](cross-platform-sandbox.md#validation-record)
distinguishes executed native tests from configured CI and source typechecks.
No macOS native pass is claimed from this Linux/Windows development host.

`evals/suite.json` is versioned machine-readable infrastructure data with eight
cases. `examples/eval.rs` runs them through the public engine, real temporary
SQLite, fixture workspaces and, for uncertain writes, a local HTTP service.
Reports include observed file diffs, test exit status, schemas/expectations,
POST counts, policy violations, recovery state and Run metrics. Missing usage
or timings are null/unknown, never invented zero-cost inference. Scripted
answers are not model-quality measurements.

Test groups cover engine budgets and tool pairing, arbitrary SSE byte/UTF-8
boundaries, exact approvals, file races/traversal, generic network confinement,
Werk envelopes/capabilities, SQLite ownership/corruption, three write crash
boundaries, and a real killed/restarted JSONL subprocess with pending input.
Regression coverage includes an event stream retained after `select!` chooses
`wait`, ensuring slow subscribers cannot retain the session mutex.

The package smoke runs `cargo package --allow-dirty`, extracts the resulting
crate into a fresh temporary directory, installs it into an isolated root, and
runs the offline demo outside this repository. The separate consumer crate must
compile against the library without CLI imports. License/skills/profiles are
checked in the packaged file inventory.

Live model smoke is explicit: supply a local running Werk origin and confirmed
model capabilities. No test starts inference, downloads a model or sends real
tickets. There is no general performance or model-quality acceptance threshold.

## Parallel agents, context and benchmark reproduction

```sh
cargo test --test subagents --test context_policy --test streaming
cargo test --test werk --test rpc
cargo run --example parallel_benchmark > scheduler.json
cargo build --bins
python3 scripts/tests/tui_smoke.py target/debug/ww
python3 scripts/tests/selection_tui_smoke.py target/debug/ww
python3 scripts/tests/subagents_tui_smoke.py target/debug/ww
python3 scripts/tests/delegation_tui_smoke.py target/debug/ww
python3 scripts/tests/setup_smoke.py target/debug/ww
python3 scripts/generate-cli-reference.py target/debug/werk-worker --check
```

The authenticated setup fixture also exercises remembered service reconnects,
workspace-scoped session selection, same-session follow-ups with model history,
crash/restart with a pending approval, and retry/new-connection choices after a
server disappears. It verifies stable session/run/request IDs and no replay of
committed tool calls. It also checks that connection setup omits global backend
probes and runs 20 immediate chat/setup/connection transitions to catch lost
keyboard input between screens. The subagent PTY test reopens the parent conversation and
checks that completed child results remain inspectable.

The scheduler benchmark uses eight simulated 100-ms requests and five repeats
for unknown/1/2/4 slots. It needs no model and must not be called GPU throughput.
Record raw JSON, environment, exact command and validation logs alongside the
summary; keep baseline and later runs distinct. See the
[2026-10-19 experiment record](benchmarks/2026-10-19-subagents/README.md).
Context tests verify rolling compaction within a single long tool run, follow-up
turns after a large final code response in the same session, preserved
constraints/tool pairs, server-sized reserves, oversized input rejection before
dispatch, learned smaller limits and nonretryable truncated tool output.
`tests/context_planning.rs` additionally bounds count requests for a 120-read
history, checks one-count continuation after durable resume, invalidates changed
prefixes, rechecks smaller limits, and stops ineffective compaction without
changing canonical history. Its ignored saved-session replay requires explicit
`WW_REPLAY_DATABASE`/`WW_REPLAY_SESSION` or a private `WW_REPLAY_SNAPSHOT` JSON file;
it opens the database read-only and uses an offline approximate counter, never
credentials or a live model. Run it with
`cargo test --test context_planning saved_session_planning_replay -- --ignored --nocapture`.
The fixture deliberately has no count cache, so every planning estimate is visible.
The TUI PTY test also checks keyboard/mouse scrolling, code-block colors, visible
run errors and follow-ups after failure without creating another session.
Permission regressions cover upgrading a restricted chat while awaiting an answer,
loading the bundled coding skill, writes only after addressed approval, denial,
stale approvals, persistence/restart, hard denies, read-only scopes and actual
Bubblewrap process execution (`REQUIRE_SANDBOX=1` makes missing isolation fatal).
The PTY tests exercise F2 confirmation and typed `ja`, including real writes by
two subagents without preconfigured grants or a coding profile.

## Adaptive harness

`tests/process_input.rs` runs real sandbox checks for literal bounded stdin, EOF,
early exits, blocked-input timeout/cancellation, and approval binding. Its engine
fixture rejects a malformed interpreter invocation before approval and completes
after one corrected script/check call. This measures orchestration steps, not
model speed. `src/repair_cycle.rs` also replays legacy replacement receipts and
excludes invalid command/inspection failures from application repair obligations.
The opt-in `saved_malformed_check_does_not_exhaust_later_application_repair` replay
uses a private `WW_REPLAY_SNAPSHOT` and executes neither tools nor model requests.

Repair-cycle regressions in `tests/work_cycle.rs`, `tests/completion.rs` and
`src/repair_cycle.rs` cover changing diagnostics, edits, unrelated passing checks,
successful fixes, explicit replacement checks and durable receipt replay.
Check phases preserve admitted delegation tools; hard capability exclusions
still apply. Automatic-helper tests cover the configured wall allowance and one
bounded replacement after failure and new parent work.
An optional offline replay of a private session snapshot uses
`WW_REPLAY_SNAPSHOT=/path/to/snapshot.json cargo test --lib saved_session_cannot_reset_repair -- --ignored --nocapture`.
It executes no tools or model requests and proves bounded recovery, not task completion.

`tests/harness.rs` covers explicit legacy migration, quiet operation without Markdown,
scoped activation/removal, duplicate suppression, adaptive resume, stable authority, truncation,
completion/approval/restart boundaries, polling and reproducible whole-request
fixture comparisons. Context, steering and subagent fixtures exercise the adaptive
default. The initial two-mode implementation is recorded historically in
[the 2026-10-21 validation record](benchmarks/2026-10-21-harness/README.md)
for commands, actual measurements and limits. Fixtures prove runtime contracts,
not live model quality. The `untrusted-document` infrastructure eval explicitly
expects the existing `ValidationError/path_denied` contract for absolute host
paths; it does not confuse path validation with a missing permission grant.


Adaptive revision validation (2026-10-21, Linux): 166 Rust test executions with
`REQUIRE_SANDBOX=1` and the installed Bubblewrap backend, no skips; all eight
infrastructure evals; 9 Python tests; CLI migration/default/completion smoke and
six PTY suites (TUI, subagents, permissions, setup, sessions, selection). Formatting,
Clippy with warnings denied, the library without default features and generated
CLI-reference checks also pass. No live-model inference or quality claim.
