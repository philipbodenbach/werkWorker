![werkWorker](docs/assets/werkworker-banner.svg)

werkWorker is a local, general-purpose Rust agent engine with its own harness.
The default `ww` command opens an interactive terminal in the werk top palette.
The package is `werk-worker`, the Rust import is `werk_worker`, and the binary is
`werk-worker`, with `ww` as its short command. Both commands are installed by
Cargo and expose the same CLI. It is licensed under [Elastic License 2.0](LICENSE).

The SDK runs an asynchronous model/tool loop with streamed events, explicit
resource grants and approvals, SQLite sessions, cancellation, steering,
follow-ups, crash recovery and parallel subagents with separate conversations. Coding and document/ticket automation use the
same engine. Four bundled Agent Skills and three profiles are embedded in the
package; they do not require a repository checkout at runtime.

Werk1112 supplies inference and model/runtime management. werkWorker supplies
agent orchestration. Future werkCoder and werkStation clients consume this SDK
or the JSONL protocol. There is no GPU runtime, Jira implementation or
autonomous package installer here. Scripted tests measure the harness, not model
intelligence.

## Install and try

Rust 1.88 or newer. Native packages target Linux x86_64/ARM64, Windows x86_64
and macOS Apple Silicon; file/process isolation currently requires Linux.
See [install scripts and release workflow](docs/releases.md). Build from this
checkout (nothing is published by these commands):

```sh
cargo install --path . --locked
ww tui --demo "Hello"
ww --help
ww profiles list
ww skills inspect ticket-drafting
ww sandbox doctor
```

The demo is explicitly offline. A missing model connection never falls back to
scripted replies. Process tools require Linux user namespaces, a usable
Bubblewrap installation and the kernel interfaces described in
[the sandbox matrix](docs/security.md). Missing isolation fails closed.

With no arguments, `ww` opens an interactive setup: choose a Werk server, enter
its API key in a masked field, discover models, and select token budgets,
profile and workspace access. Settings can be saved globally in
`~/.config/werkworker/config.json` or as project preferences in
`.werkworker/config.json`. Keys are session-only; reusable credentials use an
environment-variable reference. `/setup` reconfigures after a run finishes.
On subsequent starts, **Continue previous** reconnects to the remembered service;
**Connect new** opens setup. Choose **New session** or a previous conversation
to restore its history and continue. `/sessions` opens this list from chat.
See [setup and authentication](docs/setup.md) and [terminal controls](docs/tui.md).

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
Werk deployment diagnostics or WerkProtocol observability must establish server
capacity; unknown concurrency gracefully limits inference to one request. Children
still run as independent tasks. Shared-workspace writes are serialized and
checked for stale edits. See [configuration and safety boundaries](docs/subagents.md),
[SDK](docs/sdk.md), [protocol](docs/protocol.md) and
[experiments, raw data and benchmark results](docs/benchmarks/2026-10-19-subagents/README.md).

Context and output reservations respect the provider limit before every request.
Automatic rolling compaction keeps long tool runs within that budget while
preserving user constraints and complete tool pairs. If Werk cannot report its
context size, set `capabilities.context_tokens` explicitly in the connection JSON.
See [server limits and compaction](docs/context.md).

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
Linux rlimits, not aggregate cgroup quotas. These limitations are explicit in
the security documentation. No live-model quality or benchmark parity is claimed.

## What’s new in v0.1.0

werkWorker SDK and both CLI commands share release version **0.1.0**.

- Initial agent engine, native packaging and interactive terminal.

See [build, install and release](docs/releases.md).
