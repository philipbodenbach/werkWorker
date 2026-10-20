# Tools, skills and profiles

Implement `Tool: Send + Sync`: `definition`, optional `prepare`, and async
`execute`. Register it explicitly with `ToolRegistry::register(Arc<dyn Tool>)`
before building the engine. Definitions declare version, description, JSON input
schema, effect, timeout, output limit and concurrency safety. External schema
references are rejected; validation cannot fetch remote URLs. Duplicate names
fail. All tools use the same executor, including registered connectors.

`prepare` is side-effect-free except scoped reads and returns normalized JSON.
For mutations it must include a concrete reviewable base/diff or fixed origin.
The hash of this value binds approval. `execute` receives only `ToolContext`:
session/run/invocation identity, pinned workspace, cancellation, sandbox,
run-owned jobs, owned artifacts and allowed skill snapshots. No global cwd or
process environment is changed. Never put tokens in arguments/definitions.

Effects determine gates: internal, read, write, process, network_read,
external_write. External writes on timeout/cancellation must report uncertainty,
not pretend rollback. Follow the built-in `TicketConnector`, which supplies a
fixed host origin and secret outside the model schema. Its test service is
explicit test infrastructure. It is not Jira Cloud/Data Center support.

`WerkNativeTool::discover` requires a host allowlist, real `/v1/tools` schemas
and installed task readiness. Runtime capability statuses do not create grants;
experimental operations require host opt-in. Namespaced calls use
`/v1/tools/call`, preserve invocation correlation and native receipts. Job
inspection/cancellation requires session-owned job IDs. `WerkClient::job/output`
are low-level trusted-host helpers for bounded native job/output transport.
Returned native jobs retain their own identity and lifecycle; they are not
Worker runs. The host must explicitly monitor/cancel retained external jobs.

Builtin inventory:

| Family | Tools |
|---|---|
| Files | files.read, files.list, files.search, files.write, files.edit |
| Process | process.run, process.start, process.status, process.cancel |
| Broker | http.fetch; explicitly registered tickets.create / werk.* |
| Instructions | skills.list, skills.load |
| Interaction | interaction.ask, task.plan |
| Outputs | artifacts.read |

Listings/search are bounded, nonrecursive and exclude dotfiles/symlinks. Search
uses a portable literal-text fallback, at most 500 matches. Binary/large files
are skipped; reads expose `content`, `start_line`, `end_line`, `total_lines`
and SHA-256. Large reads return complete text lines and `next_start_line` rather
than a broken JSON fragment; a single oversized line uses an excerpt/artifact.
Writes return compact path/hash/byte-count receipts; full diffs remain in approval
records. File preparation conflicts include the concrete reason so the agent can
distinguish a stale hash from a missing or ambiguous edit target. Writes are create-only
without expected_hash; edits require one exact nonempty match. File operations
do not implement a multi-file transaction. Tools run serially in this release.

Process `cwd` accepts workspace-relative paths and the fixed `/workspace` sandbox
mount, normalized before approval. Other absolute paths and traversal remain
denied. `/workspace` persists between calls; each call gets a fresh `/tmp`.

Skills follow [Agent Skills](https://agentskills.io/specification): a directory
name matching YAML `name`, a nonempty bounded `description`, Markdown body and
optional resources. `SkillRegistry::load_directory` accepts only an explicitly
host-selected directory and uses safe file access. Conflicting names fail.
`allowed-tools` never grants authority. Scripts are not started on discovery
or load. Additional resources must be explicitly made available as read-only
workspace data and use normal granted tools; no ambient home scan occurs.

Profiles are version-1 TOML parsed by `AgentProfile::parse`. They contain name,
prompt, tools and skill names, never credentials or permissions. Bundled
general-readonly/coding-basic/automation-basic profiles are embedded with
`include_str!`. Host builder and explicit CLI flags choose effective resources;
workspace configuration is not implicitly loaded. The exported session shows
the effective profile hash, host policy and immutable skill snapshots.
