# Context limits and automatic compaction

Every model attempt, including children, retries and optional model summaries,
passes an engine preflight. The effective context is the minimum of the run's
`Limits.context_tokens` and the provider's known context size. Output reservation
is capped by the requested reserve and one quarter of the provider context.
After compaction, a nearly fitting request may borrow up to one quarter of that
output reserve for input, keeping at least 2,048 output tokens (or the smaller
configured reserve). The builder updates its output guidance, recounts the
actual request and keeps the full safety margin. This adjustment applies only
to that request; later steps can use the full configured output reserve again.
The input estimate, reserved output and 512-token safety margin must fit before
any provider dispatch. A custom `ContextBuilder` cannot bypass this check.
Zero output budgets and arithmetic-overflow-sized limits are rejected.

The default context builder keeps a stable profile/host prefix, sorted tool
schemas, available skill metadata, optional output schema, committed summaries
and canonical history. Skill content enters only through `skills.load`; project
instructions enter through explicitly granted file reads.

When werkProtocol advertises `api.chat.token_count`, sizing uses the exact native
chat count from `/werk/v1/chat/count_tokens`, including tool schemas/history and
template options. Identical requests reuse a bounded cache; eviction triggers
recounting before admission. Protocol errors and model identity are validated.
Unsupported counters fall back to conservatively counting **wire-shaped UTF-8 bytes**, including
escaped tool arguments, tool schemas and structured output schemas, plus 256
units of template margin. It is an upper estimate for ordinary byte-based tokenizers,
not a native tokenizer measurement or a universal guarantee about custom templates.
Actual usage stays optional and separate from estimates. Werk's Anthropic count
endpoint is not silently substituted for OpenAI chat/tool history.

## Server evidence

WerkProvider resolves context from these sources, always honoring a smaller
explicit `capabilities.context_tokens` in the connection JSON:

- Fresh, protocol-validated llama.cpp observability supplies
  `context_per_request_tokens`, the minimum native slot size. Older servers with
  **one slot** may supply `context_capacity_tokens`. A multi-slot aggregate
  cannot establish the minimum; it is never divided to guess a limit.
- Matching deployment diagnostics are a fallback: use the smallest
  `profile.context` across the configured ID/alias. Both vLLM and llama.cpp
  deployment profiles express per-sequence context; the llama launcher multiplies
  it by parallel slots when allocating the native aggregate. Unknown runtime or
  invalid values supply no context evidence.
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

A `finish_reason: length` becomes `model_output_limit`. It is never blindly
replayed by transport retries. The engine can rebuild with feedback to use
smaller complete calls, up to `max_retries` consecutive recoveries, inside the
same step/request/time budgets. Partial text remains audit evidence; none of
the incomplete response's tools execute. Ordinary
429/503 retries remain bounded and honor Retry-After. A provider's private
rate/total-token quotas cannot be inferred from concurrency telemetry.

## Automatic rolling compaction

Compaction is enabled by default without an extra model request. Before **each**
ordinary inference, the default builder checks whether input capacity is exceeded.
Only then does it compact toward 80% of the space left after the fixed system/user
prefix, images and schemas. A large image reserve must not make the compaction
target smaller than the non-compactable prefix and erase evidence on every turn.
It replaces complete earlier assistant/tool groups with bounded extractive
excerpts, including groups completed earlier in the same long-running task.
The latest assistant/tool group **within the current run** and every user/system
constraint are retained. Under pressure, a latest group consisting solely of
successful file writes/edits may also be compacted if
complete, bounded path/hash receipts are available. Its implementation already
lives in the workspace and canonical history; the receipt summary instructs the
model to reread code when needed. Any accompanying assistant text/images remain
verbatim in the request. Failed writes and the latest reads/checks
retain their ordinary protection. After a run finishes, its final answer becomes eligible
for compaction on the next turn. A long code answer therefore does not permanently
block follow-up messages in the same session. This trims the next request, not
the saved conversation, and does not require a server restart or cache reset.
Pending invocations finish or resolve their approvals before this step; no
half tool-call/result group is compacted.

Excerpts include source message indices, roles, tool names and up to 400
characters of each source content; the accumulated excerpt is capped at 1,536
UTF-8 bytes. Candidates that would increase size are discarded. These are
explicitly labeled incomplete, untrusted excerpts, not a claim of lossless
semantic summarization. The model may need to reread evidence through granted
tools. If the fixed excerpt allowance would crowd out current work, older
excerpts are further removed to fit the available budget. Canonical messages,
outcomes and approvals are never deleted.

Small setup/skill results, file reads (including their hashes and errors) and task
plans receive bounded retention. Before discarding this evidence the builder can
use the bounded output-reserve adjustment described above. A separate receipt summary
keeps the latest successful changes for up to eight paths within 2 KiB, so losing
earlier code from the prompt does not make the agent restart file creation.
It records observed hashes and sizes, not a claim that external files never change.
These receipts include earlier runs in the session, so a follow-up can continue
after interruption. A separate 2 KiB ledger retains up to eight distinct recent
read observations with path, hash, line range, next-page cursor and error code.
The latest bounded task plan also survives a run boundary, explicitly subordinate
to the current user request. These ledgers inspect at most 128 recent invocations;
they preserve evidence, not current filesystem guarantees or new permissions.
Read excerpts put hash/range/cursor metadata before code text so truncating an
excerpt cannot preferentially hide the edit base or pagination instructions.

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
owned artifacts. File reads return compact text with line-range metadata; long
reads retain complete lines, their hash and `next_start_line`. A single oversized
line falls back to an excerpt/artifact. Failed artifact writes never claim a retrievable artifact.
`artifacts.read` checks scope and returns at most 64 KiB. No Worker KV cache,
weights, residency manager or hidden long-term memory is implemented. Normal
Werk chat can benefit from server caching; only observed usage is reported.

See [recorded validation](benchmarks/2026-10-19-subagents/README.md) for the
long-run compaction, oversized-constraint, context-reduction and truncated-tool
regressions. The [live coding runner](local-werk-example.md) additionally exercises
the configured local model without scripted replies.

## Interactive budgets and project rules

Identical tool calls are limited within a workspace revision, not across the
whole task. A different successful write or process call resets that comparison,
so an edit/test/fix/test cycle can reuse the same test command. Failed writes and
read-only setup calls do not reset it; identical processes cannot reset themselves.
Processes are conservatively treated as potential workspace changes, including
shell edits. The overall step, request and time budgets still bound alternating
processes.

The connection wizard defaults to 4,096 output tokens including reasoning. It
checks the effective per-request context and requires an output reserve no larger
than one quarter of it before opening the session. Headless/direct CLI paths keep
`Limits::default()` (2,048 output tokens). Setup applies its selected limits to
each new TUI run; existing recovered runs retain their original persisted limits.
Project instructions are immutable profile constraints in the request, counted
by the existing estimator and retained during compaction. See [setup](setup.md)
and [project rules](project-configuration.md) for configuration and size bounds.

For image attachments, a native multimodal count is used when available. An
unsupported image count is remembered separately for this connection and does
not disable the native counter for text requests. In that case, the worker
counts the unchanged text and tool schemas natively with image parts removed,
then adds 8,192 input tokens per normalized image and 256 tokens for image
framing. Only the counting copy omits images: the actual model request and saved
history retain every attachment. If text counting is unavailable too, the
serialized text/schema byte estimate remains the fallback. Context planning,
agent admission and transport admission use the same calculation; repeated
planning passes reuse the text count instead of repeating unsupported image
requests. The combined value remains an estimate, separate from the exact
native-count cache. Encoded base64 is not counted as text. This is a conservative
image estimate, not a model-specific
measurement. Server context admission remains authoritative. The regular body
size, context and run limits still apply to messages containing images.

After a complete validated response, WerkProvider can refine this reserve from
the server's total input usage minus the native text count for that exact request.
The native baseline comes from werkProtocol; cached-token usage is never subtracted.
The observed image overhead receives at least 256 tokens or 25% extra margin,
in addition to the 256-token framing reserve. The largest observation is retained
for the identical ordered images, roles and message positions on this connection.
Different images/placement or reconnect start with the conservative fallback.
Missing, invalid or partial-response usage and an evicted native text baseline do
not establish a measurement. The bounded cache holds only fingerprints and counts.
These derived values remain estimates: native multimodal counts take precedence,
and no estimated value enters the exact token-count cache. Providers that do not
implement the optional observation hook retain the original fallback behavior.
