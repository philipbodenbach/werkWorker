# Changelog

## [Unreleased]

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
