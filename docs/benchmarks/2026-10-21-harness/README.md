# Harness validation — 2026-10-21

Historical record of the initial classic/minimal implementation. The current
adaptive default supersedes its activation instructions; these measurements
are retained unchanged and are not claims about the revised implementation.

Worker baseline: `d9b91087976bdb3c1103f6855c23cf2e653558fa`, with this local,
uncommitted patch. Werk public wire reference inspected read-only:
`193f8cd3e3316f8773c6c15b9427dbcf8625640e`. Linux/WSL host, Rust
`1.98.0 (88d9e12ae 2026-08-18)`, Cargo `1.98.0 (797e8a9bc 2026-08-05)`.
No live model, GPU backend, quantization, warmed KV state or generation sampling
was used. All model responses below are deterministic ScriptedProvider fixtures.

## Whole-request fixture measurements

Reproduce with:

```sh
cargo test --locked --test harness compare_classic_and_minimal_contexts -- --nocapture
```

[Raw measurements](measurements.jsonl) include each request's estimate, tool
schema size/count, system size, requests, tool calls, retries, compactions,
status and observed fixture wall time. `estimate_request` measures serialized
wire-shaped UTF-8 bytes plus 256 units of template margin, **not native tokens**.
These values include every message and tool definition for every request, not
only the system prompt. No shifted context is excluded. Profiles, policies,
input, output reserve and scripted actions are identical within each pair.

| Task/profile | Tools / wire schema bytes | Total request estimate classic → minimal | Requests / tool calls (both) | Fixture wall ms classic → minimal |
|---|---:|---:|---:|---:|
| Compare two file observations / general-readonly | 13 / 4,611 | 14,767 → 12,382 | 2 / 1 | 22 → 25 |
| Read, correct addition, inspect saved file / coding-basic | 19 / 7,152 | 48,564 → 38,934 | 4 / 3 | 76 → 89 |
| Read notes and save a ticket preview / automation-basic | 14 / 5,173 | 25,972 → 21,680 | 3 / 2 | 45 → 53 |

Both modes completed all three fixtures with successful tool outcomes. The edit
fixture asserts the exact saved implementation; the preview fixture asserts
that the file exists. Each comparison has zero retries and zero compactions;
separate regression cases exercise these paths. The answers and tool choices
are supplied by the test, so completion is **not** a model-quality result.
Observed millisecond differences are fixture/scheduler noise, not inference
speed measurements. The evidence supports a smaller assembled context; it does
not establish better live reasoning, tool choice, quality or prefix-cache hits.

The bounded catalogue remains fully available and permission-filtered. No
additional MCP/native catalogue was enabled here. These measured baseline sizes
do not justify another tool registry or mandatory discovery workflow.

## Executed checks

- `cargo fmt --check` and `git diff --check`.
- `cargo clippy --locked --all-targets -- -D warnings`.
- `WERK_WORKER_BWRAP=/home/philipbodenbach/.local/share/werk-worker/tools/bubblewrap/usr/bin/bwrap REQUIRE_SANDBOX=1 cargo test --locked --all-targets -- --nocapture`: 160 successful test executions, no ignored tests or sandbox skips. This includes the shared CLI tests compiled for both binaries.
- Targeted `cargo test --locked --test context_policy -- --nocapture` and `cargo test --locked --test werk` after the last summary-provenance/count-source refinements: 12 and 21 cases passed.
- `cargo check --locked --no-default-features --lib`.
- `python3 -m unittest discover -s scripts/tests`: nine tests passed.
- `python3 scripts/tests/{tui_smoke,subagents_tui_smoke,permissions_tui_smoke,setup_smoke,sessions_tui_smoke,selection_tui_smoke}.py target/debug/ww`: each script executed separately and passed.
- `python3 scripts/generate-cli-reference.py target/debug/werk-worker --check`.
- Temporary-workspace CLI smoke for `--harness minimal`, `--harness classic` and `--harness-config`: SQLite inspection confirmed the persisted mode and accepted configured check. The smoke used `skills.list` with the actual array result schema.
- `WERK_WORKER_BWRAP=/home/philipbodenbach/.local/share/werk-worker/tools/bubblewrap/usr/bin/bwrap REQUIRE_SANDBOX=1 cargo run --locked --example eval`: all eight fixtures passed, including real sandbox execution and crash-safe external write receipts.

The first general test invocation skipped the dedicated isolation test because
it explicitly defaults to absent `/usr/bin/bwrap`; the mandatory-sandbox rerun
above used the already installed local binary and passed. No sandbox was
installed or bypassed. The eval initially exposed a stale fixture expectation:
absolute host paths already returned `ValidationError/path_denied` at the
baseline, while the fixture expected policy `Denied`. Its expectation now checks
the exact validation error code; access checks were not weakened.

New/extended regressions cover automatic tool continuation, real steering,
stable minimal authority, transient truncation feedback, successful prior writes,
bounded repeated denials/read observations, live owned-job polling without
writes, restored full skills after compaction, lower-authority summaries,
child inheritance/synthesis, bounded completion repairs, tool/step/request
budgets, addressed real approvals, and five durable completion crash boundaries.
Checks that would write or submit externally are rejected at configuration time.
Persisted unknown outcomes continue to block automatic resume.

Existing Werk fixtures cover native counters, cache eviction/recount, unsupported
fallback, wrong identities/envelopes, old/mismatched telemetry, hybrid image
estimates, smaller-context recovery, and alternative providers. Added assertions
verify exact/hybrid/unspecified count provenance and JSON/SSE numeric timing
retention without backend diagnostic text.

Live inference, native Windows/macOS execution and deployment/package publishing
were not performed. Host permissions and backend limits remain authoritative.
See [the mode and completion contract](../../harness.md) and
[the verified capability matrix](../../reuse-matrix.md).
