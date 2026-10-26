# Recovery and scoped-context follow-up validation

Local patch against `3c35c67e8d9aad18034889f838eb3d41fd9a0e86`, 2026-10-21.
No old revision was checked out. No model server, backend probe, cache reset or
change to werk1112 was used. These are deterministic fixtures, not measurements
of live model quality, inference throughput or prefix/KV cache hits.

## Complete request measurements

`tests/project_context.rs::measure_complete_requests_with_real_profiles_and_project_loader`
compares the previous adaptive context with the new scoped adaptive context.
The previous side uses the original bundled coding profile, preserved verbatim
in `tests/fixtures/coding-basic-before.toml`, and the real legacy `Project::profile`
loader. The new side uses the actual bundled `coding-basic` and
`Project::scoped_profile` / `scoped_metadata`. This isolates the context change
inside the patched runtime; it does not run an old binary or simulate the old
recovery collision as if it had completed successfully. Both sides read identical fixture
source files through `Project::load`: root instructions, `a`, `a/deep`, and an
unrelated `b` scope. Priority selects `a/deep/AGENTS.override.md` over its sibling.
Each scoped text is the respective fixture requirement repeated 160 times;
there is no shortened or summarized baseline source. Each run uses the same
100,000-unit context limit, tool catalogue, grants, file content and user task.
The test asserts identical original source hashes/text lengths and tool
catalogues across all four runs. The legacy profile contains every complete
source; the new profile contains no embedded source text.
Specialized startup/input/browser verification guidance is available in the
optional `coding-basic` skill body. Its metadata description is unchanged, and
the profile imposes no automatic skill load. These fixtures do not load that body.

[measurements.jsonl](measurements.jsonl) records every request estimate and
serialized size, the original source manifest, actually loaded sources,
tool-definition sizes, deferred proposals and verification status.
[source-sha256.json](source-sha256.json) identifies the profile files; the baseline
fixture is byte-identical to `git show 3c35c67:profiles/coding-basic.toml`.

Both tasks write `complete`, read it back and report that verification. The
scoped task targets `a/deep/result.txt`; the root task targets `result.txt`.
The new scoped run needs one extra model decision because its original write
was proposed without the applicable rules. That proposal gets a durable
non-execution receipt and consumes the ordinary tool-call allowance. Only the
reconsidered write executes. File contents and read receipts are asserted.
No executable product test is claimed for these trivial write/read tasks.

The estimates below use `estimate_request`: all wire-shaped messages, tool
schemas, response schema and 256 units of template margin. They are conservative
UTF-8 byte estimates, **not native tokenizer counts**. The independent serialized
byte total includes every complete `ModelRequest` field. Source index, complete
loaded rules, skill metadata, call/result history and feedback are all included;
the test compares estimates with the actual saved `ContextSnapshot` values.
The production native Werk counter still receives the fully assembled request.
The raw `tool_schema_bytes` field measures the complete serialized
`ModelRequest.tools` definitions (7,156 bytes per request in this fixture);
the estimate itself uses the provider's wire-shaped tool definitions.

| Task | Context | Estimates for each request | Total estimate | Serialized bytes, total | Model requests | Tool proposals / executed |
|---|---|---|---:|---:|---:|---:|
| Root write | Previous adaptive | 20,928; 21,451; 22,003 | 64,382 | 66,879 | 3 | 2 / 2 |
| Root write | Scoped adaptive | 8,478; 9,001; 9,553 | 27,032 | 29,529 | 3 | 2 / 2 |
| Scoped write | Previous adaptive | 20,935; 21,472; 22,038 | 64,445 | 66,942 | 3 | 2 / 2 |
| Scoped write | Scoped adaptive | 8,485; 17,500; 18,419; 18,603 | 63,007 | 66,253 | 4 | 3 / 2 |

The root task reduces the total estimate by 58.0% (serialized bytes by 55.8%).
The scoped task reduces it by only 2.2% (serialized bytes by 1.0%) after charging
the extra request. A smaller unrelated scope could make the extra decision cost
more than it saves. Root instructions remain complete; processes need all
configured workspace rules. Already needed rules remain loaded through the
session to protect open work. There is no promise of a universal context saving.

The separate collision regression measures all five requests, including both
recovery messages: `[1755, 2269, 11559, 11526, 11775]`, total **38,884** estimate
units and **38,053** complete serialized request bytes, five model requests and
three executed tool calls; see [recovery-measurement.json](recovery-measurement.json).
Both estimate and serialized totals include every request's tool definitions
(1,123 serialized bytes each), and saved context estimates are asserted equal.
It first confirms a
write, then truncates a large file read, then returns `finish_reason: length` on
the very request containing the tool-truncation notice. The following request
contains generation recovery with the exact discarded attempt ID. The previous
write survives; the discarded call never creates its file. The final request
does not reinject the consumed recovery. This is a recovery correctness
measurement, not a claim that the formerly broken behavior completed the task.

Reproduce the full-context figures with:

```sh
cargo test --locked --test project_context measure_complete_requests -- --nocapture
cargo test --locked --test harness truncated_tool_then_limited_generation -- --nocapture
```

## Regression coverage and checks

The new tests cover independent truncation/generation delivery; active and
cooling-down tool notices; interrupted versus consumed delivery on resume;
legacy scope and durable-trigger migration; conservative ambiguous provenance;
preserved counters, cooldowns and historical events; and bounded repeated limits.
Migration coverage includes an already observed generation failure previously
suppressed by the old shared slot, as well as contradictory legacy provenance.

Project tests use the actual bundled profile and secure project loader. They
cover root/disjoint/deeper rules, a deferred batch corrected by a fresh model
decision, absence of early writes, hash and priority preservation, custom and
host/user text, compaction plus resume after an external rule edit, inheritance
on fork, conservative process scope, completion-check rule admission, ordinary
request/tool budget exhaustion, indispensable-rule overflow, and operation
without any optional Markdown files or available skills. Existing tests retain
legacy embedded profiles, real approvals, tool JSON fragmentation, child/tree
limits, native Werk token accounting and external-write recovery coverage.
Additional cases cover nonrecursive directory inspection, equivalent source-path
spellings, host denials and approval order, and the CLI profile editor preserving
saved rules for both scoped and legacy sessions after on-disk source changes.

Passed locally:

- `cargo fmt --check` and `git diff --check`.
- `cargo clippy --locked --offline --all-targets -- -D warnings`.
- `cargo test --config /tmp/werkworker-followup-checks.toml --locked --offline --all-targets --no-fail-fast -- --nocapture`: **181 passed** in 26 suites, no failed, ignored or skipped sandbox cases.
- `python3 -m unittest discover -s scripts/tests`: **9 passed**.
- `python3 scripts/tests/setup_smoke.py target/debug/ww`: **passed**, including real PTY setup, separate snapshotted root rules in authenticated requests, approvals, saved sessions, reconnect and terminal restoration.
- `python3 scripts/tests/permissions_tui_smoke.py target/debug/ww` and
  `python3 scripts/tests/sessions_tui_smoke.py target/debug/ww`: **passed** with
  real PTYs and temporary workspaces.
- `cargo run --config /tmp/werkworker-followup-checks.toml --locked --offline --example eval`: **8 passed**, including real sandbox execution and unknown external-effect recovery.

The temporary Cargo config sets `REQUIRE_SANDBOX=1` and
`WERK_WORKER_BWRAP=/home/philipbodenbach/.local/share/werk-worker/tools/bubblewrap/usr/bin/bwrap`.
This uses the existing namespace sandbox; no isolation fallback was introduced.
The same settings can be supplied as environment variables on another host.
[checks.json](checks.json) records commands, statuses and the test sandbox settings.

Not run: live-model comparisons, Windows/macOS/ARM native CI, release/package
installation checks and the other full CI PTY scenarios. Some initial outer
commands could not start because the command sandbox rejected its existing
`.aws` symlink layout. The checks listed as passed subsequently executed; the
application's namespace sandbox was retained and required for sandbox tests.
