# Bounded, durable context planning

See [results.json](results.json) for raw aggregate measurements and
[the reference study](../../context-planning-study.md) for harness/protocol rationale.

| Workload | Previous planning | Optimized first planning | Optimized continuation |
|---|---:|---:|---:|
| 120 completed file-read groups | 235 estimates | 4 | 1 after serialized resume and new steering |
| Same frozen saved session, 265 messages | 120 each time | 6 | 1 |

These are counter-invocation measurements from deterministic local fixtures,
not llama.cpp tokens, model-quality results or observed live latency. The offline
counter has no memoization. The separate user-supplied server log establishes the
real latency of the previous repeated counting, but was not replayed against a
live model. No project API key or authenticated server request was used.

The saved session was read through SQLite's read-only mode into a private temporary
snapshot. Both versions consumed that identical snapshot. No private transcript is
checked into this repository. Tools were empty in this sizing fixture; the real
protocol tool/image/reasoning cases are covered separately by `tests/werk.rs`.
The saved-history optimized selection omitted one additional old assistant/tool
group (255 versus 253 messages). Current-work protection and constraints were
unchanged. All original messages remain canonical.

Reproduce synthetic regressions:

```sh
cargo test --offline --locked --test context_planning -- --nocapture
```

Explicit private offline replay:

```sh
WW_REPLAY_SNAPSHOT=/path/to/private-session.json cargo test --offline --locked \
  --test context_planning saved_session_planning_replay -- --ignored --nocapture
```

Tests additionally cover lower limits after checkpoint reuse, source-prefix
changes, legacy snapshot deserialization, ineffective compaction and reuse without duplicate model summaries. Existing
Werk fixtures exercise native counting, image fallbacks and temporarily growing
compaction candidates, so an initial cost increase cannot prematurely stop a
later useful reduction.

Validation on Linux: 246 Rust tests passed with `REQUIRE_SANDBOX=1` and the real
Bubblewrap backend; the explicit private replay is ignored in the default suite
and passed when invoked separately. Nine Python tests, TUI/effort PTY suites,
format checking and Clippy with warnings denied passed. No live-model test was run.
