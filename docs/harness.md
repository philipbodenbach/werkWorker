# Adaptive harness

New runs use one lean, adaptive harness through the existing model/tool loop.
There is no mode choice, supervisor, second agent engine, classifier request,
model switch or additional permission grant. A successful ordinary run receives
no precautionary planning, review, budget-reminder or recovery rules.

## Startup and compatibility

```sh
ww --werk-config examples/werk-config.json run --profile general-readonly "Compare the supplied notes"
```

The SDK default, TUI, RPC and CLI all start adaptive root runs. Optional
`--harness-config harness.json` / `EngineBuilder::harness` configure explicit
completion criteria and host guidance; neither requires a mode field.
`HarnessConfig::default()` means adaptive. Explicit host `local_guidance` is
limited to 8,192 bytes and is not inferred from a model's name or capabilities.

Old `classic` / `minimal` config values and the hidden legacy `--harness` flag
remain readable. New root runs normalize these values to `adaptive` and emit
`harness_migration_new_run_adaptive`. Already enqueued or running runs retain
their stored classic/minimal contract, including completion rules and guidance.
Opening a session migrates absent, version-1 or version-2 harness records to version 3,
emitting `harness_migration_legacy_contract_preserved`. Absent legacy state or
an omitted legacy mode means classic. Unsupported future versions are rejected.
New follow-ups are adaptive even in an old session. A child inherits its active
parent's contract, so an old parent's execution rules remain intact; root
completion criteria do not become acceptance criteria for independent child tasks.

The reserved `SessionData.metadata._werkworker_harness[run_id]` contains
`version`, `config`, completion `checks`, canonical feedback `messages` indices,
and `adaptive`. Other object metadata is preserved. Legacy scalar/array metadata
is retained under `_werkworker_legacy_metadata` when the namespace is added.
Hosts must preserve these reserved fields on export/import. Existing public
struct literals, RPC methods, event variants and session schema remain compatible.
Use `harness::config(&session, &run)` to inspect the effective saved contract.

## Observation, support and removal

Adaptive decisions use durable host observations, never assertions in generated
text, documents, skill bodies or child output. The system prefix contains the
short task instruction, full profile requirements, scoped project snapshots, available skill
metadata and requested output schema. Tools and their schemas retain their
normal authority. Missing AGENTS.md, skill directories or SKILL.md are optional;
unavailable configured skills are omitted from snapshots and `skills.load`
returns `available: false`. Invalid or unsafe files still receive normal
validation. Missing optional guidance does not trigger stronger control.

| Observed trigger | Bounded assistance on the next eligible request | Removal condition |
| --- | --- | --- |
| Tool validation error | Reference the exact invocation, tool and error; consult its schema and correct the rejected call | Same tool succeeds, or two complete responses |
| Model output limit | Reference the discarded attempt; preserve earlier results and use one smaller complete action | A complete model response |
| Truncated tool result | Reference the receipt; use recorded artifacts only when present, or a narrower query | Notice expires after two complete responses; this does not prove missing content was read |
| Actual context compaction or verified context-limit error | Use retained evidence and retrieve only necessary omitted details | History fits again, or two complete responses |
| Missing requested information (`not_found`, `file_not_found`, `missing_information`) | Reference the failed receipt; consult a relevant source or ask for that input | Same tool succeeds, or two complete responses |
| Repeated identical action without new evidence | Reuse the receipt or select a different relevant action | New evidence, or two complete responses |

A hint is request-local, with a persisted trigger ID and scope. Each episode
gets one delivery; unresolved repeats do not inject the same hint again. After
deactivation, two complete model responses must pass before a different scope can
activate that kind again. The previous identical scope remains suppressed;
at most three episodes of each kind are allowed in a run. This also bounds
alternating-error behavior. Terminal runs deactivate any remaining help.
Repetition, missing information and context pressure never enable a general
"strict" mode. Output/schema/completion recovery retains its existing budget.

Tool truncation uses `tool_result_truncated`; a discarded model generation uses
`generation_output_limit`. Each has its own trigger, scope, delivery step,
episode allowance, cooldown and expiry. A still-active or cooling-down tool
notice cannot consume the generation recovery. The generation trigger identifies
the discarded attempt: no calls from that attempt executed, while earlier
confirmed work remains valid. No skeleton-file or insertion-marker workflow is
required. Successful generation resolves generation recovery, not missing file
content. The tool notice's `scope_expired` reason denotes expiry, not completion
of the underlying information task.

Version-2 `output_limit` states migrate using `generation_limit` or
`<tool>:truncated` scope, with a matching durable attempt/result trigger as a
fallback. Delivery markers, observation cursors, episode counts and cooldowns
survive the rename. If the retained tool notice suppressed an already observed
generation failure in the unfinished error sequence, migration restores that
pending generation episode from its durable attempt without rewinding cursors
or resetting retry budgets. Ambiguous records, including conflicting scope and
receipt evidence, remain under `output_limit`, verbatim in
metadata; any still-pending notice uses neutral uncertainty text instead of
repeating unverified execution/artifact claims. Their spent episode allowance
and cooldown are reserved in both new slots. Reopening cannot rearm a consumed
notice. Historical events are never rewritten; session/event schema stays at
version 1. Unsupported future harness versions still fail closed.

Aids, observation cursors, delivery steps, success counters, cooldowns and removal
reasons persist atomically in the ordinary session store. `Diagnostic.code`
records `harness_<kind>_activated:<trigger-id>`, `..._delivered`,
`..._suppressed_<reason>:<trigger-id>` and `..._deactivated_<reason>:<trigger-id>` alongside existing context,
attempt and tool events. Metadata contains the scope and bounded episode count.
Context-pressure assistance follows the observed compaction; the first compacted
request already contains retained receipts and skill content. No additional
classification or planning inference is performed.

Context planning records delivery with the exact request. A failed/completed
attempt consumes that delivery; an interrupted dispatch with no outcome can
reconstruct the same pending episode on resume. Transport retries reuse their
existing request. Hints are not appended repeatedly to canonical history.
Canonical schema/completion/child findings keep separate provenance indices.
Feedback uses the supported assistant role and a structured
`source: werk_worker.harness` envelope; it cannot grant authority. Real steering
remains a user message.

## Context, resources and execution

Compaction uses canonical history, artifacts, receipts and complete tool groups.
User/system/developer constraints remain verbatim. Omitted successful skill loads
restore the full immutable skill snapshot as lower-authority evidence. Large
indispensable constraints can still produce explicit context overflow.

WerkProtocol supplies verified context/output limits, token accounting and
execution capabilities. These determine resource admission, sizing, compaction
and concurrency; they do not establish reasoning or planning competence. No
adaptive support decision is based on a presumed "weak" or "strong" model.
`input_tokens_*` diagnostics identify accounting provenance; numeric
`ModelCompleted.metadata.werk.timings` remains observational.

The repetition guard recognizes new or changed read observations and successful
workspace actions. Unchanged reads cannot reset one another. Polling a proven
active owned process/child remains bounded by ordinary run budgets. Failed calls
and denials are not progress. Permission checks, approval, revocation, sandbox,
leases, tool/step/request/tree/wall budgets and unknown-outcome recovery remain
binding. No aid increases them or authorizes another action.

## Explicit completion check

A host can opt a task into one concrete tool check, with at most three correction
rounds. For example, save this as `harness.json`:

```json
{
  "completion": {
    "tool": "process.run",
    "arguments": {"argv": ["python3", "-m", "unittest", "discover", "-s", "tests"]},
    "result_schema": {
      "type": "object",
      "properties": {"exit_code": {"const": 0}},
      "required": ["exit_code"]
    },
    "max_corrections": 1
  }
}
```

```sh
ww --werk-config examples/werk-config.json --harness-config harness.json run --profile coding-basic "Fix the requested regression"
```

This example assumes Python tests
are the task's explicitly chosen acceptance criterion and normal host policy
allows or can approve `process.run`. The harness invents no test requirement.
Without `completion`, final
answers take the ordinary completion path with no extra model/tool request.

A final model response is a candidate. After pending results, steering and child
synthesis, the harness records a complete assistant tool call and invocation,
then uses the ordinary executor. Profile membership, tool version, schemas,
policy, approvals, sandbox, leases, hashes and timeouts all apply. Each check
consumes a step and a tool invocation; any model correction also consumes the
ordinary model/tree request budget. Check success requires a successful,
untruncated ToolResult whose `data` satisfies `result_schema`. In particular,
process exit status should be checked explicitly. A process having run does
not by itself prove test success.

Acceptance and findings are persisted beside the invocation ID and candidate
fingerprint. Findings reference the canonical result and expected schema; they
may request a correction within `max_corrections` (default zero). Denial or an
unknown result blocks completion; it never grants rights. Limits produce normal
budget errors. `completion_failed` means the configured check did not pass in
the permitted rounds; `completion_blocked` means it could not proceed safely.
An unregistered tool is rejected at host setup; a tool absent from the run profile
is a blocker. Checks must be observational tools or sandboxed `process.run`.
Write/submission and job-control tools are rejected as repeatable acceptance
probes. Custom tools must truthfully declare their side effects. A sandboxed
process may still change workspace files under ordinary process permissions.

Restart uses the same completion gate. A saved `final_text` cannot skip a pending
check. Prepared checks resume through ordinary approval/execution recovery;
finished checks use their saved outcomes and do not run again. A running check
with no durable receipt becomes outcome_unknown and blocks automatic resume,
including external writes. Explicit host reconciliation is required as before.
No automatic rollback or claim of external exactly-once execution is introduced.

See [Werk capability decisions](reuse-matrix.md), [context limits](context.md),
[recovery](recovery.md), and the fixture measurements in
[the validation record](benchmarks/2026-10-21-harness/README.md) and
[the follow-up measurements](benchmarks/2026-10-21-harness-followup/README.md).
