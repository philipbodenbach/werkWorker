# Testing and evals

Normal checks require no GPU, network service, API key or live Werk instance.
Local HTTP fakes use loopback listeners. Run:

```sh
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo run --example eval
```

The Linux isolation job must set `REQUIRE_SANDBOX=1`; it tests a real backend.
A local missing backend prints an explicit SKIP reason. A green unit suite with
that skip is not isolation proof. Example:

```sh
WERK_WORKER_BWRAP=/usr/bin/bwrap REQUIRE_SANDBOX=1 cargo test --test isolation
WERK_WORKER_BWRAP=/usr/bin/bwrap REQUIRE_SANDBOX=1 cargo run --example eval
```

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
The TUI PTY test also checks keyboard/mouse scrolling, code-block colors, visible
run errors and follow-ups after failure without creating another session.
Permission regressions cover upgrading a restricted chat while awaiting an answer,
loading the bundled coding skill, writes only after addressed approval, denial,
stale approvals, persistence/restart, hard denies, read-only scopes and actual
Bubblewrap process execution (`REQUIRE_SANDBOX=1` makes missing isolation fatal).
The PTY tests exercise F2 confirmation and typed `ja`, including real writes by
two subagents without preconfigured grants or a coding profile.

## Adaptive harness

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
