# Sessions, persistence and recovery

SQLite schema version 1 stores complete canonical session snapshots, including
durable events, in one transaction with revision CAS. An exclusive sidecar file
lock prevents a second host from opening the same store. WAL and FULL synchronous
mode provide local durability; filesystem/hardware guarantees still apply.
Migration 0→1 creates the table; future versions and corrupt databases fail
explicitly. `InMemoryStore` provides identical CAS behavior without durability.
SQLite/filesystem work runs on blocking-pool workers. Commit publication is
shielded from caller cancellation.

One session has one active mutating run; follow-ups are persisted FIFO, maximum
16. Separate sessions run independently, but shared workspace identities are
reserved exclusively while active. User-provided metadata may contain optional
external references; no ticket/Git fields are required by core.

States: queued → running → waiting_approval/waiting_input → running. Successful
work completes; provider/budget/invariant failures fail. Cancellation moves
through cancelling to cancelled only after local cleanup. Process loss yields
interrupted. Unknown external outcomes are a separate invocation outcome and
block automatic continuation. `wait` never equates a pending request with success.

Barriers: accepted command + run/queue; completed model message + tool intents;
prepared arguments + approval; running intent before side effects; outcome +
event before model continuation; terminal result + event after local job cleanup.
The original model history is canonical, not reconstructed from UI strings.
Tool groups are projected in original call order using assistant-message indices,
so identical provider call IDs in later steps cannot corrupt pairing.

| Persisted boundary after a crash | Recovery |
|---|---|
| Prepared intent, no start | Resume rechecks tool version, policy and exact preparation |
| Pending approval/input | Same request IDs and choices remain answerable through host API |
| Running without outcome | `outcome_unknown`, including reads conservatively; no blind retry |
| External effect before receipt commit | Block; inspect external state and record host reconciliation |
| Outcome committed | Reproject stored result, never execute again |
| Final model response committed | Complete without a new model request |
| Background local process lost | Do not adopt or kill a stored numeric PID; classify uncertain execution |

`reconcile_invocation` records a confirmed success/failure receipt without an
action replay. Historical unknown events remain. Resume is allowed only after
all uncertain invocations are resolved. This is not external exactly-once
delivery: a remote service may commit after disconnection. No idempotency-key
guarantee is invented for the sample ticket API.

Commands dedupe for the lifetime of the stored session (limit 10,000), keyed by
command ID and canonical payload hash including method/run. Same payload returns
duplicate and original run ID; different payload returns command_conflict.
Two distinct invocation IDs with identical arguments are distinct requested
actions. Command, event, step, attempt, invocation and provider call IDs are
separate.

Resume checks host principal and pinned workspace device/inode; missing/replaced
roots conflict. File contents persist in the explicitly allocated workspace,
not an automatically recreated empty mount. Individual writes verify their
base hashes. Session limits are 100,000 events and 64 MiB SQLite snapshot data.
Fork requires settled tool groups, new identities and explicit new policy;
it copies conversational history, not active grants, jobs or requests.

Graceful `close` cancels the engine and reaps owned processes/jobs. `Drop` offers
no asynchronous cleanup guarantee; embedding hosts must call `close().await`.

Werk inference transport failures are not automatically retried. A timeout,
broken stream, or cancellation before a terminal response does not prove that
the server stopped its work. The affected provider connection blocks subsequent
inference and token-count requests with `inference_unresolved`, including work
from other sessions sharing it. Check or restart the backend and reconnect before
continuing. Existing work and history remain saved; reconnecting is not itself
proof that an old server request has stopped. This protection is connection-local,
not a cross-process server lock.

Explicit HTTP rejections such as 429/503 retain bounded retries. Completed
responses, including a reported output limit, retain ordinary recovery behavior.
A healthy stream can run longer than the configured inactivity timeout while
it continues delivering data.

## Configured completion checks

An optional [harness completion check](harness.md) makes saved final text a
candidate, not proof of acceptance. Normal execution and resume share the gate.
The exact check configuration, candidate, invocation and verdict persist in
`metadata._werkworker_harness[run_id]`. A prepared invocation follows ordinary
approval recovery; a finished one is evaluated without replay; a running one
without a receipt is unknown and requires reconciliation. A failed check's
correction allowance is persisted and remains bounded after restart. Old runs
without this metadata explicitly migrate to a preserved classic contract. New
runs are adaptive; their trigger scopes, episode counts, delivery steps, observation
cursors and cooldowns persist in the same namespace for resume.

Runtime feedback must not become an assistant response prefix. Default requests
ending with assistant content include a fixed continuation boundary, including
retained prose after compaction; observations retain their existing low-authority
role and no tool/document text enters the boundary.
Empty answers or exact echoes of owned feedback are failed attempts, not final
results. Their recovery shares the output-limit retry allowance and is derived
from persisted attempts, including on resume. The boundary participates in the
full request count and admission. See [harness behavior](harness.md).
