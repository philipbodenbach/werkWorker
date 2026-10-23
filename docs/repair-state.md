# Working state and stalled file repairs

The 2026-10-23 local Tetris run exposed a repeatable failure chain: a rejected
edit was followed by shell deletion, then compacted context advertised an old
successful read alongside fresh missing-file results. Oversized reconstruction
attempts and version checks prolonged the loop. Successful tool execution was
being confused with implementation progress.

## Worker behavior

- One chronological projection supplies both retained evidence and coding task
  state. Logical workspace path aliases share one record. New `not_found`
  results and complete directory listings supersede old hashes and edit tails.
  Listings only establish absence for visible, immediate children. Historical
  canonical messages remain intact; the current projection explains which
  observations supersede them.
- `process.run` compares hashes of up to 32 known files, each at most 1 MiB,
  before and after execution under the workspace lease. These observations are
  retained separately from potentially truncated process output. This is not a
  recursive snapshot or a rollback facility; unknown paths are observed when
  subsequently read/listed. Permission and size failures mean unverified, not
  absent. Processes started in the background still require later observations.
- In coding profiles, an unresolved file-edit/create error temporarily mounts
  the shell workspace read-only. Read commands and tests that use `/tmp` can
  still run. The model must repair with the ordinary atomic, hash-bound file
  tools. This covers `rm`, shell scripts and interpreter deletion without
  interpreting command strings or adding an approval dialog. A successful
  changed-file repair or a new user instruction ends the pending repair.
  Tests that write into the workspace must wait until the repair succeeds.
- Rejected oversized file arguments include measured character counts and the
  actual schema limit. Rejections never shrink the limit of subsequent edits:
  providers without verified generation limits retain the ordinary 4096-character
  content/replacement limit, or 2048 during output-limit recovery. With verified
  context/output ceilings, request-local section guidance replaces these fixed
  caps (see below). Invalid anchors or argument shapes do not shrink the
  new-content limit either. Legacy receipts advertising shrinking repair limits
  remain in the transcript, but the current repair projection marks those
  instructions obsolete and refers to the current tool schema.
  Receipts report the actual attempted and allowed sizes;
  the final stalled-repair error includes the path and last concrete error.
  Code is never mechanically truncated or partially executed.
- Read aliases share repetition counts. Unchanged version checks and no-op file
  edits do not reset them. Observed file mutations and distinct verification
  results permit continued work. Existing changed-read/research behavior remains.
- Completion after consolidation requires verification of all remaining changed
  files. Earlier outputs confirmed absent on disk do not require recreation just
  to obtain a hash. Inaccessible/oversized files are not treated as absent;
  every file named in the verification receipt must still have its verified hash.
- Repeated file repair failures are counted by normalized path, not argument
  identity. After `max_repeated_calls + 2` failures without a successful change
  of that file, the run reports `repair_no_progress` with its receipts preserved.
  This is a stalled-repair cutoff, not a total iteration limit. Reads, planning,
  compaction and resume do not replenish it. New user instructions start a new
  repair episode.

Regression tests in `tests/working_state.rs` use scripted providers and temporary
workspaces, including actual bubblewrap execution. No model, API key or local
inference server is needed. They cover deletion after a failed edit, stale
receipts after compaction/resume, output truncation, alias/version-check loops,
stable argument ceilings and recovery to verified completion.

Validation on 2026-10-23: 252 Rust tests passed (two opt-in replay tests skipped
by the ordinary suite), plus nine Python tests, strict Clippy and rustfmt. The
saved-session repair replay was then run explicitly against the local SQLite
store in read-only mode: 750 canonical messages, 731 omitted, `tetris.html`
correctly missing with no stale hash/tail and one pending repair. General TUI
and effort/steering PTY checks passed using isolated fixtures. This validates
the harness behavior; it is not a live-model success-rate benchmark.

The subsequent real-model run exposed a design error in the shrinking limits:
the model reduced its arguments from 4500 to 3992, 2606, 1025 and 314 characters,
while the harness lowered its ceiling from 4096 to 256. The four edits all had
matching hashes and unique anchors. The moving ceiling rejected otherwise
valid repairs. The extra per-file limiter was removed entirely; a valid edit
within the offered limit now executes even after a larger rejected attempt.
Regressions cover that exact size sequence, edits to other files, legacy error
projection, and the unchanged cutoff for genuinely unsuccessful repairs.
The opt-in `saved_size_rejection_replay` reads the failed session and applies
its actual first edit only in a temporary workspace, without inference or
modifying the user's game. These checks do not establish that the model will
finish the game successfully.

Validation of the stable-limit correction: 256 Rust tests passed (three opt-in
replays skipped by the regular suite), plus strict Clippy and rustfmt. The
saved size-rejection replay was run separately: the actual 3992-character edit
successfully applied after the 4500-character rejection. Its original file
bytes were reconstructed from the saved read and final newline, verified
against the original expected hash before execution in a temporary workspace.

## Protocol-informed sections and file tools

When the provider reports both context and output ceilings, each request's file
tools receive `x-werk-file-io` metadata and matching instructions. WerkProvider
gets these ceilings from `/werk/v1/chat/limits`, and the selected/default tier's
numeric reasoning allowance from `/werk/v1/chat/reasoning`. The actual request
output reservation, capped by server limits, determines the remaining guidance.
Unknown numeric reasoning uses a conservative estimate. Character targets are
explicitly estimates, never a tokenizer guarantee or a shrinking execution gate.
The ordinary 65536-character tool resource cap remains; actual output is still
subject to token admission. Current tool descriptions are included in token
counting and request fingerprints.

`files.read` returns complete line pages with `next_start_line`. If a line cannot
fit, it returns a UTF-8 byte page and `next_start_byte`; subsequent reads use that
cursor without line arguments. Page caps use available context and remain at
most 8192 serialized bytes. Files still have the existing 1 MiB read limit.

`files.insert` adds a complete section after a numbered line, with `0` meaning
prepend and `total_lines` meaning append. It requires the current `expected_hash`
and shares atomic writes, permissions, repair tracking and verification
invalidation with `files.edit`. Content is literal: no newline is inserted
implicitly. Receipts carry the new hash and, when there is a valid line boundary,
`next_insert_after_line`. Use `files.edit` for replacing an existing section.
Neither tool executes a truncated/incomplete model call.

The bundled coding profile includes `files.insert`. Existing custom/persisted
profiles keep their configured tool list; enable it explicitly or use the
existing `files.edit` fallback. This adds a built-in file tool, not a new plugin
loader. Existing host-side custom tools can still use `ToolRegistry`.

`tests/file_io.rs` exercises the real WerkProvider against an isolated HTTP
fixture: distinct server budgets, known/unknown reasoning, paginated reads,
hash conflicts, and successive atomic section insertion. It does not call a
live inference server or establish a model's task-completion success rate.

Validation of protocol-informed sections: 260 Rust tests passed, with three
opt-in replays excluded from the regular suite. The saved size-rejection replay
passed separately. Additional focused checks cover a 512-token output ceiling,
explicit reasoning overrides, refreshed profiles after context-driven output
reduction, byte-exact Unicode continuation, and insertion at empty/CRLF/EOF
boundaries. Strict all-target Clippy and rustfmt passed. The regression for
retaining loaded skills and directory evidence in small contexts also passes;
line-read projections avoid redundant byte metadata.

## Reference mechanisms

Primary sources consulted on 2026-10-23; mechanisms adapted to Worker's existing
tool receipts and policy boundary, without copying upstream implementation:

- [little-coder write guard](https://github.com/itayinbarr/little-coder/blob/main/.pi/extensions/write-guard/index.ts):
  distinguish creating files from editing them, return a concrete correction
  recipe, and account for shell paths around a file-tool guard.
- [little-coder quality monitor](https://github.com/itayinbarr/little-coder/blob/main/.pi/extensions/quality-monitor/index.ts):
  deliver correction at the next decision and bound ineffective corrections.
- [Claude Code checkpointing](https://code.claude.com/docs/en/checkpointing):
  preserve recoverable work independently of conversation compaction. Worker
  uses prevention during edit repair here; this change does not implement a
  Claude-style rewind interface or claim to snapshot the whole repository.
- [OpenAI structured patch workflow](https://developers.openai.com/api/docs/guides/tools-apply-patch):
  apply discrete operations, return precise success/failure, then iterate.
  This is public tool guidance, not a claim about private Codex internals.
- [DSH token-meter replay](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/llm/token-meter/README.md):
  derive consistent state from durable events rather than a model-written
  narrative. DSH's cited module measures context; the file-state application
  here is our adaptation, not an assertion that DSH uses this file algorithm.
- [DSH file tools](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/fs/tool-fs/README.md):
  paginated line reads, targeted atomic edits, and typed failures with explicit
  recovery instructions. Worker already has line ranges and `next_start_line`
  for capped reads; this pagination is separate from accepting write arguments.
- [DSH section editor](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/fs/tool-str-replace-editor/README.md):
  targeted insertion at a line boundary alongside view/create/replace operations.
  Worker's `files.insert` adapts that interaction to its own hash-bound atomic
  writes, permissions and persisted verification receipts.
- [DSH observation policy](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/fs/fs-observation-policy/README.md):
  distinguish unread, observed present and confirmed absent files; stale
  observations require a fresh read. This does not establish guaranteed model
  recovery or identical policy in every DSH deployment.
