# Keep implementation moving toward the requested result

## General execution contract

The target is a complete runnable core first, followed by behavioral checks and
focused fix iterations. Simplify optional polish to fit an output allowance;
do not substitute an unconnected scaffold for the first working version.
Independent implementations with disjoint file ownership, acceptance planning
and focused diagnostics can run concurrently within verified host capacity.
Check and repair phases retain admitted delegation and child-control tools;
the phase filter must not serialize independent helper work.
No model name, architecture or tokens-per-second threshold selects this workflow.

`process.run.verification.kind` distinguishes `behavior`, `syntax` and
`inspection`. Successful syntax/inspection does not establish behavior; a parser
failure remains useful repair evidence. Existing calls without the optional kind
retain their legacy contract. An arbitrary executable's semantics cannot be
proved from its exit code; checks still need real assertions through entry points.
Malformed interpreter invocations (for example a shell here-document placed in
one literal Python argument) are rejected before execution/approval with a
correction hint. Historical receipts for those invocations and rejected
inspections do not start application repair cycles.

`process.run` accepts optional `stdin` (at most 32768 UTF-8 bytes), for example
`{"argv":["python3","-"],"stdin":"print('ready')"}`. Input is delivered literally,
then closed; it is part of the exact prepared approval. No shell is inserted,
and timeout, cancellation, read-only mounts and all sandbox limits remain in
force. This permits one script/check call without a temporary-file write or
shell quoting. Inspection scripts retain the same verification rules as inline
argv scripts. Failure excerpts preserve both the beginning and the end of a
diagnostic so a long echoed source line cannot hide the final exception.

A failed check starts a durable repair cycle. Edits, changed error strings,
replanning and unrelated successful commands do not erase that obligation.
Rerun the original check after finishing a correction. If the check itself was
broken, a corrected check can supply `verification.kind: "behavior"` and
`verification.replaces: "<source_invocation>"` from `work_cycle.unresolved_check`.
It must pass against unchanged files and cover the original paths before it
resolves the cycle. Declaring a replacement alone has no effect.
An accepted legacy verification without `kind` can also explicitly replace a
check; the optional field must not silently invalidate otherwise valid evidence.
Declared syntax/inspection still cannot pass as behavioral verification.

The cycle is reconstructed from receipts across resume/compaction. A tool batch
counts as one model decision. After `4 * max(3, max_repeated_calls)` decisions
without resolution (12 by default), the runtime stops with
`repair_cycle_exhausted`, preserving work and reporting the unresolved failure.
If the boundary decision just applied an edit, one final check is offered before
stopping. This is a check-only phase: a passing recheck resolves the obligation;
a failed check or another decision exhausts the allowance. Edits cannot repeatedly
reset the limit. This prevents reporting an obsolete failure immediately after
the model has applied its correction.
This guard also applies in continuous mode. It bounds wasted work; it is not a
claim of completed implementation or universal model quality.

Automatic helpers use the configured wall allowance, not a separate fixed
three-minute cutoff. One replacement is allowed after failure and new parent
file changes; no retry loop, additional permissions or extra inference slots are
created. See [subagent limits](subagents.md).

## Previous investigation: action admission and resource-safe validation

The follow-up still does **not** establish reliable real-model completion. The
latest candidate fixes further concrete recovery dead ends, while live inference
has been stopped after the user reported system-wide freezes. Scripted success
is orchestration coverage, not a replacement for the original game acceptance
test with both native/default and explicit reasoning.

The latest changes are provider-independent:

- After a failed check, allow two diagnostic calls, then offer repair/check
  tools instead of continuing broad inspection. The same action phase applies
  to post-change repeated-read recovery. Admission also rejects out-of-phase
  calls returned by providers. A stale edit reopens inspection. Initial useful
  investigation and analysis-only requests do not force writes.
- Keep recovery active if a passing process receipt precedes any recovery edit.
  A source dump returning zero must not by itself restore ordinary exploration.
  Actual recovery changes require a new check; browser checks remain available
  when configured and permitted. Generic shell evidence still cannot prove
  arbitrary test-code semantics or full task coverage.
- Offer the existing hash-bound `files.write` replacement operation during
  repair. A complete runnable replacement no longer needs to repeat the entire
  old file as an edit anchor. Missing hashes remain create-only, stale hashes
  cannot overwrite, and policy/completion checks remain in force. This addresses
  a live proposal of a complete 10,125-byte game rejected by the previous coding
  schema. That proposal was not applied by the agent and is not a completed run.
- Restore prepared current source before its first read, filling known middle
  sections when the existing eight-page/12-KiB allowance permits. Preserve
  requested interface ranges before optional middle gaps. Compacted successful
  writes carry their applied receipts, not a lone stale pre-action diagnosis.
  Canonical history is unchanged; newer versions supersede old checkpoint text.
- Start the existing automatic acceptance reviewer only after a current passing
  check in **Conservative** delegation mode. **Active** (now the default) starts
  after the first actual file change; **Max** can start a check-planning helper
  once a substantial plan exists. See [delegation levels](subagents.md).
  An earlier explicit-reasoning live
  test started review too early; its child exhausted the wall allowance and
  the shared provider correctly refused further uncertain inference. Review
  still cannot replace actual verification or grant capabilities.

No additional model call, model-name branch, reasoning reserve, context limit,
or hard repetition limit is introduced. Reasoning and generated tool arguments
continue sharing the negotiated output allowance. The latest complete-file
repair path has a real-filesystem regression covering failed verification,
bounded diagnosis, stale-hash rejection, repair, passing verification and final
completion. Its real-model completion remains unverified.

The freeze investigation found the previous 2026-10-22 diagnosis in the sibling
Werk repository (`docs/benchmarks/2026-10-22-system-crash/diagnosis.json`). It
records NVIDIA TDR events followed by a native server abort, without proving
the hang trigger. Worker commit `b16fc65` prevents further inference and token
counting on a connection after uncertain cancellation/transport failure; its
regressions still pass. Backend commit `11d4ac4` bounds overlapping telemetry
and memory probes and preserves similarity routing with multiple slots. Those
source protections remain present. The running Werk binary is dated October 24,
matches the installed binary, contains the memory-probe marker and uses the
corrected multi-slot cache flags. This is not proof that the current freeze has
the same cause or that driver/backend faults are eliminated.

After stopping only the isolated test processes, guest RAM remained about
75 GiB available, swap unused and memory PSI zero. The 24-GiB GPU had only a few
hundred MiB free. No matching Windows System event was found in the queried last
eight hours. These post-event samples neither prove a leak nor exclude an earlier
peak, host pressure or driver stall. The user's model server remains running;
no model/runtime configuration or installed user binary was replaced.

`scripts/tests/live_resource_guard.py` is an opt-in Linux/WSL test wrapper. It
serializes guarded runs, checks available RAM/GPU memory before launch, records
process-group RSS and memory samples, and terminates only its owned test group
on pressure or deadline. For local GPU tests, use `--require-gpu-metrics`;
missing/partial metrics then fail closed. Defaults require 8 GiB available RAM
and 2 GiB free GPU memory. These are conservative test-admission limits, not a
claim that a particular model is safe. The current GPU fails preflight, so no
further inference is launched. A disconnected client does not prove backend
cancellation; this wrapper does not alter that existing limitation.

CPU-only CI uses two build jobs, two test threads, low scheduling priority and
the same sampling wrapper with GPU admission disabled. GPU memory is observed
but no model inference is requested. End-to-end completion must still be tested
serially on a backend with adequate measured headroom, using the original goal,
independent behavioral acceptance, and explicit reasoning as well as defaults.

Local validation of this candidate passed: 310 regular Rust tests (10 opt-in
tests ignored), 14 Python tests, formatting, Clippy with warnings denied, seven
TUI/setup smokes, three examples, packaging, installation in a fresh directory,
and four installed-command checks. This executes the GitHub workflow locally;
it is not a claim of a remote pipeline run. The user subsequently authorized a
temporary single-slot server restart for serial, monitored live validation.

That monitored replay used binary SHA-256
`82e058b31dd1055f2c3360bcad6d01cbe44333cf21e8026054c7291d60b62e4c`
and the original 10,589-byte file/hash and goal/image, with 16,384 context tokens
and 4,096 output tokens. After the single-slot restart, 2,409 MiB GPU memory was
free; the separate image warmup completed. The first resumed decision received
the repair catalog, four current source pages and hash-bound replacement support.
Before that answer completed, `nvidia-smi` timed out. The guard stopped the test
with exit 78 instead of launching more requests. Last successful measurements
showed at least 2,370 MiB free GPU memory, approximately 84 GiB available guest RAM,
zero swap and at most 102 MiB test-group RSS. Missing GPU measurements do not
establish whether an unobserved peak or driver stall occurred.

Werk logged cancellation at 2026-10-26 09:59:26 UTC; a subsequent native slot
query confirmed no inference was processing. No matching Windows System driver
reset event was returned for the queried hour. This reproduces a GPU-monitoring
timeout, not a proven memory leak or a verified reproduction of the user's full
system freeze. The explicit-reasoning case was consequently not started. No new
implementation/check completion was observed in this attempt. The original
two-slot server command is restored after this temporary experiment; the worker
installation remains unchanged. More load tests are deferred until the GPU/
backend stability issue is understood.

## Original saved-loop investigation

The saved Tetris run inspected on 2026-10-24 executed 207 tools. Its last file
change was invocation 6; 201 further calls produced no implementation change.
Most shell calls succeeded because they read code or checked syntax. Exact-call,
repeated-read and failed-command guards therefore did not describe the problem:
the agent repeatedly recovered details without finishing a runnable version.
The final-answer guard eventually stopped it as incomplete.

## Reference mechanisms inspected

These projects can also stall. Their sources support specific mechanisms, not
a guarantee that their agents always finish.

- **dsh:** [summarizer.ts](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/compaction/compaction-basic/src/summarizer.ts)
  builds a continuation checkpoint containing original/evolving intent,
  unfinished work, current work, decisions and one next action.
  [region.ts](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/compaction/compaction-basic/src/region.ts)
  retains recent message groups and rejects a replacement summary that does not
  save space. [tool-todo](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/todo/tool-todo/src/index.ts)
  records concrete steps with active/completed status.
- **little-coder:** [quality-monitor](https://github.com/itayinbarr/little-coder/blob/main/.pi/extensions/quality-monitor/index.ts)
  delivers corrections as immediate steering, with a cap on consecutive
  corrections; its [detector](https://github.com/itayinbarr/little-coder/blob/main/.pi/extensions/quality-monitor/quality.ts)
  checks exact consecutive calls and allows repetition after possible state
  changes. That detector alone would miss many varied reads in this run.
  [evidence-compact](https://github.com/itayinbarr/little-coder/blob/main/.pi/extensions/evidence-compact/index.ts)
  keeps evidence outside the compacted message array and reconnects the agent
  to it. [phase-model](https://github.com/itayinbarr/little-coder/blob/main/.pi/extensions/phase-model/phases.ts)
  distinguishes planning and action; it is model handover, not an automatic
  implementation/review state machine.
- **Hermes:** its documented [context compressor](https://hermes-agent.nousresearch.com/docs/developer-guide/context-compression-and-caching)
  preserves recent tool groups and summarizes goals, decisions, completed/open
  work and next steps.
- **Claude Code:** the official [workflow guidance](https://code.claude.com/docs/en/best-practices)
  separates exploration/planning from implementation and recommends executable
  feedback for repeated correction. This is documented behavior, not an audit
  of proprietary implementation internals.
- **Codex:** the official [agent-loop description](https://openai.com/index/unrolling-the-codex-agent-loop/)
  explains continuation through compacted context and structured tool results.
  Its hosted compaction preserves model state; that API-specific mechanism is
  not assumed available to werkWorker's local models.

## Adaptation in werkWorker

Coding guidance now targets a runnable end-to-end first version, followed by
review and repairs until the complete request is satisfied. Output-sized edits
are chunks of that version, not separate research tasks.

`task.plan` accepts an optional bounded `checkpoint` with `next_action` and
`decisions`. Save it at meaningful phase changes, including interfaces and
assumptions needed to continue. The existing durable plan carries it through
serialization and context compaction; old plans remain valid. The checkpoint
is lower-authority model-authored data, never proof of completed work.

`coding_task_state.work_cycle` reconstructs the original task, latest steering,
current milestone, last actual change and latest check from saved run data.
The suggested phase is build, check when verification is due, repair after a
failed check, or review after a passing check. A later edit returns to build/check. Passing one check does not
establish task completion; the existing completion gate still checks coverage,
current file hashes and open plan steps.

After implementation starts, six investigation calls without an observed file
change or a distinct recorded check trigger immediate runtime steering back to
the current milestone. With an implementation change since the latest check,
this also makes behavioral verification due as described below. This is a
workflow transition, **not a new stop limit**.
Different shell reads, searches, plan updates and no-op edits cannot reset it.
A new check result clears the pending check; more edits cannot clear it. Useful
initial exploration and background-job waiting do not trigger this recovery.
Redundant initial reads have their own recovery path, described below.
The host does not bypass permissions, fabricate edits or mark anything complete.

The host emits the current checkpoint with the next decision rather than
depending on the last 1,536 bytes of rolling transcript excerpts. Runtime
steering consists of fixed host text and remains separate from arbitrary file
or plan content. Owned steering echoes use the existing response-retry path.
There is no extra inference call or model switch.

The control flow is provider-independent. It consumes normalized tool receipts,
workspace versions, the durable task/plan and recorded checks; it does not
branch on model names or infer an effort budget from a model family. Request
admission enforces the check phase even if a provider returns a tool outside
the offered catalog. Context/output sizing uses the provider's negotiated
limits and selected reasoning profile. Qwen is a live regression subject, not
a runtime dependency or a special recovery implementation. Command inspection
recognizers are supporting evidence filters, not the primary loop mechanism.

## Validation and limits

`tests/work_cycle.rs` exercises actual file creation, varied successful shell
reads, an executed failing public-API assertion, repair, a passing assertion,
and successful completion. Its counter function and assertion use `/bin/sh`,
so the fixture does not depend on Node being installed inside the sandbox.
It also checks checkpoint restoration without tool
transcripts and preserves analysis-only work. This uses a scripted model to
test orchestration deterministically, not to measure a real model's compliance.

An opt-in, read-only replay of the saved session locates the first recovery
point without executing its tools or modifying its workspace:

```sh
WW_REPLAY_DATABASE=/path/to/sessions.sqlite \
WW_REPLAY_SESSION=session_id \
cargo test --lib saved_loop_is_redirected -- --ignored --nocapture
```

Replay proves when the new steering appears and what goal information survives;
it cannot prove that a fresh model response would finish the original game.
Model ability, available context and actual test quality still matter. Existing
circuit breakers remain the last resort when the model ignores recovery.

Earlier validation on 2026-10-24: the first saved run triggers recovery at invocation
12 of 207. The regular suite passed 298 tests (9 opt-in tests ignored); the
saved-run replay passed separately. Formatting and Clippy with warnings denied
passed. An additional isolated live-model probe could not start because the
configured local backend at port 11434 refused the connection. No running
services or installed worker binaries were changed.

## Follow-up: recovery must preserve the code needed to act

A subsequent real run still failed with `repetition_limit`: 53 invocations, the
last actual edit at invocation 18, then 35 calls without a change. After read
25, read recovery removed `files.read` from the offered tools. The model moved
to searches, eventually repeating `placeholder__resume`. Compaction retained
only a 2,048-byte excerpt of the last HTML read; the edited JavaScript's
constants, state and functions were no longer present. Work-cycle steering
alone did not resolve this failure.

Read repetition no longer removes `files.read` from the implementation tools.
It restores current-version pages of the active milestone, the most recently
edited file and the latest read file from durable receipts, including when
their original message groups have been compacted. This retention starts with the first reads, before
another repeated-read warning. It does not depend on the model writing a plan
checkpoint. The working set is bounded to eight pages and 12 KiB of content;
covered pages are deduplicated, with wider pages preferred at the same offset.
Already-present full reads/writes are not copied into the request again. A
successful edit retains its full prepared result outside the transcript; when
its content hash matches the current observed version, that result supplies
current head/tail pages even after every old read hash has become obsolete.
Large files retain up to 6 KiB from the head and 2 KiB from the tail, with UTF-8
boundaries preserved. Head/tail pages take priority so broad earlier reads cannot
evict the append anchor. Previously inspected middle line windows are materialized
from the committed current version as well, so earlier edits do not invalidate
the working knowledge of those interfaces. Covered ranges are removed before spending the shared
context budget. The active milestone takes priority over incidental side edits.

Each restored page carries its original source, offset and hash as historical,
lower-authority evidence. Later edits, observed shell changes and missing-file
observations invalidate older versions. Nothing is silently reread or counted
as new progress. If all restored copies prevent context admission, the builder
tries the pages in priority order and skips individual pages that do not fit the
provider's token count. A large head cannot exclude a smaller usable tail. After
counting mandatory context, this takes at most eight additional token counts for
the eight optional pages, with no additional generation calls. The model can still request a
specific missing range. Repeated reads and identical calls retain their existing
last-resort stop limits. No tool permission or completion requirement is relaxed.

The regression `compaction_preserves_edited_code_when_latest_reads_target_another_file`
checks this exact multi-file failure shape, serialized receipts, source hashes,
stale-version invalidation and constrained context admission. The read-only
`saved_read_loop_recovers_an_actionable_request` replay checks the actual
JavaScript declarations and HTML script reference from the second saved run.
These tests establish the recovery contract; real-model completion is a
separate measurement.

The working-context regression also checks 0, 2,048 and 3,072 reasoning tokens
with a 4,096-token total output reserve. Restored code consumes input context;
reasoning and the tool-call JSON share output. With 2,048 reasoning tokens, the
existing file-I/O budget advertises about 3,328 new characters after JSON
headroom, rather than treating all 4,096 tokens as code output. Native token
counting and dispatch use the same selected effort and adjusted tools.

Review of the incremental workflow exposed another gap: real but small edits reset
investigation recovery without ever exercising the entry point. After at least three
observed changes and six investigation calls since the last distinct behavioral
check, `work_cycle.check_due`
now remains true and the suggested phase becomes `check`. Immediate steering
requests startup and main-interaction smoke coverage of the current version.
Further edits, no-op proposals, reads and plan updates cannot clear this
checkpoint. Six consecutive investigation calls without progress after even a
single change also make that check due; merely recommending execution left this
read-only loop open in the live replay. A distinct recorded check transitions to repair or review and
starts the next cycle. When a permitted `process.run` is available, the request offers that check tool
with required `verification` for the next decision. A recorded result, including
a failure, restores the normal tool catalog. `interaction.ask` remains available
to report a real blocker. If execution is unavailable, the
existing tools remain available. No host permission changes and no test result
is fabricated. Already admitted calls retain their original authority.
`repeated_small_edits_cannot_postpone_a_behavioral_check` executes that transition
with real shell behavior, including an intervening no-op and another edit.

The check phase is enforced at admission for new model proposals: calls outside
its offered tool receive `work_cycle_check_required` without preparation,
approval or execution. The original implementation permissions remain intact,
and calls already admitted in a prior decision are not revoked. Refusals still
obey the existing identical-call repetition limit. This closes the previous
loophole where an unoffered tool fell back to its registered definition and
could bypass request-local limits. Consecutive productive edits without the
accumulated investigation pattern retain the normal implementation catalog.

The recovery decision uses the durable task, checkpoint, current file pages
and two recent complete assistant/tool groups. Older investigation narratives
are omitted from the request without deleting canonical history or user/system
constraints. This also provides one bounded recovery when a provider rejects
context without advertising a smaller verified limit: rebuild from the
checkpoint and retry only if the counted/estimated input actually shrinks.
Another rejection stops; no backend limit is invented and existing retry,
request, step and wall budgets still apply.

The live replay also exposed false progress from labeling source inspection or
syntax validation as `verification`. Direct source/metadata readers and known
standalone syntax modes do not produce behavioral receipts when they succeed.
A syntax failure still allows repair. A narrow inline Node inventory consisting
of `fs` reads, string/regular-expression queries and console output is likewise
inspection. Unknown calls, application imports, VM evaluation and assertions
retain the ordinary verification contract. This conservative recognizer is not
a JavaScript parser or a proof of arbitrary test semantics. These filters
supplement the general state/checkpoint mechanism and actual execution/review.

The regressions run real file edits, successful reads, syntax checks, a rejected
premature final answer and public-function assertions. They verify restoration
of the current file head/tail after all reads become stale, corrupted prepared
content rejection, Unicode boundaries, retained user constraints and a single
smaller-context retry. None requires Qwen or Node to pass CI.

Live probes use isolated workspace/database copies and the explicit Qwen
`high` tier (2,048 reasoning tokens, 4,096 total output tokens), with a 32,768-token
context matching the supplied server launch. The installed baseline repeated
`placeholder__resume` and failed; candidate context restoration produced edits.
Those observations alone do not establish completion of the original game.
Each tested executable is copied to a hash-named path and its running process
image is verified. This caught a local Cargo cache issue after packaging:
the normal build's dependency fingerprint pointed into the extracted package,
so an apparently fresh CLI could contain older code. Invalidate that generated
fingerprint or use a fresh target directory before another live build.

Latest validation on 2026-10-25: all 305 regular Rust tests passed (9 opt-in
tests ignored), including all six work-cycle regressions. The saved-session
replay passed separately. Formatting, Clippy with warnings denied, Python tests,
all seven TUI/setup smokes, the three CI examples, packaging and installation
from the extracted package also passed. These are local executions of the
GitHub workflow steps; no remote pipeline run or repository publication occurred.

The final isolated live probe used executable SHA-256
`4c469f792b436ec63877ae093518da18a7305ba0539fb6321637b4ad1f280da8`.
It entered the mandatory check phase, but its command only printed JavaScript
declarations while claiming to test input and gameplay. That arbitrary inline
program passed the generic exit-code/unchanged-file verification contract;
three more reads followed. The probe was deliberately stopped with the original
task incomplete. This is a remaining false-verification boundary, **not** evidence
that the game completed or that every investigation loop has been eliminated.
The changes fix reproducible context-loss and workflow-admission faults; the
real-model result does not justify calling the overall completion problem solved.

## Follow-up: repeated inspection before the first change

The next saved failure contained 15 calls and no file mutation. Three reads
already covered all 235 lines of `game.js`, but the model repeatedly requested
the same sections. The previous recovery required an implementation change;
the working set also favored the latest read and had no edited file to retain.
Switching to `index.html` therefore lost useful JavaScript context. The
repeated-read breaker eventually stopped the run without a runnable increment.

At the existing repeated-read recovery threshold, the host now rebuilds the
decision context even before the first mutation. It retains the original task,
checkpoint, current file evidence and two recent complete tool groups. Immediate
steering asks implementation tasks to produce a small runnable increment and
check it; analysis-only tasks should report supported findings without editing.
Useful initial reads do not trigger this transition. The original tools remain
available; permissions, completion requirements and stop limits are unchanged.

The bounded working set now also includes recently read distinct files, up to
four paths, after prioritizing milestone/edited files. This starts during initial
inspection and uses the same eight-page/12-KiB content limit and current-version
validation. Switching from code to markup no longer selects only the markup.
No model-name, language, reasoning-effort or provider-specific branch is involved.

The regular regression exercises both implementation and analysis-only requests
against real files before any mutation. It checks retained code and notes,
fresh recovery, unchanged read permissions, an actual shell assertion after
repair, and an unchanged workspace for the analysis-only case. The optional
`saved_initial_read_loop_keeps_code_when_switching_to_html` replay accepts
`WW_REPLAY_SNAPSHOT=/path/to/session.json` and checks the actual saved failure at
calls 13 and 15. Both retain JavaScript state, shapes and the append anchor plus
the HTML script reference. Its offline token approximation is not a live
image-token measurement or proof of model completion.

Validation for this change: 306 regular Rust tests passed, with 10 opt-in tests
ignored; the new saved-run replay passed separately. All local GitHub workflow
steps passed, including Python tests, Clippy, the seven TUI/setup smokes, three
examples, packaging and installation from the extracted package. These results
do not represent a remote pipeline run.

The repeated pasted failure was the same persisted run: its ID, start time and
15 invocations were unchanged. `Opened saved session` replays that history.
Starting the executable again does not start a new conversation; use `/sessions`
and `New session`, then submit the task, to obtain a fresh run.

An isolated genuinely fresh 16,384-token live run with the original image, existing
files and default reasoning configuration still repeated reads after the first
pre-implementation recovery change. Its context snapshots exposed another fault:
when all restored pages exceeded the input allowance, the old fallback discarded
every page, including those that would fit. The constrained-context regression
reproduced this with zero retained pages instead of one. The budget selection
above fixes that all-or-none fallback and preserves the full output reserve in
the regression. The admitted request is recounted; no provider limit is guessed
and no reasoning tokens are silently reassigned.

The final isolated live test used executable SHA-256
`0b46c3d46681a40dc02259aff7d9661bd1a234734ce8501551a54b54c9b1d6d1`,
the original task/image and hash-verified initial project files. It kept the
16,384-token context, 4,096-token output reserve, default/native reasoning and
repetition limits. Continuous budget renewal was disabled in the test clone so
the original 900,000-ms wall allowance bounded the experiment; neither the user
database nor running user services were changed.

This run executed 37 tools. Call 25 changed HTML but duplicated existing footer
elements; call 28 appended JavaScript. Call 35 entered the check phase and ran
`node --check game.js`, which failed with `SyntaxError: Unexpected token '}'`.
Two further reads followed, then the test exhausted its wall budget. The game
did not complete. The first edit, a failed check and avoiding the original
read-only stop are not sufficient evidence that end-to-end convergence is fixed.
This remains an open behavioral limitation, not a successful game benchmark.

After the final context-budget change, all 306 regular Rust tests and all local
workflow steps passed again (10 opt-in tests ignored). The saved initial-read
replay passed separately. The constrained-context regression first failed on
the old all-or-none fallback and passed with partial retention. No installed
user binary was replaced; the tested builds and databases are isolated artifacts.


## Follow-up: keep recovery active until action, with usable anchors

The subsequent `run_3aac73e0-4a89-40d4-af7c-e745152a131e` was a new run using
the updated executable. It made 27 calls and stopped at `read_no_progress` on
`index.html`. Its last real change was call 11. Two later edit proposals failed
because their anchors contained literal escaped quotes absent from the file.
One proposal also copied a rejected-proposal placeholder into its replacement.
The recovered current file was 10,589 bytes; its prepared successful edit and
SHA-256 were used to reconstruct an isolated replay, without restoring old
content into the user's workspace.

Three corrections address mechanisms reproduced from this run:

- Restored source is a verbatim fenced body with separate provenance/hash/offset
  metadata. JSON escaping is no longer embedded in the displayed anchor. The
  fence exceeds any backtick run in the body; source remains lower-authority
  evidence. Regression coverage applies the rendered quoted/Unicode/backslash
  anchor to a real file and checks the resulting content and version.
- Optional context selection skips oversized pages while retaining smaller
  pages that fit the actual provider count. The previous priority-prefix search
  could discard a usable tail when its higher-priority head did not fit. The
  constrained-budget regression fails on that implementation and passes with
  the same output reserve after the correction.
- Read recovery is an episode that survives newly inspected ranges, another
  file, failed edits and no-op edits. A real observed version change or distinct
  verification result advances it. A recovery edit transitions directly into
  the existing mandatory behavioral-check phase, rather than immediately
  reopening exploration. A distinct check (including a failure) releases that
  phase for review or repair; a successful syntax-only check does not. A failed
  check keeps recovery context active throughout the next repair, whose edit
  requires another check. Only passing verification ends the episode. Previously
  a read returning a few new bytes
  immediately removed the refreshed goal/checkpoint context, even though no
  implementation progress had occurred. The regression checks the immediate
  steering across these decisions and its removal after a real repair. Analysis
  requests still finish with findings and without forced file changes.

These changes do not alter model selection, reasoning effort, output reserves,
permissions or the hard repetition limits. Reasoning and tool-call output still
share the negotiated output budget. Additional context counts are bounded;
there is no new reasoning or model-generated summarization call.

The project's persisted explicit tool list also predates `files.insert` and
`browser.check`, so those tools are unavailable in its runs despite being present
in the newer bundled coding profile. The replay keeps that original tool list.
The fix does not silently override explicit project configuration or grant new
capabilities.

The full suite exposed a separate timing-sensitive approval test under load.
Its active-work allowance is now two seconds; approval still deliberately takes
longer than that allowance, and a stalled post-approval model still hits the
unchanged production deadline. Exact pause-credit and no-double-credit assertions
remain. The generic completion wait guard now also uses the existing 30-second
approval wait allowance, after a separate approval/file-write test exceeded its
old five-second test guard under full-suite load. No production timeout was increased.

Validation of the complete action/review change: all 306 regular Rust tests
passed (10 opt-in tests ignored), including all seven work-cycle tests and the
read-recovery fixtures. The latter now explicitly review the first recovery
increment, observe a real failed assertion, repair it and verify the final state.
Formatting, Clippy with warnings denied, all nine Python tests, all seven
TUI/setup smokes, three examples, packaging, installation from the extracted
package and four installed-command smokes passed. These were local executions
of the GitHub workflow; no remote pipeline was published or run.

The isolated live comparison used the original task/image and saved file content,
16,384 context tokens, a 4,096-token total output reserve and default/native
reasoning. The test kept the project's old explicit tool list. Reference-image
warmup was a separate diagnostic request on the same provider connection so
its existing image-overhead estimator could learn from native usage; it is not
an additional call introduced by recovery in production.

The action/review candidate with SHA-256
`44cb57da525615be0ced831fbfdcba20a4c3c9881302a0aadab51ca00448bb95`
made two real edits (calls 25 and 36) and immediately checked the first one.
That check failed because the script block was unfinished. The following repair
returned to inspection; 40 tools had executed when the test's 48-step allowance
ended. Continuous renewal was disabled to bound the experiment. This established
an action/check transition, not completion of the game. The captured context
showed that failed review still released focused recovery, which the final
repair-retention regression reproduces and fixes. The original database,
workspace and installed user binary were not replaced.

The final production-code continuation used executable SHA-256
`35f0b2e970226b9124ac009dcf83974ca271d815aa846b8a69c4a16c1e5abcba`.
Its actual request retained the failed-review repair instruction after the next
check. It nevertheless made two more process calls, two reads and one search,
with no file change, and exhausted the remaining shared request budget. The
five-minute/eight-step test caps did not remove that shared budget. This is not
a completed live benchmark: the implementation corrects reproducible state and
context defects, but reliable end-to-end convergence remains unresolved.

A separate diagnostic tried a complete current file, compact host guidance,
current failure/last receipt and a smaller edit/check tool catalog. Native input
counting still had to preserve the 4,096-token output reserve and 512-token safety
margin inside the 16,384-token window. Some candidates did not fit; admitted
candidates still chose source inspection, and a longer error result exceeded the
remaining context allowance. This experimental context builder is not part of
the repository change. The live results do not justify shipping that additional
restriction or claiming that supplying a complete file alone solves the loop.

## October 26: source inventory incorrectly admitted review

The saved ordinary `ww` run starting at 11:24 CET wrote and extended
`tetris.html`, then repeatedly inspected it. Its sole purported verification
read the source with Node and printed `has render: false`, `has audio: false`
and `has loop: false`. Exit zero incorrectly produced a passing receipt and
started the automatic reviewer. The parent eventually failed at 11:37 with an
incomplete HTTP chunked response; the reviewer used its six-request budget.
That transport failure is separate from the invalid verification receipt.

The narrow inline inventory recognizer now rejects this receipt as behavioral
evidence. Recovery and repetition replay also reclassify old stored receipts,
so resuming cannot restore the false pass. The real saved run replays offline
with the check phase still due and no passing behavioral check. This uses no
model-specific rules or additional model/reasoning calls. Actual execution,
VM harnesses and unknown programs retain the existing contract; arbitrary test
semantics and end-to-end completion are still not guaranteed.

The Windows platform workflow also exposed a Linux-only `fcntl` import in
the live-test resource guard. Pure admission checks now import on Windows and
macOS; invoking the Linux monitor there refuses before sampling or launching
anything. Linux resource monitoring and locking remain unchanged.

Validation: 312 Rust tests passed with the real sandbox required (11 opt-in
tests ignored), plus formatting, Clippy with warnings denied, 16 Python tests
and the setup/reasoning PTY smokes. All seven resource-guard tests also passed
with native Windows Python 3.10; the complete Windows Python suite requires
3.11 or later for `tomllib` (GitHub uses 3.12). The saved-run replay and tests
use no real model or GPU. No remote workflow was rerun and no installed command
was replaced; live convergence and system stability remain unverified.
