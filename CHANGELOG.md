# Changelog

## [Unreleased]

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
