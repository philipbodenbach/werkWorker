# Parallel agents and server limits — experiment record, 2026-10-19

Scope: implement parallel parent/child orchestration, conservative Werk capacity
checks, default rolling compaction and enforced context/output limits. The
request also required reproducible evidence and updated README/reference docs.
Changes remain local and uncommitted. No release action, remote CI, commit, PR,
push, model download or server reconfiguration was performed.

## Provenance and environment

[environment.json](environment.json) records compiler, platform, base revision
and the separate Werk reference revision. [source-sha256.json](source-sha256.json)
identifies the final local implementation/tests/examples used for final checks;
it is not a claim that these changes exist in a published Git commit.

Environment: Linux x86_64 on WSL2, kernel 6.18.54.1, rustc/cargo 1.98.0,
unoptimized dev/test builds. The benchmark uses the default in-memory store;
it does not measure SQLite throughput. Real isolation uses Bubblewrap 0.6.1
from the previously extracted Ubuntu package under `/tmp`, with
`REQUIRE_SANDBOX=1`. The outer tool sandbox restricts loopback/netlink/namespaces;
local HTTP and real-isolation tests ran with execution approval outside it.

`ss -ltnp 'sport = :11434 or sport = :11435'` outside the outer sandbox showed
no listeners. No live Werk inference, GPU timing, tokenization-quality evaluation,
native Windows/MSVC build or native macOS build is claimed.

## Design experiments and corrections

| Stage | Observation / decision | Evidence or resolution |
|---|---|---|
| Source inspection | Ordinary capability discovery does not prove inference parallelism | Inspected Werk deployment routing/diagnostics and protocol observability; recorded revision in [reuse matrix](../../reuse-matrix.md) |
| Admission design | Reserving an entire workspace per run prevents independent subagents | Replaced run exclusion with shared read / exclusive write/process operation leases; background jobs retain leases |
| Child orchestration | Parent must remain free to work or await children without holding an inference slot | Separate session loops; global stream semaphore, active-child permits, tree-wide request budget |
| Initial overlap tests | Parallel children overlap; unknown capacity serializes inference | Six initial subagent cases passed; later expanded to ten including nesting/idempotency/revocation/reopening |
| Initial HTTP test attempt | Loopback bind failed with PermissionDenied in the outer sandbox | Repeated outside it; HTTP fixtures passed. This was an environment failure, not a passing HTTP test |
| Initial compile/lint corrections | Agent tool name required dereference; fixture helper used a nonexistent constructor; process-start signature exceeded Clippy argument limit | Fixed tool registration and fixture constructor; introduced `ProcessJob`/`start_managed`, preserving old public `start` |
| Further lint corrections | Nested format, collapsible conditional, test MutexGuard across await | Fixed formatting/conditional and cloned test request snapshots; final strict Clippy passes |
| TUI review | Root approval events could replace a displayed child request without its addressed handle | Pending request and owner now refresh together from snapshots; root events do not overwrite child routing |
| Summary sizing | Many children could generate excessive synthesis input | Bounded child goal/response summaries; full evidence remains in child sessions |
| Token-limit follow-up | Provider context was not enforced at every request; old compaction only covered prior runs | Added wire-shaped sizing and preflight for all attempts; automatic extractive compaction within long runs; reserves clamped to server context |
| Server evidence boundary | `/v1/models` has no effective context; aggregated multi-slot context is not a minimum | Deployment per-request limit, single-slot protocol telemetry or explicit host context; unknown context fails before chat |
| Rejection behavior | Retrying an identical oversized/truncated request wastes budget | Smaller structured server limit triggers rebuild; missing smaller evidence or output truncation is nonretryable |
| Compaction review | A short group's excerpt can exceed its original size | Skip size-increasing replacement; retain all constraints and the latest complete tool group |
| Recovery review | Direct child resubmit/resume could bypass active-child admission | Children are one-shot; general APIs reject extra runs/resume; durable reopening fixture verifies no redispatch |
| Generated docs | Clap blank lines introduced trailing whitespace | Generator now strips line endings; regenerated CLI reference and checked it |

The first scheduler series is retained as [scheduler.json](scheduler.json), the
post-context series as [scheduler-context.json](scheduler-context.json), and the
final series as [scheduler-final.json](scheduler-final.json). No series has been
selected or removed based on speed. Early compile/lint error transcripts were
not archived individually; the table records their actual causes and fixes.
Final command output is retained in the files below.

A later uninstrumented repeat stalled (observed process elapsed time 6:27; it
was subsequently terminated with SIGTERM, exit 143). It emitted no JSON, so no
timings are attributed to it: [scheduler-aborted.json](scheduler-aborted.json).
An instrumented diagnostic rerun completed; both its
[raw timings](scheduler-diagnostic.json) and [phases](scheduler-diagnostic.log)
are retained. Inspection identified a mutex scheduling hazard: a timer branch
awaited the session mutex while its paused `select!` sibling could own the next
queued permit. A deterministic unit fixture reproduces that ownership condition.
Timer checks now use `try_lock`, allowing the sibling to progress; both model
and tool execution loops use the correction. No stack capture of the terminated
process exists, so attribution of that particular stall to this hazard remains
an inference. The final benchmark includes phase logs and ten-second submission,
wait and close guards so a new stall fails visibly instead of hanging forever.
Final and additional repeat series below were run after this correction.

## Functional validation matrix

| Concern | Automated coverage |
|---|---|
| Independent work | Child overlap and separate conversation history; shared-workspace independent sessions |
| Graceful capacity | Unknown capacity peaks at one; deployment aliases sum only matching distinct profiles; missing/403/invalid diagnostics; fresh/stale/wrong-model/fractional slot telemetry |
| Limits and ownership | Shared root request budget, global child capacity, depth, restricted tools, foreign child IDs, command idempotency/conflicts |
| Cancellation | Root/child/grandchild cancellation, queued inference and released permits |
| Workspace safety | Stale hash writer conflict; parent revocation denies child writes; overlapping path leases/read sharing and retained job lease |
| Persistence | SQLite child links reopen; explicit interrupted child fixture does not redispatch; extra child runs/resume denied; existing unknown-effect and subprocess recovery suite remains green |
| Context | Ten reads in one run with automatic compaction; intact tool pairs and user constraint; raw messages retained; every fixture request fits 8,192 tokens by conservative sizing |
| Server limits | Smaller output reserve, oversized multilingual constraint blocked before provider; deployment minimum and unknown-context rejection; HTTP 400 lower limit learns/rebuilds; truncated tool output never executes or retries |
| RPC | Discovery, child delegation/list/steer/cancel, deduplication, foreign-child rejection and tree cancellation |
| TUI | Real PTY streams, root write approval, interaction answer, slash completion, Ctrl-C reset/double exit, restored terminal; per-binary rendering unit tests |
| Isolation | Required real Linux Bubblewrap test, host sentinel/network/descriptor confinement, timeout/descendant cleanup and existing broker tests |

A native child-approval PTY journey, actual process kill specifically during
multi-session child creation, dynamic live server reconfiguration and hostile
external concurrent writers were **not** exercised here. The SQLite interrupted
child fixture is state-injection evidence, not a real crash test. Existing real
subprocess recovery tests cover the general harness separately.

## Commands and retained output

Run from the repository root. Substitute your usable Bubblewrap path.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
WERK_WORKER_BWRAP=/tmp/werk-worker-study/bubblewrap/usr/bin/bwrap \
  REQUIRE_SANDBOX=1 cargo test --all-targets
WERK_WORKER_BWRAP=/tmp/werk-worker-study/bubblewrap/usr/bin/bwrap \
  REQUIRE_SANDBOX=1 cargo run --quiet --example eval
cargo check --no-default-features --lib
cargo build --bins
python3 scripts/tests/tui_smoke.py target/debug/ww
python3 scripts/generate-cli-reference.py target/debug/werk-worker --check
cargo run --quiet --example parallel_benchmark > scheduler.json
```

- [tests.log](tests.log): full test execution, including required real isolation.
- [clippy.log](clippy.log): all-target strict lint check.
- [library-check.log](library-check.log): library without optional TUI dependencies.
- [tui.log](tui.log): real PTY result.
- [eval.jsonl](eval.jsonl): eight existing harness evaluations and observed metrics.
  The [pre-mutex-correction run](eval-before-mutex-fix.jsonl) is retained separately.
- [checks.json](checks.json): final exit/result summary; timing is not an acceptance threshold.

## Scheduler benchmark method

[parallel_benchmark.rs](../../../examples/parallel_benchmark.rs) creates eight
sessions in one workspace and submits one request per session. A fake provider
waits 100 ms per response; it advertises unknown, 1, 2 or 4 slots. The host cap is
4. Each case runs five times in that order, with fresh engine/workspace/metrics.
Timing covers submission through all terminal results; setup/build is excluded.
There is no warmup, randomization or statistical significance claim. Atomic
stream guards record observed peak overlap and release on drop. Tool writes,
model execution, network, tokenizer, GPU and real server scheduling are absent.

The result demonstrates admission/overlap behavior under controlled latency.
It does not predict an LLM's tokens/s, quality, VRAM usage, rate limits or scaling.
Subagent functionality is established by the tests above; this timing benchmark
measures the shared session scheduler rather than a model-led delegation plan.

## Final measurements

| Reported slots | Effective slots | Median ms | Min–max ms | Peak in flight | Relative to 1 slot |
|---|---:|---:|---:|---:|---:|
| unknown | 1 | 822.73 | 817.63–825.45 | 1 | 1.00× |
| 1 | 1 | 822.83 | 820.68–825.73 | 1 | 1.00× |
| 2 | 2 | 414.95 | 413.56–417.86 | 2 | 1.98× |
| 4 | 4 | 210.73 | 209.64–211.82 | 4 | 3.90× |

Two additional guarded repeats each completed all 20 cases (160 requests per
series). Both retained the configured peak of 1/1/2/4. Raw evidence:
[repeat 1](scheduler-repeat-1.json), [phase log 1](scheduler-repeat-1.log),
[repeat 2](scheduler-repeat-2.json), [phase log 2](scheduler-repeat-2.log).
Together with the final series this is 60 post-correction cases / 480 requests.
These bounded repeats reduce the regression risk; they are not proof that no
possible scheduling interleaving can stall.

Final validation: **65 test executions passed**, zero failures/ignored cases,
including ten subagent integration cases and the mutex regression. Eight harness
evals passed; strict Clippy, formatting, library-only check, generated CLI-reference
check and the PTY smoke passed. The workflow remains local and uncommitted.
