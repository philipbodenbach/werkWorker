# Keep implementation moving toward the requested result

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
retains the highest-priority prefix that fits the provider's token count. After
counting the mandatory context, bounded binary selection takes at most three
additional counts for the eight optional pages. The model can still request a
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
A syntax failure still allows repair. Arbitrary inline programs are deliberately
not classified from language-specific code patterns: that would be a brittle
substitute for actual execution and review. These filters supplement the general
state/checkpoint mechanism; they cannot prove arbitrary test semantics.

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
