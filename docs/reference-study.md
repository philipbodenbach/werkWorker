# Reference study — 2026-10-19

References inspected before implementation, using GitHub's public content API
at the following exact remote HEAD revisions. No source code copied.

| Upstream | Revision | Examined entry points | Decision |
|---|---|---|---|
| [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) | 687f282e8625c6b8d11ff58211a90524be2e4317 | docs/architecture.md; docs/subsystems/tools.md; packages/core/agent-loop/README.md | Separate agent lifecycle, guarded tools, canonical session projection. Do not adopt Cordis, hot reload or plugin boot trees. |
| [Hermes](https://github.com/NousResearch/hermes-agent) | 6e9b3e3b8852f12bf965f888213470574b72803e | agent/conversation_loop.py; tools/registry.py | General purpose toolsets, bounded errors, context maintenance. Avoid import-time registry mutation and central agent/UI coupling. |
| [Pi](https://github.com/earendil-works/pi) | d2e7d8b7c1d31d3661f769d5caef7ba8d50c3e03 | packages/agent/src/agent-loop.ts; packages/coding-agent/docs/rpc.md | Small provider/message seam, queued steering, correlated RPC. Own policy layer and durable pending requests. badlogic/pi-mono redirect verified. |
| [OpenCode](https://github.com/anomalyco/opencode) | 815de9950b205cd734c9962c4629b07d3b83ee07 | packages/core/src/permission.ts; tree inventory; [server docs](https://opencode.ai/docs/server/) | Headless client separation, addressed permissions. Current code is split into core/server/protocol; historical packages/opencode paths are unsuitable. No UI or HTTP server dependency. |
| [werk1112](https://github.com/philipbodenbach/werk1112) | 78254d7f709586576ec14d06a7f362a8185c2f36 | src/werk_protocol/client.rs; docs/api.md; docs/reference/werk-protocol-v1.md; docs/tool-calling.md | Follow actual control and inference contracts, preserve principal scope. Elastic-2.0, not MIT. See reuse matrix. |

[Agent Skills specification](https://agentskills.io/specification) was read on
the same date: YAML frontmatter, bounded name/description, directory matching,
optional resources and progressive disclosure. Skill loading never executes
scripts or elevates grants. Worker metadata uses the `werk-worker` namespace.

These are architectural references, not measurements of model quality or claims
about proprietary agent internals. Referenced project licenses do not replace
werkWorker's Elastic-2.0 license.
