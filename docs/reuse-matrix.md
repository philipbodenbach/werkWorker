# Werk reuse decision — 2026-10-19

Wire reference: philipbodenbach/werk1112
`78254d7f709586576ec14d06a7f362a8185c2f36` (remote HEAD checked).
The neighboring local checkout has a different HEAD; it is not modified and
is not the authoritative revision for this integration.

| Function | Existing API | Public reuse | Decision and boundary |
|---|---|---|---|
| Control discovery | WerkProtocolClient; /werk/v1/info, /capabilities | Public synchronous HTTP-only client | Thin async TLS-capable wire adapter; avoid inference dependency graph; validate envelope/version/request ID |
| Tool chat | /v1/chat/completions | Public HTTP | Native documented chat/SSE contract; no invented /werk/v1/chat |
| Task/model schemas | /v1/models for chat startup; /v1/capabilities for explicit media-tool discovery | Public HTTP | Keep model, runtime capability and grant separate; avoid global backend probes on connect |
| Native task execution | /v1/tools, /v1/tools/call | Public HTTP | Explicit allowlist; preserve server function schema and job receipt |
| Job lifecycle | /v1/jobs/{id}, /v1/outputs/{id} | Public HTTP | Preserve IDs and principal, bounded authenticated reads/cancellation |
| Token count | /v1/messages/count_tokens | Anthropic message contract | Not semantically identical to full OpenAI tool history; conservative labeled local estimate, no fake exact count |
| Prefill/decode | /werk/v1/prefill, /decode | Text-oriented control DTOs | Do not project tool histories through this lossy path |
| Residency/cache | Runtime-control capabilities | Backend owned | Report evidence only; no worker KV cache |
| OS isolation | Runtime handoff/security modules | Not an agent sandbox | Dedicated Linux bubblewrap backend |

The lightweight client could later be extracted jointly with Werk. No private
module imports or changes to werk1112 are needed. Conformance is tested using
local HTTP fixtures; a real model connection is a separate opt-in check.

## Parallelism and context follow-up

For this additive work the inspected local Werk reference was
`9b195c9b1f60838a538fdde8002e87500e565610`. This is a separate, explicitly
identified source from the original wire reference above; it was not modified.
Relevant files in that revision:

- `src/api/deployments.rs`, `src/deployments.rs`: ordinary-JSON deployment
  diagnostics, ID/alias routing, configured `parallel` and `context`, runtime.
- `src/api/observability.rs`, `src/observability.rs`,
  `src/backend/llama_server/telemetry.rs`: WerkProtocol envelope, timestamps,
  available backend/model identity, native slots and aggregate context gauges.
- `src/api/generation.rs`, `src/openai.rs`: `context_length_exceeded`, flattened
  numeric `context_length` details and output-token reservation.
- `src/api/chat.rs`: `/v1/models` does not advertise a per-request context limit.

These contracts are covered by HTTP fixtures. No dependency on Werk's private
Rust types was added. Multi-slot aggregate context telemetry is deliberately not
used as a per-request minimum. See [context limits](context.md) and
[parallel execution](subagents.md) for fallback behavior and source limits.

## Current harness audit — 2026-10-21

The historical token-count row above describes the initial integration only.
The current public contract was inspected read-only in werk1112 revision
`193f8cd3e3316f8773c6c15b9427dbcf8625640e` (clean local checkout). No server files
were changed and no live inference was performed. This supersedes the older
claim that only Anthropic-shaped counting is available.

Verified sources in that revision: `src/api/werk/mod.rs`,
`src/werk_protocol/{client,types}.rs`, `src/api/{chat,generation,observability,deployments}.rs`,
`src/runtime_control/local.rs`, `src/backend/llama_server/telemetry.rs`, and
`src/backend/mod.rs`. The adapter uses HTTP contracts, never private Rust imports.

| Capability | Verified contract | Worker use / patch / fallback |
|---|---|---|
| Discovery | GET `/werk/v1/info`, `/werk/v1/capabilities`; version-1.0 envelope, request ID and optional matching headers | Already used at connect; no per-step/global backend probes |
| Chat and tools | POST `/v1/chat/completions`, OpenAI-shaped complete history, JSON or SSE; GET `/v1/models` | Already used; adaptive runs keep deterministic stable instructions/tool order, with no promise of backend cache reuse |
| Native count | POST `/werk/v1/chat/count_tokens`, ChatCompletionRequest body, `ChatTokenCountResponse { model, input_tokens }`; capability `api.chat.token_count` / `count_tokens` | Already used/cached; patch exposes count provenance without extra count calls; exact, hybrid image estimate and wire-byte fallback remain separate |
| Admission plan | POST `/werk/v1/chat/admission`; capability `api.chat.admission` / `admission` | Preferred for context planning. Uses the same native count or explicitly labeled fallback estimate as server generation, including image requests. Exact counts and `server_admission_estimate` remain distinct; older servers retain the previous counting path. |
| Count errors | Werk envelope with structured code/retryability; backend can return `unsupported` | Already validated; only verified unsupported falls back. Auth, identity, protocol and exhausted transient failures remain errors |
| Context / concurrency | GET `/werk/v1/observability` in protocol envelope; fresh matching backend/model slot evidence. GET `/werk/v1/deployments` is optional ordinary JSON | Already used once per connection; host caps remain upper bounds; reconnect refreshes deployment/capacity evidence. Stale/mismatched/aggregate-only telemetry cannot establish per-request context |
| Context overflow / output limit | Chat HTTP 400 `context_length_exceeded` with numeric `context_length`; completion `finish_reason: length` | Existing bounded rebuild/discard recovery preserved; patch makes output feedback transient lower-authority data in adaptive and preserved minimal runs |
| Usage / timings | `usage.prompt_tokens`, `completion_tokens`, `prompt_tokens_details.cached_tokens`; `werk.timings` in JSON and final SSE chunk | Usage/cache already used. Patch retains documented nonnegative numeric timing fields in existing completion metadata; arbitrary backend diagnostics excluded; absent values stay absent |
| Prefix/KV/residency | Capabilities depend on backend; automatic/external state differs from executable state operations | Deliberately report evidence only. No preload, unload, state prune, cache reset, slot borrowing or worker KV cache |
| Prefill / decode | POST `/werk/v1/prefill`, `/werk/v1/decode`; text/token/state DTOs | Verified not a lossless replacement for full chat/tool/multimodal history; deferred |
| Native tools/jobs | Explicit GET `/v1/tools`, POST `/v1/tools/call`, owned `/v1/jobs/{id}` and `/v1/outputs/{id}` adapters | Existing adapters retained, allowlists independent of discovery; no additional grants or generic polling framework |
| Tool discovery | Bundled profile wire schemas measured at 4,611–7,152 bytes, 13–19 tools | Existing registry and sorted, permission-filtered baseline retained. No additional MCP/native catalogue is enabled in these fixtures; no second registry or activation framework warranted by measured baseline |

Unknown capacity still means one inference slot; unknown safe context still
requires explicit host configuration. Observed external load is not reserved
capacity. Count/discovery timeouts retain the existing bounded semantics; active
run context/admission work is interruptible by cancellation and wall deadlines.
Both modes work with providers that have no WerkProtocol support.

Verified protocol limits and capabilities guide resource decisions only; they do not
prove model reasoning or planning competence and do not activate adaptive aids.
