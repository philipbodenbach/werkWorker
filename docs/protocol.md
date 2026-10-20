# Worker JSONL protocol 1

Start `werk-worker --script responses.json rpc` for an explicit offline fixture,
or select `--werk-config` for Werk. Host startup fixes workspace, credentials
and grants. RPC parameters cannot pick arbitrary host paths or elevate policy.
This is an independent Worker control protocol, not `/werk/v1`.

Each UTF-8 JSON object ends with LF; CRLF is accepted. Maximum input is 1 MiB
per record. Invalid records return an error and do not terminate the host.
Stdout contains JSON only; diagnostics are on stderr. Queue capacity is 128,
subscription limit 16, command timeout 30 seconds, stalled output timeout five
seconds. EOF performs shutdown.

```json
{"id":1,"method":"initialize","params":{"protocol_version":1}}
{"id":2,"method":"session.create","params":{"profile":"general-readonly"}}
{"id":3,"method":"events.subscribe","params":{"session_id":"SESSION","after_cursor":0}}
{"id":4,"method":"run.submit","params":{"session_id":"SESSION","input":"Analyze notes","command_id":"job-1"}}
```

Responses repeat `id` and contain exactly `result` or `error`. Errors carry
`code`, redacted `message`, `retryable`, and optional `retry_after_ms`. Acceptance
does not mean completion. Events are notifications:

```json
{"method":"event","params":{"schema_version":1,"event_id":"evt_...","session_id":"session_...","run_id":"run_...","cursor":12,"timestamp_ms":0,"event":{"type":"run_state","data":{"status":"completed"}}}}
```

| Method | Parameters besides request `id` |
|---|---|
| initialize | protocol_version=1 |
| session.create | optional profile and metadata |
| session.open, session.export | session_id |
| session.list | none |
| session.fork | session_id; uses explicit startup host policy for new scope |
| run.submit | session_id, input, command_id; optional limits/output_schema/images |
| run.status | session_id, run_id |
| run.cancel | session_id, run_id, command_id; optional reason |
| run.steer | session_id, run_id, input, command_id; optional images |
| run.resume | session_id, run_id |
| session.followup | session_id, input, command_id; optional limits/images |
| approval.respond | session_id, run_id, request_id, decision, command_id |
| interaction.respond | session_id, run_id, request_id, answer, command_id |
| events.subscribe | session_id, optional after_cursor (exclusive) |
| execution.status | none; configured/effective limits, discovery source and free slots |
| agents.delegate | parent session_id, run_id, tasks, command_id |
| agents.list | parent session_id, run_id |
| agents.cancel | parent session_id, run_id, child_session_id, command_id |
| agents.message | parent session_id, run_id, child_session_id, input, command_id |
| shutdown | none |

Approval decisions serialize as `allow_once`, `deny`, `allow_same_arguments`.

`images` is an optional array of up to four attachments with `mime_type`
(`image/jpeg`), `data_base64`, `width` and `height`. Use the SDK's
`ImageAttachment::from_bytes` to normalize PNG/JPEG data to at most 1024 pixels
per side and 128 KiB per image before encoding. Image-only submissions use
`input: ""`. Attachments persist with their message; they are sent as standard
OpenAI `image_url` content parts and require a vision-capable model/backend.
Omitting `images` preserves existing text requests and their idempotency keys.

Inspect `run.status` for pending request IDs after reconnect. Reuse command IDs
with the same payload if delivery is uncertain; changed payloads conflict.
The dedupe scope is the session, including method and run. Native Werk HTTP
request IDs, Worker command IDs and JSONL response correlation IDs are separate.

`resync_required` terminates the affected subscription. Resubscribe from the
last *received durable cursor*, not the notification's arrival count. A new
subscription can replay terminal events. The SDK methods for host reconciliation
and revocation are intentionally not exposed to model-generated data.

See `src/transport.rs::METHODS` and the CLI/RPC integration tests for executable
contract consistency. `examples/jsonl_client.py` is a complete local client.

`agents.delegate.tasks` uses `{goal, context?, tools?, read_only?}` entries from
[the delegation contract](subagents.md). List/delegate return direct children
with session IDs, goals and optional Run snapshots. Model tools use
`session_id` for a target child; host RPC uses `child_session_id` to distinguish
it from the parent session. Only directly owned children are accepted.

To approve a child, the trusted host opens its session, subscribes to its events
and sends `approval.respond` with that child's session/run/request IDs. The same
rule applies to `interaction.respond`; no parent approval substitutes for it.
`run.cancel` on the parent cancels descendants. The new methods are additive to
protocol 1. Discovery uses WerkProtocol on the server; Worker JSONL remains its
own local protocol.
