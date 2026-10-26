# Changelog

## [Unreleased]

## [0.2.0] - 2026-10-26

- Persist validated rolling-context selections across turns and resumes; use
  local sizing to bound remote candidate counts while retaining complete-request
  admission under current Werk Protocol limits and reasoning settings.
- Detect ineffective compaction, stop unsuccessful repeated pruning, record
  planning cost/progress, and suppress duplicate TUI compaction notices when the
  working selection is merely reused.

- Add `/effort` and `/reasoning` chat controls with protocol-owned model tiers,
  effective defaults and availability. Session choices persist and apply to new
  parent/subagent requests while in-flight request settings remain stable. A small
  arrow-key slider offers only valid values above the input.
- Replan when steering arrives during context construction or before a model
  retry. Show separate queued/delivered notices and retain user corrections
  through compaction; task planning and verification must reflect updates.
- Preserve current coding file receipts after compaction, stop retaining stale
  directory listings as setup evidence, and explain existing-file edits without
  recommending unavailable write parameters. Repeated reads get bounded,
  persisted recovery decisions before a run-wide repetition failure.
- Deliver completed subagent reviews during parent work without waiting for
  running siblings or duplicating results at final synthesis.

- End runtime feedback turns explicitly for assistant-prefill backends, preventing
  output-limit notices from being returned as completed work. Empty/echoed replies
  now use bounded, resume-safe recovery instead of false completion.
- Keep generation recovery tied to the newest discarded attempt within the same
  bounded episode, including saved states that previously suppressed that notice.
- Evaluate rolling compaction cumulatively when an image-position change or
  retained evidence makes the first shortened candidate temporarily larger.
- Keep initial image turns ahead of adaptive context observations, allowing
  recovery from large failed writes without discarding their proposed content.
- Batch eligible history omissions before remote token counting, preserving full
  counting and admission for the selected request while reducing planning calls.
- Independent adaptive recovery for truncated tool receipts and discarded model
  generations, with bounded delivery and migration of legacy aid snapshots.
- Shorter bundled profile prompts and immutable scoped project context for new
  CLI sessions; missing rules defer tool proposals for a fresh model decision,
  while existing SDK embedded profiles and saved sessions retain their contract.
- F5 `/settings` editor for session profiles, access presets, individual tools
  and skills, with optional project defaults for new chats and separate
  host-owned workspace permissions. Session reconfiguration preserves history
  and rejects changes during active or interrupted work.
- TUI text selection with Ctrl-C/right-click copy, Ctrl-V/right-click paste,
  safe multiline bracketed paste, native terminal selection via F6, and
  independent mouse-wheel scrolling for conversation, request, sidebar,
  inspector and input panes.
- Compact chat layout by default, with inline magenta question/approval cards,
  keyboard-selectable answers, freeform replies and a persisted F4 `/view` menu
  switching between Compact and the existing Cockpit layout.
- Addressed approval/question cards, F2 permission inspection and
  clearer profile/access setup guidance; F3 opens selectable live subagent details.
- PTY regression for two child agents, addressed approvals/questions and retained
  tool results; slash commands remain available while an agent asks a question.
- Accept omitted empty capability operation lists from Werk during connection
  discovery, fixing the setup's `invalid_json` failure with Werk 1.9.1.
- Automatically load interactive connection defaults and optional local API keys
  from the selected workspace's `.werkworker/werk-config.json` and `secrets.json`.
- Interactive TUI server/authentication/model setup, masked key entry, persistent
  global settings, project save destination and `/setup` reconnection.
- Project instruction snapshots with werkWorker/AGENTS/CLAUDE priority, scoped
  directory chains, custom named profiles and separate tool/skill/test preferences.
- Session-only workspace access choices, display-only project permission requests,
  configurable TUI output reservation and clearer bare HTTP 401/403 errors.
- Real PTY setup regression in CI and documented configuration/security experiments.

- Parallel child agents with isolated conversations, scoped tools, nesting limits,
  shared request budgets, cancellation and durable parent/child records.
- Conservative inference admission from Werk deployment diagnostics or
  WerkProtocol observability, with a one-request fallback for unknown capacity.
- Operation leases for shared workspaces, TUI `/agents` and addressed child
  approvals/questions, SDK/RPC controls and `--execution-config`.
- Enforced provider context/output limits, automatic rolling compaction within
  long runs, bounded recovery from smaller server limits and nonretryable token truncation.
- Reproducible scheduler benchmark, raw results and subagent regression coverage.

- Initial async agent SDK, Werk adapter, durable sessions, scoped tools and Linux sandbox.
- Native platform packaging and installers for Linux, Windows and macOS.
- Interactive agent terminal with the werk top color palette and the short command `ww`.


[Unreleased]: https://github.com/philipbodenbach/werkWorker/compare/v0.2.0...HEAD
