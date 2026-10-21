# Architecture decision — 2026-10-19

Use Rust 2024, an asynchronous Tokio library (`werk_worker`) and a thin CLI
installed as `werk-worker` and `ww`, both sharing the same implementation.
The starting repository had only Cargo's hello-world program.
Keep all changes local. Product name: werkWorker. License: Elastic-2.0.

Dependencies flow from CLI/transport to the public SDK, then to contracts,
providers, store, policy and runtime. Tools implement an explicit trait; core
does not know ticket or Git semantics. Skills are immutable data. Profiles
select components, never grant authority.

One serialized transactional state per session, concurrent I/O outside its
lock, CAS revisions in stores, and an exclusive SQLite store owner provide
ordering. Persist model messages and intents before dispatch; persist outcomes
before continuation. A started invocation without an outcome is unknown after
recovery, never silently retried. Workspace operation leases allow concurrent reads/inference and serialize writes
and writable processes, including overlapping directory scopes.

SQLite snapshots and their durable events commit together. This simple v0.1
representation favors auditability over high-volume database efficiency.
SQLite work uses blocking-pool threads. Event clients replay by cursor and wait
on bounded watch notifications, so subscribers cannot stall execution.

Linux bubblewrap is the process boundary: separate namespaces, read-only OS
tools, explicit workspace, empty environment and no network. File access uses
descriptor-relative Linux openat2 with no symlinks or mount crossings. There is
no host-shell fallback. HTTP and authenticated connectors are trusted brokers.
JSON Schema validation is delegated to jsonschema; rusqlite bundles SQLite for
installation portability. No inference or reference-agent runtime is linked.
The optional `tui` feature supplies the ratatui/crossterm CLI client; library-only
consumers can disable default features.

## Parallel orchestration

`parallel` owns validated host limits and workspace leases; `agents` owns durable
child identities, delegation, tree budgets and result collection. `providers`
reports evidence for inference capacity; `api` enforces the shared stream
semaphore. Unknown capacity permits one request. Active-child capacity is
reserved without blocking to avoid nested admission deadlocks. Each session
retains its own ordered model/tool loop and transaction lock.

Parent/child links commit before child dispatch. Completion waits for descendants
and managed jobs; automatic result delivery and its message commit together.
TUI/RPC use the same SDK and addressed child requests. See [subagents](subagents.md)
for the contract, partial-creation recovery boundary and resource limits.

## Adaptive run-bound harness — 2026-10-21

The existing model/tool loop uses one adaptive harness for new runs, with a lean
default and persisted, bounded assistance triggered by runtime observations.
`harness` owns small configuration/state helpers in the reserved session metadata
namespace; `api` shares one completion gate between normal execution and resume.
There is no supervisor or second executor. Explicit completion checks use the
ordinary durable tool path. Stable instructions and lower-authority event data
are separated in `context`. See [activation and contracts](harness.md).
