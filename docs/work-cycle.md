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
The suggested phase is build, repair after a failed check, or review after a
passing check. A later edit returns to build/check. Passing one check does not
establish task completion; the existing completion gate still checks coverage,
current file hashes and open plan steps.

After implementation starts, six investigation calls without an observed file
change or a distinct recorded check trigger immediate runtime steering back to
the current milestone. This is an advisory cadence, **not a new stop limit**.
Different shell reads, searches, plan updates and no-op edits cannot reset it.
An edit or a new check result clears it. Initial read-only exploration and
background-job waiting do not trigger this recovery. The steering does not
remove tools, bypass permissions, fabricate edits or mark anything complete.

The host emits the current checkpoint with the next decision rather than
depending on the last 1,536 bytes of rolling transcript excerpts. Runtime
steering consists of fixed host text and remains separate from arbitrary file
or plan content. Owned steering echoes use the existing response-retry path.
There is no extra inference call or model switch.

## Validation and limits

`tests/work_cycle.rs` exercises actual file creation, varied successful shell
reads, an executed failing public-API assertion, repair, a passing assertion,
and successful completion. It also checks checkpoint restoration without tool
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

Validation on 2026-10-24: the saved run first triggers recovery at invocation
12 of 207. The regular suite passed 298 tests (9 opt-in tests ignored); the
saved-run replay passed separately. Formatting and Clippy with warnings denied
passed. An additional isolated live-model probe could not start because the
configured local backend at port 11434 refused the connection. No running
services or installed worker binaries were changed.
