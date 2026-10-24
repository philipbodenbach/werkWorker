# Recovery continuation and image-context compaction

Local correction against `c6f237c942297f3637813808c6ef8894e777cba7`.

Final status: the recovery and context regressions pass, but the original live
Tetris task still does not complete within its unchanged 900-second budget.
The final replay fails explicitly; its generated game is not playable.

## Observed incident

The reported Tetris session read the existing project, planned the implementation,
then exceeded its 4096-token output limit. Its next final response was byte-for-byte
the engine's `generation_output_limit` JSON. No write had executed in that run.
The recovery observation was the final assistant message of the next request.

The already running werk1112 instance confirmed the same sequence. Its preserved
observability records are in [werk-observability.json](werk-observability.json):

| Inference | Start UTC, 2026-10-21 | Seconds | State | Prompt | Cached | Output |
|---|---|---:|---|---:|---:|---:|
| 9 | 19:04:41.972 | 118.074 | length | 5549 | 4859 | 4096 |
| 10 | 19:06:43.219 | 6.410 | done | 5663 | 5338 | 1 |

The first eight requests ended with tool calls. The backend reported ten completed
requests, zero errors and zero cancellations. There is no evidence of a backend
crash or resource abort. The running process wrote only to its terminal; the
repository's on-disk werk log was dated October 8 and did not cover this incident.
The saved records came from the same read-only observability source as `werk top`,
without another inference, backend restart or cache operation. Numeric inference
IDs are available; HTTP request IDs are not present in this retained schema.

werk1112 passes chat message roles through to llama.cpp. The native backend
[defaults to prefilling a trailing assistant message](https://github.com/ggml-org/llama.cpp/blob/master/common/common.h).
The exact returned prefix and one new token are consistent with immediate EOS
after that prefill. A scripted provider that blindly returns prescribed turns
did not expose this role-dependent behavior.

## Correction and limits

Recovery observations stay in the assistant role. If assistant content ends the
default request, a fixed user-role transport boundary requests the next response
under the existing instructions and permissions. This also covers retained model
prose after compaction removes a completed tool group. It contains no dynamic tool, document,
skill or child data and grants no approval. The engine derives ownership from
construction or persisted indices, not a JSON `source` string. The boundary enters
the complete request before native counting, admission and hashing, including
compaction rebuilds. It is not persisted as actual user steering.

Whitespace-only answers and exact echoes of owned feedback cannot become accepted
final responses. They share the existing retry allowance with output-limited
attempts, derived from durable history. Partial tool calls still never execute.
Request, step, tree, wall-time and approval limits remain in force. Model summaries
also get a fresh turn and cannot retain their own echoed evidence or boundary.
Existing completed sessions are not rewritten; this corrects future requests.

The change does not classify arbitrary prose as successful implementation and
cannot guarantee that every model will complete every task within its limits.

## Validation

Regression fixtures now model assistant-prefill behavior and cover recovery into
real file writes, completion correction, compaction and resume. Additional cases
cover repeated empty/echo responses, saved Minimal-contract notices, successful-write
compaction, shared retry exhaustion after restart, request
limits, preserved earlier writes, discarded partial calls and legitimate user
JSON that merely resembles a harness envelope.

`cargo test --config /tmp/werkworker-followup-checks.toml --locked --offline --all-targets --no-fail-fast -- --nocapture`
passed **193 tests in 27 suites**, with no failures, ignored tests or skipped
sandbox cases. The existing config requires the real Linux namespace sandbox.

`cargo fmt --all -- --check` and
`cargo clippy --locked --offline --all-targets -- -D warnings` also passed.

The unchanged recovery fixture still takes five model requests and three tool
invocations. Its complete serialized requests total 38,805 bytes, compared with
38,053 before this change. Two continuation boundaries add 498 bytes and the
more concrete generation guidance adds 254 bytes, totaling 752 additional bytes.
The deterministic fixture estimator totals 39,636 versus 38,884;
these estimates are not native tokenizer counts or speed measurements.
[request-measurement.json](request-measurement.json) includes every request,
tool-schema size and the unchanged baseline totals.

## First live replay and second defect

The original prompt, attached image, existing project files and run limits were
replayed against the running local model in a separate temporary workspace and
database. The original workspace and session remained unchanged. The
[first live record](live-replay.json) contains the corresponding werk1112
inferences and sanitized receipts.

This run recovered from a 4096-token generation limit into successful
`files.write` and `files.edit` calls. It did not falsely complete. It then failed
after 538 seconds with `context_overflow`: 14,063 estimated input tokens versus
11,776 available. Its generated file had valid JavaScript syntax but still lacked
the game loop and keyboard handlers; Chromium confirmed it was not yet playable.

This exposed a separate planner defect. Rolling compaction inserts evidence
before the image-bearing user message. The changed image position correctly
invalidates the connection's learned image estimate and restores its conservative
8,192-token reserve. The planner rejected every individually larger candidate,
so it never combined enough omissions to remove old failed writes and saved
implementation bodies. The fix evaluates those candidates cumulatively and
retains the smallest complete request. Image identity/placement checks, counting,
reserve margins and all run limits remain unchanged.

The initial image-coding regression reported validated input usage through the real
WerkProvider adapter, reproducing the learned-position transition. It failed
before the planner correction with 13,851 estimated input tokens versus 11,776
available. Afterward it completed all writes, edits and a final verification
read at the same limits. Assertions check that multiple groups and a proven
latest write are omitted while user text, images, current explanation and the
latest verification receipt remain available. Candidate selection also considers
every intermediate optional-summary trim because those message positions can
change the image estimate again.

After the cumulative change, before the placement refinement below, the exact
failed live snapshot was passed to the corrected builder with a
fresh WerkProvider connection, its original tool definitions and unchanged
limits. This performed only ordinary token-count requests, without inference or
tool execution. Native text/tool counting plus the original conservative image
reserve produced **11,652 input + 4,096 output + 512 safety = 16,260**, within the
16,384 limit. It retained the image and compacted 16 source message indices.
The [context replay record](context-replay.json) labels this correctly as
`native_text_plus_image_estimate`, rather than an exact multimodal count.

## Second live replay and repeated-generation recovery

The [second live record](live-replay-second.json) used byte-identical fixtures,
configuration and limits. It wrote the HTML, then exhausted the existing allowance
after three 4096-token generations. It failed explicitly after 500 seconds; the
JavaScript remained a nonfunctional stub and the browser check failed. No complete
task is claimed for either of these two runs.

The second failure also exposed a delivery gap: the already active aid suppressed
the newly discarded attempt. Its first notice had been consumed, so the last
retry received the earlier context without the new failure evidence. A distinct
failed attempt now updates the same active episode's trigger, scope and delivery
marker. It recommends a smaller useful part of the work, without requiring
skeleton files or insertion markers. Episode counts, cooldowns and the shared
retry allowance remain unchanged. Identical evidence is not redelivered on each
turn. Resume also recovers a newer suppressed attempt in saved active states,
without reopening inactive episodes. Tests cover successful continuation, retry
exhaustion and both current and older saved states.

## Third live replay and stable initial image placement

The [third live record](live-replay-third.json) contains 16 model requests and
successful HTML and initial JavaScript writes. A later complete `files.write`
proposal omitted the required hash for that existing file, so the tool correctly
rejected it with `write_conflict`. Its intact proposal had to remain available
for correction. With the image position changed by context observations, the
next request needed 13,509 estimated input tokens and failed before dispatch.
The generated part still had no active game loop; the browser check failed.

The final builder places adaptive observations after the leading canonical
user/system turns, keeping an initial image at its original position. Host and
project instructions remain in their stable prefix; observations keep their
assistant role and the remaining history keeps its order. This avoids invalidating
the learned image reserve solely by adding context notes, without weakening the
provider's placement key, fallback, margins or admission. Failed latest proposals
remain intact. Tests cover a large rejected write followed by a real verification
read, and a separate earlier-task/later-image case that still requires aggregate
compaction and the conservative fallback.

## Fourth live replay and planning cost

The [fourth live record](live-replay-fourth.json) recovered from an output limit,
a transport timeout and a rejected new-file write. It successfully created the
start page and 6,376 bytes of JavaScript, and continued through rolling compaction
without another context overflow. It ultimately reached the unchanged 900-second
wall budget during a further generation. The browser found keyboard handlers but
no rendering loop, so this run was still incomplete and is not counted as success.

Late context planning took roughly 32–49 seconds between a tool result and the
next model request. The planner previously counted every small intermediate
omission separately. It now evaluates a candidate after four eligible groups or
at least 8 KiB of source messages have been removed, always including the final candidate of each
retention pass. Protected groups and constraints retain the same eligibility rules;
the selected complete request still uses the original native/fallback count and
admission checks. Batching may omit more optional old history per candidate.

The same two image fixtures completed with unchanged model/tool action counts:

| Fixture | Token-count calls before | After | Model requests |
|---|---:|---:|---:|
| Initial image, large rejected write retained | 76 | 49 | 13 |
| Earlier task before a later image | 71 | 55 | 12 |

These are [fixture HTTP-call measurements](image-counting.json), not claims about
live inference speed.

## Final live replay and remaining limitation

The [final live record](live-replay-final.json) contains the final code's run and
all 26 corresponding werk1112 inference records. It uses the same original task,
image, starting files, model and limits in a separate temporary workspace. It
ends with `wall_budget` after 900.016 seconds, with 26 model requests, 25 steps
and 24 tool invocations. The original workspace remains unchanged.

The engine recovered from one output limit and one transport timeout, rejected
three invalid file-hash proposals, and saved four successful edits. The final
JavaScript file contains 10,051 bytes. Four request contexts used rolling
compaction; the last admitted request estimated 9,157 input tokens and omitted
36 old message indices. There was no context overflow or echoed-feedback
completion. The deadline was reached during context planning after a read.

JavaScript syntax validation passed. Chromium over local HTTP failed at startup:
the generated script asks for a `game` canvas, while the existing HTML still has
only `c`. The game/input/main sections also remain unfinished. No generated file
was manually repaired, and this is not counted as successful task completion.

Batching's fixture reduction does not establish a live speed improvement. Late
tool-result-to-next-request gaps were 28.8, 52.1 and 68.7 seconds; this interval
includes context planning and admission. Repeated native counting remains costly,
alongside model generation, the transport timeout and incorrect hash proposals.
The fixed runtime defects are covered by passing regressions, but reliable
end-to-end completion of this task at these limits remains unresolved.

The checked binary is `target/debug/ww`. The separately installed `ww` executable
was not replaced. Run `cargo run --bin ww` from the repository to use these local
changes. All implementation changes remain uncommitted.
