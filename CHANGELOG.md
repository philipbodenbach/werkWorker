# Changelog

## [Unreleased]

## [0.3.3] - 2026-11-10

### Changes

- fix(): pipeline release (`bb16c1b76801`)

## [0.3.2] - 2026-11-10

### Changes

- fix(): pipeline release (`85b44e947a95`)
- fix(): pipeline release (`06039d330573`)
- fix(): pipeline release (`8f4abb594502`)
- fix(): pipeline release (`ced76dc1fd24`)

## [0.3.1] - 2026-11-09

### Changes

- fix(): pipeline release (`f899d38cac57`)
- fix(): pipeline release (`c322b56c8a9a`)

## [0.3.0] - 2026-11-07

### Changes

- feat(): subagent settings (`6018c67c8b38`)
- feat(): subagent settings (`5f69e135680d`)
- feat(): subagent settings (`4aad968ea0da`)
- feat(): subagent settings (`3f8de6d671e6`)
- feat(): subagent settings (`a6b52d8a370f`)
- fix(): backend installer (`886027000ec9`)
- feat(): subagent settings (`a285746cf498`)
- feat(): subagent settings (`9c3105a8af85`)
- feat(): subagent settings (`12cc332720ef`)
- feat(): subagent settings (`99094d55e7de`)
- feat(): subagent settings (`eec9787aff4b`)
- feat(): subagent settings (`6aa5a6f44d47`)
- feat(): subagent settings (`1404ab229c15`)
- feat(): subagent settings (`99c69c449b0e`)
- feat(): subagent settings (`cbf73a7f6d75`)
- feat(): subagent settings (`d8c7e0ec9c50`)
- feat(): browser agent (`186463bc5f04`)
- feat(): browser agent (`bbb515b5f40c`)
- feat(): browser agent (`3ad6b0a9bc30`)
- feat(): browser agent (`679002b9c6c4`)
- feat(): browser agent (`17baa6d9419c`)
- feat(): expand cli (`1f0a0c4dd3f5`)
- feat(): expand cli (`a86e97957d54`)
- feat(): expand cli (`13adb9387898`)
- feat(): expand cli (`4ef56b7f35a1`)
- feat(): expand cli (`1c4f5d78a6f4`)
- feat(): expand cli (`478fef38e1ed`)
- feat(): expand cli (`faefa4ca6fcd`)
- feat(): expand cli (`59fc059bd9ce`)
- feat(): expand cli (`352190a0103f`)
- feat(): expand cli (`409c15329998`)
- feat(): expand cli (`84b0f3417734`)
- feat(): expand cli (`a809051f4c6e`)
- feat(): expand cli (`ddc5f5bd9eac`)
- feat(): expand cli (`4fe310485770`)
- feat(): expand cli (`33ac129deccf`)
- feat(): expand cli (`3c8794af63f3`)
- feat(): expand cli (`2971037f4a39`)
- feat(): expand cli (`02ff56f3e0d9`)
- feat(): expand cli (`5c46e93ff66f`)
- feat(): expand cli (`d2dc974f7ca7`)
- feat(): add jspaint experiment (`883502b09221`)
- feat(): add jspaint experiment (`8706cce376bb`)
- feat(): expand cli (`4494141dead3`)
- fix(): pipeline release (`e155be441ab5`)
- fix(): pipeline release (`1f5fb88b4a1b`)

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



[0.3.0]: https://github.com/philipbodenbach/werkWorker/compare/v0.2.0...v0.3.0

[0.3.1]: https://github.com/philipbodenbach/werkWorker/compare/v0.3.0...v0.3.1

[0.3.2]: https://github.com/philipbodenbach/werkWorker/compare/v0.3.1...v0.3.2

[Unreleased]: https://github.com/philipbodenbach/werkWorker/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/philipbodenbach/werkWorker/compare/v0.3.2...v0.3.3
