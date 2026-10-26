# SDK and events, schema 1

The authoritative contracts are `src/core.rs`, `src/api.rs` and Rustdoc. Public
operations return `Result<T, WorkerError>` with a stable machine-readable code.

| Operation | Result and meaning |
|---|---|
| `WorkerEngine::builder(Arc<dyn ModelProvider>)` | Explicit composition, no import-time effects |
| `.store(...).tools(...).skills(...).sandbox(...).artifacts(...).context(...).execution(...).harness(...).principal(...).build().await` | Open a host with chosen components |
| `engine.create_session(profile, workspace, policy, metadata).await` | Pinned profile/skills, workspace identity, host principal |
| `engine.open_session(id).await` | Recover durable state; no automatic external dispatch |
| `engine.list_sessions().await` | Stored session IDs |
| `engine.rename_session(id, title).await` | Persist a 1–80 character display title; no model or workspace startup |
| `engine.delete_session(id).await` | Delete root + descendants atomically; returns removed IDs; rejects active work and invalidates old handles |
| `session.submit(input, command_id, limits, output_schema).await` | Early `RunHandle`, busy if same session active |
| `session.follow_up(input, command_id, limits).await` | Persist a run in FIFO order; queue limit 16 |
| `session.run(run_id).await` | Address a persisted run |
| `run.events(after_cursor)` | `Stream<Item=Result<WorkerEvent>>` scoped to that run |
| `session.events(after_cursor)` | Session replay/live stream; cursor is session-wide |
| `run.wait().await` | Terminal result after outcomes, local job cleanup and final commit |
| `run.status().await` | Run snapshot, pending requests, attempts, outcomes and metrics |
| `run.steer(input, command_id).await` | Queue input before next model request; current batch finishes |
| `session.reasoning_options(&limits).await` | Query model tiers/default for the active run's effective output reserve, or supplied idle limits |
| `session.set_reasoning_tier(tier, &limits).await` | Validate and save effort for newly planned parent and child requests; `default` restores the server default |
| `run.cancel(reason, command_id).await` | Signal cancellation; use `wait` for cleanup completion |
| `run.respond_to_approval(request_id, decision, command_id).await` | Address one exact durable request |
| `run.respond_to_interaction(request_id, answer, command_id).await` | Supply one durable answer |
| `engine.resume_run(session_id, run_id).await` | Explicitly resume interrupted safe root work; children require new delegation |
| `session.reconcile_invocation(run_id, invocation_id, confirmed_result, command_id).await` | Host records external evidence, never replays the action |
| `session.revoke_tool(name, command_id).await` | Remove grants and hard-deny new starts |
| `session.enable_tool_approvals(command_id).await` | Explicit host opt-in to all registered tools/skills and per-action approvals in this root session; active children must finish first |
| `session.snapshot()/export().await` | Versioned canonical data, no provider/broker credentials |
| `session.fork(explicit_new_policy).await` | Independent identities/history at a completed boundary |
| `engine.execution_status()` / `session.execution_status()` | Capacity source, configured/effective limits and free slots |
| `run.delegate(Vec<agents::DelegateTask>, command_id).await` | Create owned children; same complete command/payload deduplicates |
| `run.subagents().await` | Direct child IDs, goals and optional full run snapshots |
| `run.subagent(child_session_id).await` | Owned child RunHandle; existing steer/cancel/approval/interaction APIs apply |
| `engine.close().await` | Cancel active work, await managed cleanup, flush and close store |

`ApprovalDecision` is `AllowOnce`, `Deny`, `AllowSameArguments` or `AllowForSession`.
`AllowSameArguments` requires `HostPolicy.allow_similar=true`. Its deterministic scope is
the same tool name/version and identical prepared JSON in the same run,
including file base/diff or process sandbox descriptor. It never covers another
run. `FullAccess` is a host-selected approval mode within existing grants.
`HostPolicy.request_approvals=true` instead asks for every non-internal action,
including actions without a static grant. It never bypasses hard denies or
read-only workspaces. `AllowSameArguments` does not skip this mode's confirmation.
The explicit `AllowForSession` decision stores a host-owned remembered grant:
workspace-wide use of the named file tool, or exact prepared arguments for other
effects, bound to the tool version. It is available only on root session requests.
`HostPolicy.tool_permissions` supports per-tool `deny`, `ask` and `allow` rules;
hard denies and workspace writability still take precedence. An explicit deny
also overrides remembered grants. Hosts should clear a tool's remembered grants
when resetting its rule to `ask`, as the CLI editor does.

`session.configure(profile, policy, command_id)` accepts idle sessions and root
sessions waiting for input/approval. Active descendants still block changes;
workspace writability cannot change during an already prepared approval.
SDK/headless defaults remain unchanged.

`ModelRequest.reasoning_tier` captures an optional session override before token
counting. `None` inherits the connection; `Some("default")` explicitly follows
the protocol default and suppresses an inherited native effort. Custom context
builders and Rust request literals must include the field (`None` to inherit).
Older serialized requests omit it and continue to deserialize. Provider count
cache keys and request fingerprints distinguish the captured selection.

Steering emits `Diagnostic` codes `steering_queued` and `steering_applied` for
acceptance and inclusion at the next model-request boundary, respectively.
These confirm delivery, not semantic compliance or completed implementation.

`ContextSnapshot.source` is an optional canonical-prefix selection checkpoint;
`planning` contains optional planner count/progress diagnostics. Custom builders
can use `None` for both fields. Older serialized snapshots remain readable.
Neither field is a cached permission, token admission or model-completion claim.

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

Library-only consumers can set `default-features = false` to omit the optional
ratatui/crossterm client dependencies. Default installs include both CLI commands.

For image input, construct `attachments::ImageAttachment::from_bytes(&png_or_jpeg)`
and use `session.submit_with_images`, `session.follow_up_with_images` or
`run.steer_with_images`. Existing text methods keep their signatures. Images
are durable in `Message.images`; old session JSON defaults to an empty array.
Providers receive standard OpenAI text/image content parts. Explicit
`Support::Unsupported` rejects an image before accepting the command; `Unknown`
lets the server validate model support. No model capability is inferred from
successfully importing a clipboard image.

Configure `parallel::ExecutionConfig` on the builder before `build`. Provider
implementations can report `providers::InferenceConcurrency` through the defaulted
`ModelProvider::concurrency()` method. Unknown capacity serializes inference.
The tree shares the root's total `Limits.max_model_requests`; each run retains
its own metrics. `Run.children`, `Run.tree_model_requests`, `SessionData.parent`
and `EventKind::Subagent` expose durable orchestration state. See
[subagent semantics and examples](subagents.md) before implementing a host UI.

New root runs are adaptive by default. `EngineBuilder::harness(harness::HarnessConfig)`
optionally configures completion criteria and explicit host guidance. Legacy mode
values are migrated for new roots; existing run contracts are preserved. Existing struct literals and provider implementations remain
compatible. Run-bound configuration, completion records and generated-message
provenance occupy the reserved `SessionData.metadata._werkworker_harness` object;
old sessions without it remain classic on resume. See [adaptive harness](harness.md)
for exact examples, limits, completion semantics and metadata requirements.
`ModelProvider::input_token_source(&request)` is an optional, non-I/O diagnostic
method; its default reports native cached evidence or an unspecified provider
estimate. It does not replace `count_input_tokens` / `estimate_input_tokens`.
