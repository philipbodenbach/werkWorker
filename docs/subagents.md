# Parallel execution and subagents

werkWorker can orchestrate background child sessions while the parent continues
working. All bundled profiles expose `delegate_task`, `agents.status`,
`agents.wait`, `agents.cancel` and `agents.message`. The model can delegate; SDK/RPC hosts can also delegate explicitly.
Delegation defaults to **Active**. F5 → **Agents** offers four levels:

| Level | Model guidance and automatic helper |
|---|---|
| Off | No new children, including explicit SDK/RPC delegation and automatic helpers. Existing children continue; controls remain available. |
| Conservative | Delegate selectively; start one read-only review after a file change and a current passing behavioral check. |
| Active (default) | Prefer independent investigations and reviews alongside implementation; start one read-only review after the first actual file change. |
| Max | Split substantial independent work early; a read-only acceptance-check planning helper may start as soon as a substantial plan exists. |

Automatic helpers require at least three plan steps with remaining work, two
verified configured model slots, no previously delegated child, available read
tools and permission/capacity for delegation. No level creates busywork or
guarantees that the model will delegate on every request.
The reviewer receives the requirements, recent user corrections, current plan and
bounded file observations (historical paths/hashes) and can inspect files. Automatic
reviews have a small finite budget even when their parent runs continuously;
explicitly delegated tasks retain their inherited limits. A review that exhausts
its budget is reported as incomplete and is not automatically started again.
Automatic review ceilings are six model requests, eight tool calls, eight steps
and three minutes, further reduced by tighter inherited limits and the parent
deadline. They share the existing tree request budget; admission leaves room for
parent work but does not reserve requests against concurrent consumption.
Completed child results arrive at the parent's next model decision while work
continues, without waiting for running siblings. Automatic delivery occurs once
per child and remains recorded across resume. Review observations may describe
an earlier file revision; the parent must check them against its current work.
It cannot write files, execute processes, or replace the parent’s behavior tests. Children run in the same
engine, not by spawning another CLI binary.

The model's `delegate_task` description includes the effective configured inference
capacity without requiring a separate status call. It recommends at most one helper
with two slots. With more slots, Conservative recommends at most two helpers,
Active at most four, and Max the remaining configured capacity after allowing
one slot for the parent. Host child and depth limits always apply. This includes
existing automatic reviews. The advice is to assign
an independent, bounded investigation/review and continue separate work rather than
immediately waiting or polling. Small and sequential tasks should remain local.
This is a recommendation, not a reserved inference slot or an increase in permissions;
explicit delegation still follows the existing scheduler and policy. `agents.status`
also reports execution capacity. Busy counters stay out of the tool description so
they do not change the prompt prefix on every admission. More slots do not guarantee
more GPU throughput or shorter task completion time.

This design uses the goal/context and isolated-conversation pattern described
in [Hermes delegation](https://hermes-agent.nousresearch.com/docs/user-guide/features/delegation/).
It is an independent Rust implementation, not a Hermes runtime integration or a
compatible implementation of every Hermes option. No Hermes source was copied.

## Start and configure

Set `"delegation_level": "off"`, `"conservative"`, `"active"` or `"max"` in
`.werkworker/config.json` to fix the strategy for new sessions. An explicit
project value always overrides the F5 preference. Missing or `null` uses the UI
preference, or **Active** if none was saved. F5 shows both the preference and
the effective project override; saving settings preserves that explicit value.
Saved workspace preferences live separately with host workspace settings.
Existing sessions retain their project snapshot; start a new session to load
manual file changes. Children inherit the resolved configuration.

`SessionHandle::set_delegation_level` changes the host preference, including
during a run. It cannot override a project value or increase execution limits,
permissions or budgets. Changes affect subsequent model requests/admission;
an in-flight request keeps its original prompt. Turning delegation Off blocks
new children, including descendant admissions, without cancelling existing ones.

```sh
ww --werk-config examples/werk-config.json \
  --execution-config examples/execution-config.json diagnose
ww --werk-config examples/werk-config.json \
  --execution-config examples/execution-config.json \
  --grants examples/readonly-grants.json --workspace /path/to/workspace \
  tui --profile general-readonly \
  "Delegate two independent reviews: architecture and documentation. Collect both results."
```

Use `/agents` or F3 to open the live subagent inspector. Up/Down selects a child,
Page Up/Down scrolls its model text and tool results, and Esc returns to chat or
the pending request. Completed child results and errors remain inspectable.
Use `/permissions` or F2 to inspect current tools and host grants.
The TUI shows effective inference capacity,
child goals/statuses and pending child approvals/questions. Ctrl-Y/Ctrl-N and
answers address the displayed child's exact request. Ctrl-C cancels the parent
and descendants; a second consecutive Ctrl-C exits after cleanup.

| Host option | Default | Valid range / meaning |
|---|---:|---|
| `max_model_requests` | 4 | 1–64 concurrent requests across the entire engine, further reduced by provider evidence |
| `max_subagents` | 4 | 0–32 active children across all roots and depths |
| `max_children_per_run` | 8 | 0–64 total direct children per run, including finished ones |
| `max_depth` | 1 | 0–4 child levels; 1 means parent plus leaf workers |

Omitted fields use defaults; unknown fields and invalid values fail startup.
Zero child capacity/depth disables delegation. The similarly named
`Limits.max_model_requests` is a **total request budget for one root tree**, not
a concurrency setting. Children, retries and context compaction share that
budget. Each run also retains its own limits/metrics. No budget is reserved for
final synthesis, so size the tree's total budget for workers plus synthesis.
Children cannot outlive an explicitly configured parent wall deadline. The explicit
continuous mode removes aggregate cutoffs; see [continuous work](harness.md).

## Werk capacity discovery

The normal WerkProtocol `/werk/v1/info` and `/werk/v1/capabilities` handshake is
still required. Optional capacity discovery follows it:

1. Prefer `max_concurrent_requests` from advertised
   `GET /werk/v1/chat/limits?model=MODEL`. This is configured capacity for the
   selected model, independent of current activity or observability sampling.
2. Fall back to fresh WerkProtocol observability: accept matching schema-1,
   available llama.cpp entries with positive `gauges.slots_total`; use the minimum
   across matching workers. Ignore unrelated, stale, or malformed samples.
3. At connection time, deployment diagnostics can provide configured `parallel`
   counts for distinct matching deployments when protocol capacity is absent.
4. Unknown capacity defaults to one inference request. The host concurrency
   limit always remains an upper bound. `parallel_tool_calls` and busy/free slot
   counters are not evidence of configured inference capacity.

`diagnose`, `engine.execution_status()` and RPC `execution.status` expose the
source, explanation and effective slots. Built-in scripted fixtures advertise
parallel capacity only for offline harness testing. Third-party providers can
implement `ModelProvider::concurrency()`; its default is unknown/one slot.

Capacity is refreshed before inference admission, at most once every five
seconds, from the limits endpoint and then observability when necessary. Probes
have two/four-second timeouts. A higher reported limit wakes waiting requests;
a lower limit lets active requests finish and restricts new admissions. Optional
probe failures retain the last established capacity; an explicit unknown value
from a valid limits response falls back to one slot without fresh telemetry. This is not load balancing
or a promise of GPU capacity reserved from other clients. Probes never load a
model or start inference. See [wire provenance](reuse-matrix.md).

Context size is checked independently from concurrency. A server with unknown
context requires explicit `capabilities.context_tokens` before inference.
Automatic rolling compaction applies to parent and children alike; see
[server limits and compaction](context.md).

## Delegation contract

Example model tool arguments:

```json
{
  "tasks": [
    {"goal": "Review architecture", "context": "Read src/api.rs and docs/architecture.md; report evidence", "tools": ["files.read", "files.list"], "read_only": true},
    {"goal": "Check documentation consistency", "context": "Compare README and docs/subagents.md", "tools": ["files.read"], "read_only": true}
  ]
}
```

A batch has 1–32 tasks, subject to available host limits. Goal/context are
bounded to 8,192/16,384 bytes at the SDK boundary. Tools may only narrow the
parent profile; omission inherits it. Children inherit profile instructions,
pinned skills, workspace and host grants, but start a fresh
conversation containing only the supplied goal/context. They receive no copy
of the parent's conversation, prior tool results or reusable approvals.
By default they also inherit the parent's provider/model. The optional project
setting `subagent_model` selects a different model on the same Werk connection
for every child, including automatic reviews and descendants. The selection is
saved with each child and restored on reopen. See [project configuration](project-configuration.md).

`read_only` removes workspace write permission; it is **not** a blanket denial
of an independently granted network-write connector. Narrow `tools` as well
when a task should be entirely observational. At maximum depth, child profiles
lose all delegation/control tools. The project setting is host-applied; the
`delegate_task` arguments do not select a model. No child selects a new workspace,
credential, principal or sandbox. Ancestor cancellation, expiry and hard policy
denials are checked before new tool execution, including live grant revocation.

Batch admission reserves all active-child slots before starting children. If
capacity is unavailable it returns `agent_capacity` immediately instead of
waiting while holding nested parent slots. A nesting limit returns `agent_limit`;
an expanded tool scope returns `agent_scope`. The orchestrator can collect
current work and retry with a new command ID. No scheduler fairness guarantee
is made for workspace access.

`delegate_task` returns child identifiers promptly. `agents.status` returns
compact statuses; `agents.message` steers a directly owned active child, and
`agents.cancel` cancels it and its descendants. `agents.wait` waits without an
inference slot and returns bounded results. Before an ordinary final answer,
the engine automatically joins outstanding children and asks the parent model
to synthesize them. Child failure is reported as task data; it does not become
a successful child result. A failed/cancelled parent cancels outstanding children.

Automatic synthesis stores child delivery markers and its result message in
one transaction. Summaries truncate goals to 160 characters and each response
to at most 4,096 characters, with a shared 8,192-character response budget.
Full results remain in child sessions. Child summaries are labeled untrusted
and cannot authorize tools or widen permissions. `agents.wait` uses normal tool
intent/outcome recovery; an interrupted unknown outcome is never replayed
silently, even if its delivery markers were already persisted.

## Concurrency and workspace safety

A shared host admission limit and adjustable per-model limits cover every
provider stream, from admission through completion/drop. Children targeting the
same model share its discovered slots; different models still share the host
limit and the root tree's request budget. Waiting for children, approvals or tools does not consume an
inference slot. Separate sessions can overlap inference and safe reads even in
the same workspace. Tool calls within one session remain ordered.

Engine-owned operation leases cover canonical overlapping workspaces, including
parent/child directories. Reads share access; writes and writable processes
hold exclusive access from preparation through approval and execution. A managed
background process retains its lease until actual completion/cleanup. Cancel a
long-running background job before requesting a conflicting operation in the
same run. Expected file hashes reject stale concurrent edits (`write_conflict`).

These are cooperative **single-engine** leases, not interprocess file locks or
Git transactions. Separate Worker engines and external editors are outside the
lease manager. No automatic worktrees, merges, per-file locks or rollback are
implemented. Parallel inference does not make writes in a shared workspace
parallel. A host can allocate disjoint workspaces to independent SDK sessions.
Linux file/process isolation requirements remain unchanged; native macOS and
Windows packaging does not imply a native execution sandbox.

## Persistence, recovery and host control

Schema 1 adds defaulted `SessionData.parent`, `Run.children` and
`Run.tree_model_requests`; existing snapshots remain readable. Parent links,
root identity, child command IDs, task fingerprints and delivery state are
stored durably. Child IDs are generated by the host. Control through a parent
handle verifies both sides of ownership, so a model cannot address another root.

A repeated delegation command with the same complete batch returns existing
children; changed payloads conflict. A storage failure during a multi-child
creation can leave a partial batch or an unstarted child record. It is not an
atomic multi-session transaction: retry returns `command_conflict` rather than
silently duplicating already-started work. Inspect the durable sessions before
choosing an explicit follow-up task.

Opening a store recovers unfinished work under the existing interruption rules.
Children are not automatically relaunched after a crash. A child recorded before
submission can have no run and is reported as not started; interrupted children
are collected as interrupted. Root resume/reconciliation is an explicit host action,
not proof of external exactly-once execution. Children are one-shot: direct child
resume returns `agent_recovery`, and an extra child-session run returns
`agent_one_shot`. Reconcile uncertain effects first, then explicitly delegate a
new task from an active parent. This prevents bypassing child admission through
the general session APIs. See [recovery](recovery.md).

SDK hosts use `run.delegate`, `run.subagents`, `run.subagent(id)` and existing
addressed approval/interaction methods. RPC uses the methods in
[the protocol reference](protocol.md). The local trusted host can open a child
session and subscribe to its own durable events; parent events include
`subagent` lifecycle notices. Per-run token/timing metrics stay local;
`tree_model_requests` is the aggregate request count, not an aggregate cost.

## Evidence and limits

[Experiment record and raw benchmark data](benchmarks/2026-10-19-subagents/README.md)
cover simulated concurrency, fallback, ownership, cancellation, nesting, shared
budgets, revocation and write conflicts. These are harness/HTTP-fixture tests,
not model quality or GPU throughput measurements. No live Werk inference was
available in the recorded environment. Native Windows/macOS execution and a
live child-approval PTY scenario were not established by those original Linux results.
The subsequent `scripts/tests/subagents_tui_smoke.py` regression exercises two
children, inspector selection, addressed approvals/questions and completed results
against a local HTTP fixture, without a real model.

## Run-bound harness inheritance

A child inherits its parent's persisted harness mode and local-model guidance,
not the reopening engine's defaults. Root completion checks are specific to the
root goal and are not applied to child goals. In adaptive and preserved minimal runs automatic child
delivery uses a low-authority assistant data envelope with separately persisted
harness provenance, rather than appearing as a new user request. The parent
continues through the same synthesis loop and tree budget. See [harness](harness.md).
