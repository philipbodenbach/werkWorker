# Context limits and automatic compaction

Every model attempt, including children, retries and optional model summaries,
passes an engine preflight. The effective context is the minimum of the run's
`Limits.context_tokens` and the provider's known context size. Output reservation
is capped by the requested reserve and one quarter of the provider context.
The input estimate, reserved output and 512-token safety margin must fit before
any provider dispatch. A custom `ContextBuilder` cannot bypass this check.
Zero output budgets and arithmetic-overflow-sized limits are rejected.

The default context builder keeps a stable profile/host prefix, sorted tool
schemas, available skill metadata, optional output schema, committed summaries
and canonical history. Skill content enters only through `skills.load`; project
instructions enter through explicitly granted file reads.

Sizing conservatively counts serialized **wire-shaped UTF-8 bytes**, including
escaped tool arguments, tool schemas and structured output schemas, plus 256
units of template margin. It is an upper estimate for ordinary byte-based tokenizers,
not a native tokenizer measurement or a universal guarantee about custom templates.
Actual usage stays optional and separate from estimates. Werk's Anthropic count
endpoint is not silently substituted for OpenAI chat/tool history.

## Server evidence

WerkProvider resolves context from these sources, always honoring a smaller
explicit `capabilities.context_tokens` in the connection JSON:

- Matching deployment diagnostics: use the smallest per-request context across
  the configured ID/alias. vLLM uses `profile.context`; llama.cpp conservatively
  divides it by `profile.parallel`, so a shared KV allocation is never counted
  as every child's individual budget. Unknown runtime/invalid values supply no
  context evidence.
- Fresh, protocol-validated llama.cpp observability with **one slot** can supply
  `context_capacity_tokens`. An aggregate over several slots cannot establish
  the smallest slot size; no average is claimed as a safe minimum.
- An explicit host setting is accepted when the server cannot expose the limit.
  Without either evidence or that setting, inference returns
  `context_limit_unknown`; `diagnose` still works. Set this to the actual server
  per-request limit (including `--ctx-size`/deployment constraints), not merely
  the model's advertised maximum training context.

For example, if your server allows 8,192 tokens per request, set
`"context_tokens": 8192` in the `capabilities` object of
[werk-config.json](../examples/werk-config.json). A null example value is
intentional: startup discovery must establish it or the operator supplies it.
`diagnose` reports the resolved size and its discovery source. Inference slots
and context capacity are distinct: four workers do not quadruple a request's
context or the root's total request budget.

If Werk returns HTTP 400 with `context_length_exceeded` and a valid smaller
`context_length`, the provider atomically lowers its shared limit. The engine
rebuilds/compacts the request and output reservation before another attempt.
A rejection without a smaller usable limit fails without blindly retrying the
same payload. Learning can only lower capacity and is bounded by ordinary
step/request/wall limits. Reconnect to discover later capacity increases.
Server error bodies are not exposed in session history.

A `finish_reason: length` becomes nonretryable `model_output_limit`. Partial
text remains audit evidence; incomplete tool JSON never executes. Ordinary
429/503 retries remain bounded and honor Retry-After. A provider's private
rate/total-token quotas cannot be inferred from concurrency telemetry.

## Automatic rolling compaction

Compaction is enabled by default without an extra model request. Before **each**
ordinary inference, the default builder targets 80% of available input capacity.
It replaces complete earlier assistant/tool groups with bounded extractive
excerpts, including groups completed earlier in the same long-running task.
The latest assistant/tool group **within the current run** and every user/system
constraint are retained. After a run finishes, its final answer becomes eligible
for compaction on the next turn. A long code answer therefore does not permanently
block follow-up messages in the same session. This trims the next request, not
the saved conversation, and does not require a server restart or cache reset.
Pending invocations finish or resolve their approvals before this step; no
half tool-call/result group is compacted.

Excerpts include source message indices, roles, tool names and up to 100
characters of each source content; the accumulated excerpt is capped at 1,536
UTF-8 bytes. Candidates that would increase size are discarded. These are
explicitly labeled incomplete, untrusted excerpts, not a claim of lossless
semantic summarization. The model may need to reread evidence through granted
tools. If the fixed excerpt allowance would crowd out current work, older
excerpts are further removed to fit the available budget. Canonical messages,
outcomes and approvals are never deleted.

Each durable `ContextSnapshot` records its hash, estimated size, omitted source
indices and excerpt/summary. Compaction is recalculated from canonical history,
so reopening a session does not depend on hidden memory. The TUI shows context
estimates and compaction notices. If preserved constraints, schemas or the newest
group still cannot fit, `context_overflow` stops **before** inference; compaction
never silently discards the user's requirements to force a request through.

`Limits.model_compaction=true` additionally opts into one model-generated summary
of old assistant/tool groups at the start of a run. This is separate from default
rolling compaction. It uses the same server preflight, shared tree request budget,
output budget and cancellation path. A `CompactionRecord` retains source indices,
hash, attempt ID and summary. Empty, larger, tool-requesting or failed summaries
are not committed; source messages remain available for export.

Large tool results use bounded excerpts and, where configured and within quota,
owned artifacts. Failed artifact writes never claim a retrievable artifact.
`artifacts.read` checks scope and returns at most 64 KiB. No Worker KV cache,
weights, residency manager or hidden long-term memory is implemented. Normal
Werk chat can benefit from server caching; only observed usage is reported.

See [recorded validation](benchmarks/2026-10-19-subagents/README.md) for the
long-run compaction, oversized-constraint, context-reduction and truncated-tool
regressions. No live-model token-limit experiment is claimed.

## Interactive budgets and project rules

The connection wizard defaults to 4,096 output tokens including reasoning. It
checks the effective per-request context and requires an output reserve no larger
than one quarter of it before opening the session. Headless/direct CLI paths keep
`Limits::default()` (2,048 output tokens). Setup applies its selected limits to
each new TUI run; existing recovered runs retain their original persisted limits.
Project instructions are immutable profile constraints in the request, counted
by the existing estimator and retained during compaction. See [setup](setup.md)
and [project rules](project-configuration.md) for configuration and size bounds.
