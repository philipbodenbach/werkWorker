# Context limits and automatic compaction

This document describes shared admission/compaction behavior. Prompt guidance
and the per-turn workspace ledgers below describe preserved legacy `classic` runs;
the default [adaptive harness](harness.md) keeps dynamic evidence out of the system
prefix and restores omitted loaded skills as lower-authority data.

Every model attempt, including children, retries and optional model summaries,
passes an engine preflight. The effective context is the minimum of the run's
`Limits.context_tokens` and the provider's known context size. The initial
`reserve_output_tokens` is capped at one quarter of a known provider context.
`max_output_tokens` is a separate optional hard ceiling. With no host ceiling,
Worker uses the verified provider ceiling; an older/unknown provider keeps the
initial reserve as its ceiling. An explicit host ceiling never exceeds a known
provider ceiling. Neither setting changes the server's context allocation.

After a truncated generation, the next desired reservation doubles within that
ceiling. Recovery state is persisted and the larger budget remains available on
subsequent turns. Compaction first frees eligible history. If the larger reserve
cannot fit, the builder reduces it using the actual input count; its minimum is
based on the original reserve (up to one quarter can be lent to input), rather
than making a larger recovery target cause an avoidable context overflow.
The input estimate, output reservation and 512-token safety margin must fit before
any provider dispatch. User/system constraints and tool pairs remain intact.
A custom `ContextBuilder` cannot bypass admission. Zero output budgets and
arithmetic-overflow-sized limits are rejected.

The default context builder keeps a stable profile/host prefix, sorted tool
schemas, available skill metadata, optional output schema, committed summaries
and canonical history. Skill content enters only through `skills.load`. New CLI
sessions keep full root project instructions and a deterministic source index in
context; scoped snapshots are supplied before affected actions, which are then
reconsidered by the model. See [project context](project-configuration.md).
These complete rule messages and recovery feedback participate in the same full
request count, hash, native token counting and admission as all other messages.
They are not shortened to make a request fit: indispensable rules can cause
`context_overflow`. Legacy profiles with embedded rules remain unchanged.

Adaptive/minimal context observations follow the leading canonical user/system
turns, after the stable host/project prefix. Adding summaries, restored skills
or receipt ledgers therefore does not move images in the initial user turn.
Observations retain their assistant role; the remaining conversation keeps its
order. A later image can still move when earlier history is compacted, and then
the normal conservative image estimate applies. No image-key or margin is relaxed.

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

- `api.chat.limits` advertises `GET /werk/v1/chat/limits?model=MODEL`, which
  reports optional `context_tokens` and `max_output_tokens`. Worker validates
  schema version, model identity and numeric ranges. This read does not load a
  model or run inference. Aliases use conservative limits across replicas.
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
replayed by transport retries. The engine can rebuild with an increased output reservation or bounded
file steps, up to `max_retries` consecutive recoveries, inside the
same step/request/time budgets. Partial text remains audit evidence; none of
the incomplete response's tools execute. Every distinct generation-limit failure
receives fresh adaptive feedback, even after intervening complete read calls.
When output cannot grow or still fails, tools request smaller complete sections.
With verified context/output ceilings, file guidance uses the actual request
reservation and its selected/default reasoning profile; character targets remain
estimates. Reads use bounded line/UTF-8 byte pages, and `files.insert` can add a
section without copying an anchor. Providers without verified ceilings retain
the 2,048-character recovery bound for file content/replacement strings. Each
process argv string also retains that recovery bound.
The request schema and execution validator enforce actual schema bounds;
the registered tool identity and approval scope are unchanged. These temporary
schemas survive restart. A model must create a small working increment and then
use exact, hash-bound edits. This is conditional recovery, not a global skeleton
requirement. Reasoning remains the selected protocol tier/server default; Worker
does not silently lower effort to gain answer tokens. An identical
previously truncated request is rejected before dispatch with
`model_output_recovery_stalled`; durable fingerprints keep this guard on resume.
This does not judge whether a complete response substantively finishes the task.
Ordinary 429/503 retries remain bounded and honor Retry-After. A provider's private
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

For coding profiles with file-editing tools, each request ends with bounded
`coding_task_state` evidence reconstructed from durable plan/check/file receipts.
It includes recent paths and observed hashes, small exact read-tail excerpts,
and repeated calls approaching the guard. These are historical observations,
not current filesystem guarantees or system instructions. A directory listing
before a successful workspace mutation loses its setup-retention preference;
an old empty listing must not encourage rebuilding files that were later saved.

Excerpts include source message indices, roles, tool names and up to 400
characters of each source content; the accumulated excerpt is capped at 1,536
UTF-8 bytes. The builder persists its selection with a canonical-history prefix
hash. Following requests and resumed sessions reuse that selection when the
prefix still matches, append new work, and recheck the full request against current
provider/reasoning/output limits. User/system constraints and the newest working
group remain protected. Legacy snapshots without a checkpoint use bounded cold
planning; an edited or truncated source invalidates the checkpoint.

When compaction is necessary, local serialized sizes and the measured excess
choose batches of whole eligible groups. Each retention pass evaluates at most
four complete candidates, including its final eligible group. Optional excerpts
can add one summary-free alternative per candidate; output borrowing can add up
to three final admission checks. The normal fitting path needs only one full
provider estimate, instead of measuring the fixed prefix and history repeatedly.
Local sizing guides selection only; the cheapest fully counted request wins.
An initial candidate can temporarily grow when
retained receipts are added or an image moves to a position without a learned
reserve; this must not prevent later groups from being compacted together.
The chosen request must still pass the unchanged full admission calculation.

`ContextSnapshot.planning` records selection reuse, provider-estimate calls,
candidate count, initial input size and low compaction progress. These are planner
invocations, not necessarily HTTP calls: provider caches, image fallback and
transport retries can change the HTTP count. A reduction smaller than 5% of the
available input budget emits `compaction_low_progress`; the selected context is
still retained, preventing another full-history sweep. If all bounded candidates
fail to reduce input and the request cannot fit, `compaction_no_progress` stops
the attempt with an actionable error rather than declaring task completion or
automatically restarting the same maintenance loop. The full transcript remains
in storage. The TUI suppresses duplicate compaction notices on reused selections.

Extractive summaries are explicitly labeled incomplete, untrusted excerpts, not a claim of lossless
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
shell edits. Explicit overall step, request and time budgets still bound alternating
processes. Interactive runs use continuous aggregate budgets by default.

In classic mode the model sees the remaining step and tool-call budgets on every turn so it can
plan implementation and repeated verification within the configured limits.
SDK/headless finite defaults allow 48 steps and 60 model requests. TUI runs
remove aggregate cutoffs unless `--limits` is supplied; headless `--continuous`
and `Limits::continuous()` provide the same behavior. The persisted `continuous` flag disables aggregate cutoffs without reinterpreting
older numeric budgets. Saved runs retain their persisted limits.

File tools accept both relative paths and the sandbox alias `/workspace/...`.
Both resolve to the same workspace scope before policy checks, and prepared
actions use relative paths. Other absolute paths and traversal return a path
validation error, not a misleading missing-permission result. Scoped grants,
hard denies, symlink isolation and hash-bound edits still apply.

The connection wizard defaults to 4,096 output tokens including reasoning. It
checks the effective per-request context and requires an output reserve no larger
than one quarter of it before opening the session. Headless/direct CLI paths keep
`Limits::default()` (2,048 output tokens). Setup applies its selected context/output limits to
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

`Diagnostic` events distinguish `input_tokens_native_exact`,
`input_tokens_native_text_plus_image_estimate`, `input_tokens_wire_bytes_estimate`,
and `input_tokens_provider_unspecified`. Reporting the source makes no additional
count request. Native count/cache/admission logic remains shared across modes;
cache hits never subtract from context occupancy. Validated numeric Werk chat
`werk.timings` measurements also survive JSON/SSE in `ModelCompleted.metadata`;
usage (including cached tokens) remains separate and optional. Arbitrary backend
diagnostics are not copied into these metrics.

## Active time and approval waits

`max_wall_ms` measures active run time, including model/tool work and retries.
A pending tool approval pauses that clock. Its start time is durable; deciding,
expiring or cancelling the request credits the elapsed wait once. Resume after a
host restart preserves the pause and original approval identity. A decision
resumes the clock immediately, even before the actor notices it. Approval expiry
is independent (24 hours), and cancellation remains available during a pause.
`metrics.total_ms` still reports elapsed wall time; `approval_wait_ms` identifies
the excluded portion. Time spent on ordinary model/tool work remains bounded.

## Recovery design references

The separation between an initial allowance and a bounded retry ceiling follows
[Hermes' truncation handling](https://github.com/NousResearch/hermes-agent/blob/92663ae8054ae81f207f46d83512d3f879dc698d/agent/turn_truncation.py): increase the actual output cap and never execute incomplete tool arguments.
Werk additionally clamps growth by native input counts and configured context.
The fallback to incremental work follows
[Anthropic's harness guidance](https://www.anthropic.com/research/effective-harnesses-for-long-running-agents); compaction alone does not solve oversized implementation attempts.
These mechanisms do not establish application correctness. The isolated live
acceptance in `scripts/tests/live_tetris.py` can require a real truncation with
`--expect-output-recovery`; its completion and syntax checks still require a
separate browser gameplay check.

Current coding plans and the latest host-produced verification evidence are
projected from durable run state even when their original messages are compacted.
The prompt excerpts are bounded; full evidence remains available to the completion
gate. See [coding acceptance](harness.md#coding-acceptance-and-continuous-work).
