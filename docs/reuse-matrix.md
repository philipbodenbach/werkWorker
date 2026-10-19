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
