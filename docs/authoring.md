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
| Browser | browser.check; browser.open, browser.observe, browser.navigate, browser.action, browser.close, browser.evaluate, browser.history, browser.download (optional runtime) |
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
`sh scripts/install-browser.sh chromium` (system Python with pip and Chromium runtime
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

## Persistent browser control

`browser.open`, `browser.observe`, `browser.navigate`, `browser.action`,
`browser.close`, `browser.evaluate`, `browser.history` and `browser.download`
control a separate headless browser, retaining tabs, cookies and form state
across tool calls. They are ordinary JSON tools shared by all
tool-capable model providers, including local Qwen, Llama, DeepSeek and GLM
models served by Werk. No Qwen-specific SDK or model-name allowlist is involved.
Text and accessibility snapshots work without vision; `browser.observe` can
add a screenshot for image-capable models. Model tool-calling quality still
depends on the selected model and server configuration.

Install with `sh scripts/install-browser.sh` for Chromium, Firefox and WebKit,
or name the engines to install. Playwright's platform libraries and OpenSSL
must be available. Existing Linux Chrome, Edge, Brave, Opera and Vivaldi are
supported through their standard executable locations; they are not downloaded
or selected from arbitrary model-provided paths. WebKit is the automation
engine, not an installed Safari app. Native macOS/Windows and existing personal
browser windows are not currently supported by WW's Linux isolation backend.

On Debian/Ubuntu, `sh scripts/install-browser.sh --local-deps` can download
missing platform libraries into the runtime's `deps` directory without sudo or
system package changes. It requires `python3-apt`, follows the pinned Playwright
dependency plan and extracts repository packages without executing maintainer
scripts. Other systems can use Playwright's documented platform dependency setup.

Enter **`/browser on`** in the TUI to enable browser mode for the current
session. `/browser` prints the panel URL; `/browser off` disables subsequent
agent actions and rejects unresolved browser approvals. The panel's Stop button
cancels the active run. Enabling this mode adds the browser tools to any profile;
workspace/process grants alone do not authorize them. Hard denies still apply.
SDK hosts use `SessionHandle::set_browser_mode`, `browser_panel` and
`set_browser_rule`; these are host APIs, never model tools.

The loopback panel provides website rules for browsing, consequential actions,
unlabelled coordinate clicks/dragging, downloads, session history and page
scripts. Each category supports Ask/Allow/Deny on an exact HTTP(S) origin or `*`
for defaults. All categories initially Ask. After the first page approval,
ordinary observations, scrolling, non-sensitive input and navigation inside the
approved scope proceed without repeated prompts. Form submission and other
buttons/keys ask by default. A site's Allow rule does not grant access to another
site. Session/project approval choices remember the displayed category and site;
the panel's rule editor changes session policy only. F5/project save can persist
those settings through the existing trusted configuration store.

Choose **Take over** to pause the agent between actions. Click the page image,
enter text in the panel, navigate or select tabs, then **Continue agent**. The
current action may finish before takeover; manual controls report busy until
then. Manual input goes directly to the isolated browser and is not added to
model messages or durable commands. Password, payment and OTP fields reject
normal model typing, have their values redacted from text/element snapshots and
are masked in screenshots. Detection uses field type, labels and autocomplete;
this is not a universal secret detector. Use manual entry for credentials, and
do not paste them into chat. Script approval is broader: an approved page script
can read page state, including field values.

`browser.evaluate` displays the exact page JavaScript and requires **Allow once**
every time, even with Full Access or remembered grants. Scripts can be disabled;
Allow for session is rejected. `browser.history` reads only this isolated
session's navigation history. `browser.download` saves an explicitly requested
URL as a bounded artifact (4 MiB maximum; requires an artifact store), using the
same scoped network broker. It does not execute files. Uploads are unavailable.

This workflow follows the browser-mode, site-permission and manual-handoff
approach documented in [Unsloth Browser Use](https://unsloth.ai/docs/basics/browser-use).
WerkWorker implements it in its own tool and host-permission system; it does not
require a Qwen or browser extension.

Example calls (the first result supplies the real `browser_id`):

```json
{"url":"https://example.com","browser":"firefox"}
{"browser_id":"browser_…","action":"click","target":{"role":"link","name":"More information"}}
{"browser_id":"browser_…","screenshot":true}
{"browser_id":"browser_…"}
```

These are arguments for `browser.open`, `browser.action`, `browser.observe`
and `browser.close`, respectively. Prefer a fresh `target.ref` from the returned
`controls` list. Other targets accept a CSS selector, exact accessible role/name,
or exact label, optionally within an iframe selector. References expire after
the next snapshot; after manual takeover, observe again before acting.
Actions also include fill, press, select, hover, scroll, back/forward/reload,
bounded wait, and new/select/close tab. The result includes stable tab IDs.

For visual tasks such as canvas painting, use `click_at`/`move` with pixel `x,y`
or `drag` with a `path` of 2–64 points. The viewport is 1024×768; resized model
screenshots report `scale_x/scale_y` to translate image pixels to that viewport.
For example, `{"browser_id":"browser_…","action":"drag","path":[{"x":40,"y":320},{"x":80,"y":360},{"x":120,"y":320}],"screenshot":true}`
draws one reviewed mouse stroke. Every action updates the local panel at
`viewer_url`, including the last mouse position. The panel is bound to loopback
and protected by an unguessable path. Host commands additionally require a
same-origin JSON POST and a custom header. The agent's network broker rejects
all panel ports even if loopback access was approved. Direct tool integrations
without a host panel retain a read-only preview. Updates are snapshots after
actions, not continuous video. Engine shutdown/session deletion closes the panel.

Opening approves exact `allowed_origins` (scheme, host and port); the initial
URL origin is included automatically. CDN, login and redirect origins must be
listed explicitly. Host/loopback services additionally need `private_origins`,
which must be a subset of those approved origins. For example, local development
can use `{"url":"http://127.0.0.1:3000","private_origins":["http://127.0.0.1:3000"]}`.
`browser.navigate` and `browser.download` can request another public origin;
preparation exposes the expanded scope for approval before any I/O. Adding a
private-origin exception still requires closing and opening a new browser.

All control tools are external writes: loading or observing a page can run
scripts that change remote data. Browser mode applies the category/site rules;
without explicit browser mode the existing per-tool approval policy is retained. Prepared approvals show the browser,
origin scope, current URL, revision and exact action. A changed session revision
invalidates old approvals. Page contents cannot grant access. Timeout/cancellation
closes the affected browser and reports uncertain remote effects; never blindly
repeat a submission. Closing a browser does not undo remote changes.

The browser has an empty workspace/home and no host network. A private proxy
forwards only approved HTTP(S) requests through Rust's bounded, DNS-pinned broker,
checking every redirect and validating upstream HTTPS certificates. Cookies stay
in the isolated browser. No personal profile, inherited proxy, file upload or
arbitrary host command is exposed. Reviewed JavaScript runs only in the page;
reviewed downloads stay in the owned artifact store.
WebSockets, service workers and streaming requests are blocked. Background
networking outside an approved action is disabled; some live websites require
these unsupported features. A page snapshot is observation, not behavioral
verification for coding completion; use `browser.check` for that purpose.

At most four browsers per registry and twelve tabs per browser are allowed.
Actions have a 50-second bound; requests/bodies and process resources are bounded.
Persistent browsers have separate host-configurable
`browser_session_memory_bytes` (2 GiB summed process-tree RSS by default) and
`browser_session_cpu_seconds` (300 CPU seconds per process) limits; normal
commands and one-shot `browser.check` keep their existing limits. Browser
processes use at most two CPUs and 1024 descriptors per process.
Browser sessions expire after 15 minutes and are not persisted on engine restart.
Close idle browsers explicitly; engine shutdown and conversation deletion also
clean them up. To require real cross-engine tests:

```sh
REQUIRE_BROWSER=1 WW_BROWSER_ENGINES=chromium,firefox,webkit cargo test --test browser_control -- --test-threads=1
```

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
