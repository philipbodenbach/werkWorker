# Project configuration, rules and profiles

werkWorker separates connection settings, agent behavior and host permissions.
The design takes inspiration from [Hermes context files](https://hermes-agent.nousresearch.com/docs/user-guide/features/context-files/)
and [profile configuration](https://hermes-agent.nousresearch.com/docs/user-guide/configuration/);
the filenames and behavior below are werkWorker's own contract.

| File | Purpose |
|---|---|
| `~/.config/werkworker/config.json` | Global connection, authentication reference, model and budgets; see platform paths in [setup](setup.md) |
| `.werkworker/config.json` in `--workspace` | Project model/profile, tool/skill selection, suggested test commands, additional rule directories |
| `.werkworker/werk-config.json` in `--workspace` | Automatically loaded connection defaults for interactive setup |
| `.werkworker/secrets.json` in `--workspace` | Optional local API key for that connection; keep private and gitignored |
| `.werkworker.md` or `WERKWORKER.md` | werkWorker-specific instructions |
| `AGENTS.override.md` | Personal replacement for the directory's shared AGENTS rules; normally gitignored |
| `AGENTS.md` | Portable project conventions, architecture and build/test guidance |
| `CLAUDE.md` | Compatible fallback instructions |
| `<global config directory>/profiles/<name>.toml` | User-defined agent prompt, tools and skills |
| `.werkworker/profiles/<name>.toml` | Project-specific named agent profile |

## Resolution and scope

New CLI `tui` and `run` sessions automatically read project preferences and root
instructions from the selected `--workspace` (default current directory). They
never walk above that boundary into `$HOME`, `/tmp` or an enclosing repository.
SDK/RPC sessions remain host-controlled: callers can explicitly use `Project::load`
and `Project::profile` and pass the resulting profile/metadata to `create_session`.

In each selected directory, **only the first existing rule file** is loaded:
`.werkworker.md` → `WERKWORKER.md` → `AGENTS.override.md` → `AGENTS.md` → `CLAUDE.md`.
An empty higher-priority file intentionally suppresses the lower-priority files.
An override replaces its sibling AGENTS file. Malformed, oversized or unsafe
files produce an error; they are not silently substituted with a different rule.

The root is always selected. `instruction_directories` explicitly adds directory
chains beneath the workspace, with parents before children and duplicates removed.
For example `src/api` loads root, `src`, then `src/api`. More specific rules apply
only to their subtree. This is bounded startup discovery, **not recursive scanning
or progressive discovery during arbitrary tool calls**. List the subtrees needed
for the task. At most 32 directories, 16 KiB per file and 32 KiB of rule text total
are allowed. Paths cannot escape the workspace; symlinked metadata is rejected.

Rules, source paths, directory scopes and SHA-256 hashes are snapshotted into the
session. Their content is added to the agent prompt and stays subject to host
policy and the user's task. Subsequent edits take effect in a new session, for
example after `/setup`. Subagents inherit the parent's profile and scoped rules.
Compaction keeps the profile constraints; too much fixed context causes a bounded
`context_overflow` rather than silent loss of instructions.

## Project JSON

Copy [the example](../examples/project-config.json) to `.werkworker/config.json`:

```json
{
  "version": 1,
  "model": "qwen3.8-27b-ud-q4_k_xl",
  "profile": "coding-basic",
  "context_tokens": 32768,
  "output_tokens": 4096,
  "skills": ["task-execution", "coding-basic"],
  "test_commands": {
    "check": ["cargo", "check", "--all-targets"],
    "test": ["cargo", "test", "--all-targets"]
  },
  "instruction_directories": ["src"],
  "requested_access": "process"
}
```

All fields except `version` are optional; version defaults to 1. Omitted `tools`
and `skills` inherit the selected profile. Supplying an array replaces that
profile's list, including an empty array. Names must refer to registered built-in
tools and bundled skills; arbitrary scripts/plugins are not loaded automatically.
A test command is an argv array included as a **suggestion** in the prompt. Merely
opening the project never executes it. There are no startup hooks.

Interactive precedence: built-in defaults → global connection → local
`.werkworker/werk-config.json` → project
model/profile/budgets → explicit `--werk-config` transport values and a non-default
CLI `--profile` → choices made in the wizard. Project model/context/output values
are TUI defaults; headless `run` still uses its explicitly provided WerkConfig and
default run limits. Both paths apply project rules, tool/skill selection and
profile defaults. Recovery retains the original session's profile snapshot.

Project preference and rule files cannot set the server URL, credentials, environment references,
external mounts, network access, executable paths or host grants. Unknown fields
are rejected. `requested_access` is only a displayed request (`none`, `read`,
`write`, `process`, `ask`); it never changes permissions. Select access in the TUI or
supply a host-owned `--grants` file. Process access still uses the existing sandbox
without new mounts or network rights. Each action requires its normal approval.

The separate connection file uses the same WerkConfig format as `--werk-config`.
It only seeds interactive setup; the wizard still requires the connection step.
An explicit `--werk-config` bypasses automatic connection and secret discovery.
Local credentials use `{"api_key":"your-token"}` in `.werkworker/secrets.json`;
`--secret-env` overrides them. Credentials are masked in the wizard and excluded
from project metadata, saved settings and session exports. A local URL change
clears inherited global authentication. Files are read only in the selected
workspace, with bounded sizes and no symlinks; parent directories are not searched.
Headless and explicit offline paths do not automatically read these files.

The TUI can save model/profile/budgets back to this file while retaining lists,
commands and other preferences. It checks the original file hash to reject
concurrent edits. Saving project settings is a direct host UI operation, separate
from the agent's workspace grants.

## Named profiles

The three built-in profiles remain `general-readonly`, `coding-basic` and
`automation-basic`. Add a distinct profile, for example
`~/.config/werkworker/profiles/reviewer.toml`:

```toml
schema_version = 1
name = "reviewer"
prompt = "Review the requested change. Cite files and distinguish evidence from assumptions."
tools = ["files.read", "files.list", "files.search", "skills.list", "skills.load", "interaction.ask"]
skills = ["document-analysis"]
```

Custom names appear in the TUI profile selector. A project profile with the same
custom name takes precedence over the global one. Built-in profile names cannot
be shadowed by files. Selecting a profile does not grant any of its tools access
to host resources. Project `tools`/`skills` preferences apply after profile loading.
The combined profile prompt and instruction text is limited to 64 KiB.

Global connection setup is available on all packaged platforms. Project metadata
uses the secure workspace reader/writer: currently Linux `openat2` is required
when such files exist. macOS/Windows fail closed for this feature, like the
existing native file/process tools; they do not fall back to insecure reads.
Use Linux/WSL2 for the full project workflow until native filesystem isolation is
implemented. See [the platform matrix](security.md).

## Validation

[Recorded setup and project experiments](benchmarks/2026-10-19-setup/README.md)
cover real terminal interaction with an authenticated fixture, configuration
persistence, rule priorities, snapshots and permission boundaries. They are not
a Qwen quality or throughput benchmark.
