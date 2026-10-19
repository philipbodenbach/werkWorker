# Context, compression and cache semantics

`ContextBuilder` is replaceable. The default has a stable profile/host-instruction
prefix, sorted tool schemas, available skill metadata, an optional output schema,
committed summaries, and canonical history. Skill content enters only through
`skills.load`; discovery itself executes nothing. Active sessions retain skill
source/hash snapshots. Project instructions such as AGENTS.md are read through
the same granted `files.read` path, not ambient filesystem discovery.

Input sizing conservatively counts serialized UTF-8 bytes as an upper token
estimate plus overhead. It includes tool schemas, reserves output and 512 units
of safety margin. Actual provider usage remains optional/null and separate from
estimates. Werk's `/v1/messages/count_tokens` uses another message contract, so
this version does not claim an exact tokenizer count for OpenAI tool histories.

On pressure, deterministic pruning removes complete *old* assistant/tool groups.
All user messages/constraints and current-run groups stay. Raw history never
changes. A ContextSnapshot records the hash, selected size and omitted indices.
If this cannot fit, context_overflow fails before inference.

`Limits.model_compaction=true` explicitly opts into at most one old-history model
summary at the start of a run. Its step/request/output budgets and cancellation
path are the same as ordinary attempts. A CompactionRecord retains source indices,
hash, attempt ID and summary. User constraints are kept separately and never
replaced by the summary. Empty, larger, tool-requesting or failed summaries are
not committed. Canonical source messages remain available for export and audit.

Large tool results become bounded excerpts with a truncation flag and, when
configured and within quota, an owned artifact reference. A failed artifact
write does not falsely claim a retrievable artifact. `artifacts.read` checks scope
and returns at most 64 KiB.

No KV cache, weights, residency manager or hidden long-term personal memory is
implemented by Worker. Normal Werk chat history can benefit from backend reuse;
reported usage/cache counts reflect only fields actually returned. Runtime
handles are not Worker session identities. Prefill/decode's reduced text DTOs
are deliberately not used to transport complete tool-call history.
