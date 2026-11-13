![WerkWorker](docs/assets/werkworker-banner.svg)

[Project website](https://philipbodenbach.github.io/werkWorker/) ·
[Projektseite auf Deutsch](https://philipbodenbach.github.io/werkWorker/de/) ·
[Tetris experiment](https://philipbodenbach.github.io/werkWorker/experiments/tetris/)

werkWorker combines a local, general-purpose Rust agent engine, an adaptive
harness and a standalone CLI/TUI in one package.
The default `ww` command opens an interactive terminal in the werk top palette.
The package is `werk-worker`, the Rust import is `werk_worker`, and the binary is
`werk-worker`, with `ww` as its short command. Both the release installers and
Cargo install these two commands, which expose the same CLI. It is licensed
under [Elastic License 2.0](LICENSE).

The SDK runs an asynchronous model/tool loop with streamed events, explicit
resource grants and approvals, SQLite sessions, cancellation, steering,
follow-ups, crash recovery and parallel subagents with separate conversations.
Coding, document and ticket automation, and browser control use this same
engine. Five bundled Agent Skills and three profiles are embedded in the
package; they do not require a repository checkout at runtime.

werk1112 supplies inference and model/runtime management. werkWorker owns agent
execution, tools, permissions and sessions. Its existing `WerkProvider` connects
to werk1112 over HTTP. Applications can embed the [Rust SDK](docs/sdk.md) through
`werk_worker` or control a separate worker process through the
[JSONL protocol](docs/protocol.md).

werkStation is planned as a wrapper suite that imports werkWorker and werk1112
as its agent and inference/runtime components. The suite would bring both
together behind a shared application interface, while the agent engine and
harness remain in werkWorker and inference remains in werk1112. This suite
integration is planned; werkWorker already provides its own CLI/TUI and SDK.
See [component boundaries and the integration plan](docs/architecture.md#components-and-planned-werkstation-integration).

There is no GPU runtime, Jira implementation or autonomous package installer
here. Scripted tests measure the harness, not model intelligence.

## Install and try

Install a prebuilt package from the [latest published GitHub release](https://github.com/philipbodenbach/werkWorker/releases/latest).
The installers select the matching platform archive, verify its SHA256 checksum
and install both `ww` and `werk-worker`. No Rust toolchain or repository checkout
is needed. Supported packages target Linux x86_64, Windows x86_64 and macOS
Apple Silicon. This checkout adds native workspace access and constrained
process backends; published archives contain the capabilities of their tagged
source. See the [platform requirements and validation status](docs/cross-platform-sandbox.md).

Linux or macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/philipbodenbach/werkWorker/main/scripts/install.sh -o /tmp/werkWorker-install.sh &&
sh /tmp/werkWorker-install.sh
```

Windows PowerShell:

```powershell
$env:WERK_WORKER_ADD_TO_PATH = '1'
irm https://raw.githubusercontent.com/philipbodenbach/werkWorker/main/scripts/install.ps1 | iex
```

On Linux/macOS, ensure `~/.local/bin` is on `PATH`. The Windows command adds the
installation directory to your user `PATH`; open a new terminal afterward.
The default download is the latest published release; drafts are not selected.
That release must contain the platform archive and its `.sha256` file. See
[installer options and the build/upload workflow](docs/releases.md#installers)
for a specific version or a custom installation directory.

Alternatively, build from this checkout with Rust 1.88 or newer:

```sh
cargo install --path . --locked
```

Try the installed commands:

```sh
ww tui --demo "Hello"
ww --help
ww profiles list
ww skills inspect ticket-drafting
ww sandbox doctor
```

The demo is explicitly offline. A missing model connection never falls back to
scripted replies. Linux process tools require user namespaces and Bubblewrap.
Windows process tools run in single-process mode and require an explicitly
provisioned local NTFS workspace;
macOS has an opt-in Seatbelt single-process mode that requires explicit acceptance
of path-based workspaces and an authenticated system task-access port exception.
macOS currently supports fresh live sessions; restoring sessions and saving
workspace permissions requires a durable identity that this backend cannot yet provide.
`sandbox doctor` reports file, process and browser capabilities separately.
See [the sandbox matrix](docs/security.md) and
[native setup and limitations](docs/cross-platform-sandbox.md). Missing isolation
fails closed; native browser isolation remains unavailable.

With no arguments, `ww` opens an interactive setup: choose a Werk server, enter
its API key in a masked field, discover models, and select token budgets, reasoning effort,
profile and workspace access. Settings can be saved globally in
`~/.config/werkworker/config.json` or as project preferences in
`.werkworker/config.json`. Keys are session-only; reusable credentials use an
environment-variable reference. `/setup` reconfigures after a run finishes.
On subsequent starts, **Continue previous** reconnects to the remembered service;
**Connect new** opens setup. Choose **New session** or a previous conversation
to restore its history and continue. `/sessions` opens this list from chat.
See [setup and authentication](docs/setup.md) and [terminal controls](docs/tui.md).

Inspect temporary files and reclaim build/cache space without connecting to a model:

```sh
ww tmp list
ww tmp purge --all --dry-run
ww tmp purge --all
ww cache list
ww cache purge --all --kind build --dry-run
ww cache purge --all --kind build
```

`ww tmp` also accepts `temp`. Temporary cleanup defaults to entries unchanged for
24 hours. Cache cleanup uses the selected `--workspace` for Rust build directories;
backups and shared package caches have separate selectors. See
[cleanup scopes and examples](docs/cleanup.md) before purging backups or shared caches.

Project rules use `.werkworker.md` / `WERKWORKER.md`, with `AGENTS.override.md`,
`AGENTS.md` and `CLAUDE.md` as ordered alternatives. Named profiles, tool/skill
selection and suggested test commands remain separate from permissions.
See [project configuration, priority and scope](docs/project-configuration.md).
`ww run ...` remains available for headless use and `ww rpc` for JSONL clients.

Use `ww --help` for all commands. During development, use
`cargo run --bin ww -- run --demo "Hello"` without installing. The longer
`werk-worker` command remains interchangeable with `ww`.

For a real Werk server, copy [the connection example](examples/werk-config.json),
set its model and only capabilities you have confirmed for that model, then:

```sh
werk-worker --werk-config examples/werk-config.json \
  --grants examples/readonly-grants.json --workspace /path/to/approved/workspace \
  tui --profile general-readonly "Summarize the supplied notes with citations"
```

`--secret-env WERK_API_KEY` resolves an optional credential in the trusted host.
Do not put it in a profile or workspace. Plain HTTP is accepted only for literal
loopback addresses; remote Werk origins require HTTPS. See
[the Werk reuse matrix](docs/reuse-matrix.md).

For a running local Qwen 27B server, see the [copyable agent example with hidden
API-key entry](docs/local-werk-example.md) and its
[connection configuration](examples/werk-qwen27b.json).

SDK/headless default policy is **ask**, with **no external resource grants**.
New TUI setups offer all registered tools/skills with explicit approval for each
file/process/network action. F2 → Enter enables this mode in an existing restricted
chat without reconnecting. Type `ja`/`yes` on an action approval to allow it once.
Project files never grant access. The example
grant file explicitly allocates workspace reads; each read still asks. A grant
allows the engine to consider an action; an approval authorizes its prepared
arguments. Full access and exact-argument reusable approvals require explicit
host configuration and cannot expand the sandbox or override a hard denial.

F5 → **Permissions** edits workspace reads, workspace writes and sandbox processes
individually: **deny / ask / allow**. Enabling access also enables the matching
tools in the selected profile. Presets include **Manual** (allow reads, ask for
edits/commands), **Accept edits** (allow reads/edits, ask for commands) and
**Read only**. In the Tools tab, Enter toggles availability and A cycles permission.
Ctrl-S applies changes; P chooses current session only or session + project defaults.
Settings can also be changed while the agent is waiting for an answer.
F5 opens Permissions directly. Tabs and options accept a single click; the
visible **Apply & close** button or F10 saves and returns to the conversation.
Unsaved edits are marked explicitly, and Esc warns before discarding them.
Explicitly enabled tools remain enabled when a different profile is selected.

**`/browser on`** enables browser control for the current session; `/browser`
prints the local panel link. Follow page snapshots, approve actions, set website
permissions, or choose **Take over** for manual login and then continue the
agent. Page scripts require approval every time. Standard tools cover navigation,
page text, forms, tabs, screenshots, canvas drawing, session history and bounded
download artifacts. `/browser perception auto|dom|vision` selects model feedback;
Browser sessions stay open until explicitly closed by default; `/browser limits`
configures optional time limits and the session action budget. The bundled browser-use
skill guides observation, action and result verification. No Qwen-specific browser extension is required.
Install the isolated runtime with `sh scripts/install-browser.sh` for Chromium,
Firefox and WebKit. Linux/WSL and the browser platform libraries are required;
installed Chrome/Edge and common Chromium variants can also be selected. These
tools work with any supported tool-capable model. See
[browser setup, permissions and limits](docs/authoring.md#persistent-browser-control).

F5 → **Agents** selects delegation: **Off / Conservative / Active / Max**.
**Active** is the default. An explicit `delegation_level` in
`.werkworker/config.json` overrides the UI preference. Host capacity, budgets
and permissions remain authoritative. See [subagents](docs/subagents.md).

Host chat commands work during questions without consuming the pending answer:

```text
/permissions writes allow project
/permissions process ask session
/permissions mode manual project
/permissions mode edits project
/permissions mode readonly session
```

Omitting the final scope saves both the session and project default. Concrete
approval prompts offer **Allow once**, **Deny**, **session**, and **save for project**.
Reusable file approvals name the individual tool and cover its workspace file
access; other actions match the exact prepared arguments and tool version.
Child requests remain one-time; configure future child access in the root session.
Project defaults store profile/tool/skill choices in `.werkworker/config.json`
and trusted permissions in the host's `werkworker/workspace-access` configuration
directory, bound to workspace identity. Editing a repository file cannot grant
itself rights. The permissions editor can forget remembered approvals.

Selections use a bright cyan background, actions and shortcuts are cyan, enabled
states are green and restrictions are amber. Ctrl-C copies selected text, Ctrl-V
pastes, a single right-click copies a selection or pastes, and the wheel scrolls
the pane under the pointer. F6 switches to native terminal selection.
The prompt has a visible editing cursor: arrows move through the text,
Ctrl-Left/Right move by word, Home/End move within a line, and Delete/Backspace
edit at the cursor. Alt-Enter or Ctrl-J inserts a newline; Enter sends the message.
Wrapped and multiline drafts scroll to keep the cursor visible while editing.
PgUp/PgDn scroll the conversation; Ctrl-Home/End jump to its start/end
(Home/End also do this when the draft is empty).
Large pastes appear as numbered blocks such as `[Pasted text #1 +24 lines]`.
The full text is sent to the model; Backspace at the block's end removes it as
one unit. The complete message supports up to 64 KiB; oversized pastes are
rejected with an error instead of silently shortening the prompt.

## Parallel subagents

The agent can delegate independent tasks with `delegate_task`, continue its own
work, and collect the child results. In the TUI, `/agents` shows the tree; child
approvals and questions are addressed individually. Cancellation cleans up the
whole tree. All three bundled profiles include the orchestration tools.

```sh
ww --werk-config examples/werk-config.json \
  --execution-config examples/execution-config.json diagnose
ww --werk-config examples/werk-config.json \
  --execution-config examples/execution-config.json \
  --grants examples/readonly-grants.json tui --profile general-readonly \
  "Delegate architecture and documentation reviews in parallel, then summarize both."
```

Defaults allow four active subagents and up to four concurrent model requests.
WerkProtocol configured limits (refreshed while running), observability, or
deployment diagnostics establish server capacity; unknown concurrency gracefully limits inference to one request. Children
still run as independent tasks. Shared-workspace writes are serialized and
checked for stale edits. See [configuration and safety boundaries](docs/subagents.md),
[SDK](docs/sdk.md), [protocol](docs/protocol.md) and
[experiments, raw data and benchmark results](docs/benchmarks/2026-10-19-subagents/README.md).

Context and output reservations respect the provider limit before every request.
Automatic rolling compaction keeps long tool runs within that budget while
preserving user constraints and complete tool pairs. If Werk cannot report its
context size, set `capabilities.context_tokens` explicitly in the connection JSON.
Coding runs require a current-file verification receipt before completion and
retain plans/checks across compaction. Interactive runs continue without aggregate
time/step cutoffs; `--continuous` enables this for headless runs.
See [coding acceptance](docs/harness.md#coding-acceptance-and-continuous-work) and
[server limits and compaction](docs/context.md).

## Minimal SDK

```rust,no_run
use std::sync::Arc;
use werk_worker::{WorkerEngine, Limits, Result};
use werk_worker::policy::HostPolicy;
use werk_worker::profiles::AgentProfile;
use werk_worker::providers::{ScriptedProvider, ScriptedTurn};

#[tokio::main]
async fn main() -> Result<()> {
    let engine = WorkerEngine::builder(Arc::new(ScriptedProvider::new(vec![
        ScriptedTurn::text("Hello from the shared harness."),
    ]))).build().await?;
    let session = engine.create_session(
        AgentProfile::bundled("general-readonly")?,
        std::env::current_dir()?, HostPolicy::default(), serde_json::Value::Null,
    ).await?;
    let run = session.submit("Hello", "command-1", Limits::default(), None).await?;
    println!("{:?}", run.wait().await?);
    engine.close().await
}
```

`submit` returns a handle before execution finishes. `run.events(cursor)` streams
durable facts; `wait` requires terminal persistence and managed job cleanup.
Always call `close().await`. See [SDK reference](docs/sdk.md) for approvals,
interaction, replay, resume and host integration.

## Examples and verification

```sh
cargo run --example sdk_consumer
cargo run --example automation
cargo run --example coding       # requires a real Bubblewrap sandbox
cargo run --example eval         # emits one JSON report per fixture
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Use `WERK_WORKER_BWRAP=/absolute/path/to/bwrap` for the examples/tests if the
binary is not installed at `/usr/bin/bwrap`. `REQUIRE_SANDBOX=1` makes a missing
backend fail the isolation test and coding eval. The dedicated CI job sets it.
The CLI supports `--bubblewrap`; otherwise it detects the system binary or the
per-user installation described in [setup](docs/setup.md).

[Coding example](examples/coding.rs) edits a broken fixture and checks its actual
sandbox exit code. [Automation example](examples/automation.rs) reads meeting
notes, loads a skill and writes structured previews; an optional explicitly
configured sample API asks before each POST. [JSONL client](examples/jsonl_client.py)
demonstrates approval, input and cancellation. A [separate consumer crate](examples/sdk-consumer)
links only the public library.

## References and limits

- [Contributing and feature-branch workflow](CONTRIBUTING.md)
- [Architecture](docs/architecture.md), [reference study](docs/reference-study.md)
- [SDK and events](docs/sdk.md), [JSONL protocol](docs/protocol.md)
- [Tool, skill and profile authoring](docs/authoring.md)
- [Security and platform matrix](docs/security.md)
- [Persistence and recovery](docs/recovery.md), [context and cache semantics](docs/context.md)
- [Verification and evals](docs/testing.md), [local validation report](docs/validation.md)

This is an initial local single-host implementation. SQLite snapshots favor
clear atomic boundaries over high-throughput event storage. Tools execute
serially (concurrency bound one); independent sessions may run concurrently.
Filesystem search is bounded and nonrecursive. Sandbox resource limits are
platform-specific: Linux uses rlimits rather than aggregate cgroup quotas,
Windows uses Job Object limits, and macOS's constrained mode denies subprocesses.
These limitations are explicit in the security documentation. No live-model
quality or benchmark parity is claimed.

## What’s new in v0.3.3

WerkWorker SDK and both CLI commands share release version **0.3.3**.

- fix(): pipeline release (`bb16c1b76801`)

See the [v0.3.3 changelog](CHANGELOG.md#033---2026-11-10) for all changes and compatibility notes.
