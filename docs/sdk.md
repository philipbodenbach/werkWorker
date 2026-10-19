# SDK and events, schema 1

The authoritative contracts are `src/core.rs`, `src/api.rs` and Rustdoc. Public
operations return `Result<T, WorkerError>` with a stable machine-readable code.

| Operation | Result and meaning |
|---|---|
| `WorkerEngine::builder(Arc<dyn ModelProvider>)` | Explicit composition, no import-time effects |
| `.store(...).tools(...).skills(...).sandbox(...).artifacts(...).context(...).principal(...).build().await` | Open a host with chosen components |
| `engine.create_session(profile, workspace, policy, metadata).await` | Pinned profile/skills, workspace identity, host principal |
| `engine.open_session(id).await` | Recover durable state; no automatic external dispatch |
| `engine.list_sessions().await` | Stored session IDs |
| `session.submit(input, command_id, limits, output_schema).await` | Early `RunHandle`, busy if same session active |
| `session.follow_up(input, command_id, limits).await` | Persist a run in FIFO order; queue limit 16 |
| `session.run(run_id).await` | Address a persisted run |
| `run.events(after_cursor)` | `Stream<Item=Result<WorkerEvent>>` scoped to that run |
| `session.events(after_cursor)` | Session replay/live stream; cursor is session-wide |
| `run.wait().await` | Terminal result after outcomes, local job cleanup and final commit |
| `run.status().await` | Run snapshot, pending requests, attempts, outcomes and metrics |
| `run.steer(input, command_id).await` | Queue input before next model request; current batch finishes |
| `run.cancel(reason, command_id).await` | Signal cancellation; use `wait` for cleanup completion |
| `run.respond_to_approval(request_id, decision, command_id).await` | Address one exact durable request |
| `run.respond_to_interaction(request_id, answer, command_id).await` | Supply one durable answer |
| `engine.resume_run(session_id, run_id).await` | Explicitly resume interrupted safe work |
| `session.reconcile_invocation(run_id, invocation_id, confirmed_result, command_id).await` | Host records external evidence, never replays the action |
| `session.revoke_tool(name, command_id).await` | Remove grants and hard-deny new starts |
| `session.snapshot()/export().await` | Versioned canonical data, no provider/broker credentials |
| `session.fork(explicit_new_policy).await` | Independent identities/history at a completed boundary |
| `engine.close().await` | Cancel active work, await managed cleanup, flush and close store |

`ApprovalDecision` is `AllowOnce`, `Deny` or `AllowSameArguments`. Reusable
approval requires `HostPolicy.allow_similar=true`. Its deterministic scope is
the same tool name/version and identical prepared JSON in the same run,
including file base/diff or process sandbox descriptor. It never covers another
run. `FullAccess` is a host-selected approval mode within existing grants.

Every event carries schema version, opaque event/session/run IDs, monotonic
session cursor and Unix-millisecond timestamp. `EventKind` is a typed Serde
tagged union (`type`, optional `data`). Families cover session creation/recovery,
commands, run states, steps, model requests/deltas/completion/failure, tool
intents/starts/outcomes, approval and interaction, skills, plans and context.

Model fragments identify an attempt. A failed attempt's visible text remains
in its attempt record; it is not a successful assistant message. Final tool
calls map provider IDs to distinct invocation IDs and assistant message indices.
Repeated provider IDs in different turns are valid. Repeated command IDs are
not new work. Errors and denied calls have terminal tool results in the next
provider request.

SDK event consumers read bounded batches of 128 from durable history and use
watch notifications only to wake up. Slow/paused/dropped subscribers are not
execution dependencies. The JSONL host additionally bounds its output queue
and emits `resync_required` when a subscription cannot keep up.

For werkCoder/werkStation: own the trusted host configuration, render prepared
approval details, store the last session cursor, reconnect with `open_session`,
then answer existing request IDs. A session ID is an identifier, not an
authentication credential. A future remote transport must add authorization
outside this local stdio protocol. Do not turn model text or process stdout
into host commands.
