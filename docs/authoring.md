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
| Files | files.read, files.list, files.find, files.search, files.read_image, files.write, files.edit, files.insert |
| Process | process.run, process.start, process.status, process.cancel, process.wait, process.log |
| Browser | browser.check (optional local runtime) |
| Broker | http.fetch; explicitly registered tickets.create / werk.* |
| Instructions | skills.list, skills.load |
| Interaction | interaction.ask, task.plan |
| Outputs | artifacts.read |

`files.list` inspects one directory. `files.find` recursively matches `*`, `**`,
and `?`; `files.search` accepts a file or directory, literal text or bounded
regex, include globs, case sensitivity and up to three context lines. Both
return `next_offset` pages (at most 100 rows); use identical query arguments
with that cursor. Search defaults to recursive; `recursive:false` keeps a flat
directory inspection. Dotfiles are opt-in; symlinks and common dependency/build
directories are excluded. Scans stop at 20,000 entries, depth 32, 32 MiB read,
or 10,000 result rows. `scan_limited` and `skipped_files` distinguish incomplete
searches from proven absence: narrow the path/include pattern. Binary, unreadable
and over-1-MiB files are skipped. Reads expose `content`, `start_line`, `end_line`, `total_lines`
and SHA-256. Large reads return complete text lines and `next_start_line` rather
than a broken JSON fragment; an oversized line uses UTF-8 byte pages with
`next_start_byte` (pass as `start_byte`, without line bounds).
Writes return compact path/hash/byte-count receipts; full diffs remain in approval
records. File preparation conflicts include the concrete reason so the agent can
distinguish a stale hash from a missing or ambiguous edit target. Writes are create-only
without expected_hash; edits require one exact nonempty match. `files.insert`
requires the current hash and `after_line` boundary (0 prepends); it inserts
literal content and returns `next_insert_after_line` when a line continuation is
possible. Newlines are not implicit. All file mutations invalidate verification.
See [protocol-informed sections](repair-state.md#protocol-informed-sections-and-file-tools)
for request-local size guidance and pagination. File operations
do not implement a multi-file transaction. Tools run serially in this release.

Process `cwd` accepts workspace-relative paths and the fixed `/workspace` sandbox
mount, normalized before approval. Other absolute paths and traversal remain
denied. `/workspace` persists between calls; each call gets a fresh `/tmp`.

`process.start` can mount the workspace read-only (`read_only:true`), allowing
other readers while it runs. Writers still wait for that lease. An owned running
job that blocks access produces `workspace_busy` with its job ID rather than
silently stalling the agent. `process.log` exposes live stdout/stderr with bounded
byte cursors and `dropped_before` for the 64-KiB tail per stream. `process.wait`
waits up to 30 seconds without further inference; it is cancellable and returns
a compact status, keeping logs in `process.log`. Completion
notices are delivered once at the next safe model decision boundary, after all
pending tool results. They do not interrupt a model generation. Jobs remain
run-owned and are cancelled during cleanup; they are not persistent terminals.

`files.read_image` normalizes local images to the same bounded image attachments
as user inputs. Text-only providers do not receive this tool. Tool images travel
as image blocks after the complete tool-result group, never base64 inside text.

`browser.check` performs a fresh local Chromium check: ordered clicks, keys,
fills, waits, visibility/text/JavaScript assertions, console/page errors and an
optional screenshot. A page load or screenshot alone cannot pass verification.
Declare `verification.criteria` and all relevant source/test `paths` to record
hash-bound evidence; the HTML entry is always included and later changes invalidate it. Checks prove the supplied
assertions, not every acceptance criterion automatically.

On Linux install the optional host runtime explicitly with
`sh scripts/install-browser.sh` (system Python with pip and Chromium runtime
libraries required). It installs pinned Playwright plus Chromium under
`$XDG_DATA_HOME/werk-worker/tools/browser`, or `WERK_WORKER_BROWSER_RUNTIME`.
Restart WW and use a new/updated coding profile to expose the tool. Discovery
never downloads software. Checks run inside bubblewrap with a read-only workspace,
no host home/network, a private local HTTP server, process/time limits and a
process-tree RSS watchdog. Browser launches are serialized within one WW process
to bound concurrent Chromium resource use; waiting remains cancellable. Browsers
use at most two host CPUs and a configurable `browser_processes` allowance
(default 512 threads above the current host UID baseline); normal commands keep
their separate 128-task allowance. `WERK_WORKER_BROWSER_DEBUG=1` adds Chromium
diagnostics to failed results. Chromium's large virtual address reservation does not
replace the RSS check. External CDNs and host dev servers are unavailable; use
local assets. This is local HTML verification, not a general persistent browser.
Screenshots become bounded model images when supported and artifact references
when an artifact store is configured. A screenshot can aid review but does not
establish functionality. See tests/agent_tools.rs; set REQUIRE_BROWSER=1 and
REQUIRE_SANDBOX=1 to require actual Chromium/bubblewrap tests.

Validation on 2026-10-24: 269 Rust tests passed, including real Chromium and
bubblewrap checks, with three opt-in saved-session replays excluded. Strict
Clippy and rustfmt passed. Tests use isolated fixtures, not live model inference.

These additions adapt bounded file discovery from
[DSH](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/fs/tool-fs-search/README.md)
and browser/background-log affordances from little-coder's
[browser](https://github.com/itayinbarr/little-coder/blob/main/.pi/extensions/browser/index.ts)
and [background shell](https://github.com/itayinbarr/little-coder/blob/main/.pi/extensions/bg-shell/index.ts).
They preserve WW's existing protocol-derived section budgets, grants and sandbox.

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
