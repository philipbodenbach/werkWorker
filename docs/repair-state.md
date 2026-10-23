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
- Rejected oversized file arguments include measured character counts and a
  smaller next-step ceiling, enforced for the affected normalized path from the
  contract saved with the next request's tool schema. Other files retain their
  ordinary limits. The
  ceiling halves down to 256 characters; successful repair releases this extra
  restriction. Invalid anchors or argument shapes alone do not shrink the
  new-content limit. Receipts report the actual attempted and allowed sizes;
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
decreasing argument ceilings and recovery to verified completion.

Validation on 2026-10-23: 252 Rust tests passed (two opt-in replay tests skipped
by the ordinary suite), plus nine Python tests, strict Clippy and rustfmt. The
saved-session repair replay was then run explicitly against the local SQLite
store in read-only mode: 750 canonical messages, 731 omitted, `tetris.html`
correctly missing with no stale hash/tail and one pending repair. General TUI
and effort/steering PTY checks passed using isolated fixtures. This validates
the harness behavior; it is not a live-model success-rate benchmark.

Follow-up validation of per-path repair limits: 254 Rust tests passed (two
opt-in replay tests skipped), plus strict Clippy and rustfmt. Added regressions
cover independent creation and editing of a second file during a pending
repair, normalized path aliases, accurate size receipts, and oversized anchors
without reducing the new-content budget. The stalled-repair cutoff remains;
these checks do not establish that a model will finish the game successfully.

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
