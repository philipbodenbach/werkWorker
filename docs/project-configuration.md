# Project configuration, rules and profiles

werkWorker separates connection settings, agent behavior and host permissions.

F5 edits all three access groups (workspace reads, workspace writes, sandbox
processes) separately from profile/tool/skill selection. Each group supports
deny, ask and allow. Ctrl-S saves the session and, by default, project defaults;
P selects session-only changes. `/permissions writes allow project` and
`/permissions mode manual session` are equivalent host commands in the chat.
They remain available while a model question is pending; the question still
requires its own answer. Applying settings during a prepared action approval
cannot change workspace writability, because that action's sandbox has already
been prepared. Resolve the action first in that case.

The editor opens on Permissions. Clicking a tab opens it; clicking an option
changes it. **Apply & close** (F10) saves the same draft as Ctrl-S and returns to
the chat. A text selection in the settings details does not consume the save
shortcut. Esc warns about unsaved edits; a second Esc discards them. Selecting
a profile retains tools explicitly enabled through permission rules.

Remembered approvals are scoped: individual file tool within the workspace, or
exact prepared arguments for non-file actions. A project approval is saved in
the host's `workspace-access` configuration and reloaded for new project chats.
Hard denials, invalid paths and read-only workers still take precedence.
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
SDK/RPC sessions remain host-controlled. For new scoped sessions, SDK callers use
`Project::load`, then pair `Project::scoped_profile` with `Project::scoped_metadata`
in `create_session`. The existing `Project::profile` / `Project::metadata` methods
keep their embedded-prompt semantics for existing callers. No new public struct
fields or provider methods are required.

In each selected directory, **only the first existing rule file** is loaded:
`.werkworker.md` → `WERKWORKER.md` → `AGENTS.override.md` → `AGENTS.md` → `CLAUDE.md`.
An empty higher-priority file intentionally suppresses the lower-priority files.
An override replaces its sibling AGENTS file. Malformed, oversized or unsafe
files produce an error; they are not silently substituted with a different rule.

The root is always selected. `instruction_directories` explicitly adds directory
chains beneath the workspace, with parents before children and duplicates removed,
including aliases such as `./src` and `src/./api`.
For example `src/api` loads root, `src`, then `src/api`. More specific rules apply
only to their subtree. This is bounded startup discovery, **not recursive scanning
or progressive discovery during arbitrary tool calls**. List the subtrees needed
for the task. At most 32 directories, 16 KiB per file and 32 KiB of rule text total
are allowed. Paths cannot escape the workspace; symlinked metadata is rejected.

Rules, source paths, directory scopes, priority order and SHA-256 hashes are
snapshotted into the session. New CLI sessions keep the original profile prompt
separate from these full sources. The initial context contains complete root
rules and an index of all configured sources, not every subtree's full text.
Within each scope the filename priority above selects one source; deeper scopes
override parents. Host policy and direct user requirements remain authoritative.
There is no keyword classification, summarization or truncation of mandatory
rules. A read explicitly requested by the user remains an ordinary file read.

When a tool proposal first touches a scope whose rules were not in its actual
model request, the engine loads all applicable snapshots and settles the entire
proposed batch with `CancelledBeforeStart` / `project_instructions_required`.
No preparation, approval or execution of that proposal takes place. The next
model request contains the complete rules; the model must reconsider and issue
new calls. The original call IDs keep matching non-execution receipts. These
proposals consume the normal tool-call allowance; the extra decision consumes
normal step/model/tree/time budgets. A crash cannot replay a deferred write.
Configured completion probes similarly require the rules before the model's
candidate is accepted for the check; loading rules does not spend a completion
correction or execute a probe.

File reads/writes/edits use the validated workspace-relative target (including
the `/workspace/` alias). Directory listings/searches inspect only direct entries,
so they require the target directory's rules and its ancestors, without preloading
untouched child-directory rules. Processes, delegation and tools without a reliable file-path contract
conservatively require all configured workspace rules; shell strings and cwd do
not establish a narrower effects boundary. Skills/artifact lookup, interaction
and task planning need only root rules. This supplies information, never grants
host rights: normal policy, approval, lease and sandbox checks remain required
for a reconsidered action.

The version-1 `_werkworker_project_context` metadata records loaded source paths
and delivery by model step; `project_instructions` stores each complete source
once. Priority is its deterministic parent-before-child vector position. The
admitted request must contain the exact source message to count as delivery,
including for custom context builders. Already needed rules stay loaded through
the session, including while processes or child work may remain open. Scope
changes therefore add applicable sources without dropping still-needed rules;
long sessions may eventually need all configured rules. Each source appears once
in the generated rule context. Compaction and resume reconstruct it from the
immutable snapshot, not from a lossy history summary. Children and forks inherit
sources and loaded scopes, but establish their own request delivery receipts.

Subsequent instruction edits take effect in a new session; opening a session or
choosing a profile in its settings does not import changed rule files. Old
snapshots without the separate-context marker retain their full embedded prompt.
No text delimiter parsing or silent migration of historical profile text occurs.
Too much indispensable context causes `context_overflow` rather than silent loss
of rules. Missing optional rule/skill files remain normal; unsafe or corrupt
files still fail through the existing secure loader.

## Project JSON

Copy [the example](../examples/project-config.json) to `.werkworker/config.json`:

```json
{
  "version": 1,
  "model": "qwen3.8-27b-ud-q4_k_xl",
  "profile": "coding-basic",
  "context_tokens": 32768,
  "output_tokens": 4096,
  "reasoning_tier": "low",
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

`delegation_level` fixes delegation to `off`, `conservative`, `active` or `max`.
It takes precedence over the F5 → Agents preference. Omission or `null` uses
that preference, defaulting to **Active**. F5 never overwrites an explicit
project value; saved UI preferences are stored separately per workspace.
Every level remains subject to host agent/depth limits, verified inference
capacity, budgets and permissions. Existing sessions retain their project
snapshot; a new session loads manual configuration changes. See
[delegation levels and automatic helper triggers](subagents.md).

`subagent_model` optionally selects one model for **all** delegated children,
including automatic reviews and nested helpers. For example, add
`"subagent_model": "your-helper-model"` alongside the existing `model` field.
Omission or `null` preserves the default: children use the parent's model.
This does not enable additional delegation or change review triggers, budgets,
tools, permissions or depth limits. The main session keeps its selected model.

The child uses the same Werk origin, authentication and transport settings.
WerkWorker discovers its own context/output limits and capacity, and uses the
child model's server reasoning default rather than a different model's effort
setting or deployment pin. Other explicit connection capability overrides and
sampling options remain inherited. A missing model, unknown child context limit,
or provider without model selection produces a delegation error; it does not
silently fall back to the main model. Preload both models with `werk serve` when
desired; Worker does not launch another server.

The option applies to new CLI `tui` and `run` sessions through their project
snapshot. Existing sessions retain their saved choice; editing the project file
does not reroute them. Project settings saves preserve this field. SDK hosts pass
`Project::metadata()` or `Project::scoped_metadata()` to `create_session`.

`reasoning_tier` is an optional tier ID reported by WerkProtocol. Omission inherits
the connection's value; explicit `null` or `"default"` selects the server's current
default, even when the global connection specifies a tier or native effort. The
wizard saves this preference along with model and budgets. `reasoning_effort`
remains available for native string/number overrides; configure only one field.
See [Reasoning effort in setup](setup.md) for discovery and limit handling.

Interactive precedence: built-in defaults → global connection → local
`.werkworker/werk-config.json` → project
model/profile/reasoning/budgets → explicit `--werk-config` transport values and a non-default
CLI `--profile` → choices made in the wizard. Project model/context/output/reasoning values
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

The TUI can save model/profile/reasoning/budgets back to this file while retaining lists,
commands and other preferences. It checks the original file hash to reject
concurrent edits. Saving project settings is a direct host UI operation, separate
from the agent's workspace grants.

## Named profiles

The three built-in profiles remain `general-readonly`, `coding-basic` and
`automation-basic`. Their base prompts state the task, preservation of user
work where relevant, appropriate verification and truthful reporting. They do
not mandate delegation or a browser/canvas/startup test catalogue on every
request. Relevant available skills can be loaded optionally; task-specific
requirements and custom/host prompts are not heuristically rewritten.
Add a distinct profile, for example
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
The profile prompt is limited to 64 KiB; legacy embedded profiles include their
instruction text in that limit. Separate snapshots retain the 32 KiB total
instruction limit above, and the actual combined request must pass admission.

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
