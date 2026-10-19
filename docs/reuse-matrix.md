# Werk reuse decision — 2026-10-19

Wire reference: philipbodenbach/werk1112
`78254d7f709586576ec14d06a7f362a8185c2f36` (remote HEAD checked).
The neighboring local checkout has a different HEAD; it is not modified and
is not the authoritative revision for this integration.

| Function | Existing API | Public reuse | Decision and boundary |
|---|---|---|---|
| Control discovery | WerkProtocolClient; /werk/v1/info, /capabilities | Public synchronous HTTP-only client | Thin async TLS-capable wire adapter; avoid inference dependency graph; validate envelope/version/request ID |
| Tool chat | /v1/chat/completions | Public HTTP | Native documented chat/SSE contract; no invented /werk/v1/chat |
| Task/model schemas | /v1/capabilities, /models, /parameters | Public HTTP | Keep model, runtime capability and grant separate |
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
