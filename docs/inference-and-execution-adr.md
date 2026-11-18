# ADR: Inference routing and local execution authority

## Decision and audit, 18 November 2026

The host selects one inference connection before assembling or disclosing agent
context. A model can propose tool calls. WerkWorker owns the tool registry,
the workspace lease, the approval decision and the operating-system execution
boundary. The existing Windows tree profile remains unqualified.

The audit used the local working trees at WerkWorker `0d61dcb` and werk1112
`a9888e0`, the feature diffs against their respective base branches and the
previous native package evidence. The existing `ModelProvider`, two HTTP
transports, negotiated chat and inference-job operations cover most of the
inference contract. Missing evidence included equivalent policy outcomes through
both transports, disclosure-aware ordered routing, and successful native
process-tree qualification. [WerkWorker PR](https://github.com/philipbodenbach/werkWorker/pull/24)
and [werk1112 PR](https://github.com/philipbodenbach/werk1112/pull/49).

## Inference boundary

Native model inventory, chat with tool calls, streaming, optional inference
tools, usage and media-job cancellation use the versioned Werk Protocol. Chat
tool calls already carry the call ID, function name and arguments. The HTTP
request ID identifies the inference exchange. The worker assigns session, run,
step and invocation IDs locally. It resolves the requested capability and
validates arguments against the registered tool, the granted resource scope and
the current host policy. Session identifiers and local execution receipts have
no role in authorizing an inference request.

Explicit third-party selection continues to use the configured compatible API.
For ordered routing the host pins the candidate's provider, origin, model,
credential reference and permitted disclosure. Treat agent messages, images,
tool results and project instructions as workspace context. Exclude candidates
without permission for that class before sending it. A local-first preference
orders eligible candidates. Availability and capability negotiation finish
before the first generation. Pin the chosen connection through a run and its tool
replies. An uncertain delivery, an authentication failure or cancellation ends
the attempt. No implicit model or credential transfer is authorized. A loopback
Werk address identifies the first hop. The operator must also authorize the
configured inference deployment and any upstream processor it uses.

## Execution and audit boundary

The trusted worker prepares each operation from its registered definition. Host
approvals bind to the prepared operation, including its resource and sandbox
description. The host checks again before dispatch. The durable invocation
records the decision, prepared action and result. The model receives the scoped
tool result for its call. Host pipe handles, access tokens, provisioning
credentials and full approval descriptions belong to the trusted host.

The native runner accepts a single bounded launch over private local pipes.
The host authenticates the runner, inspects the suspended process and token and
owns the concrete non-breakaway, kill-on-close Job. The runner receives only the
launch request and the designated I/O. Both synchronous and background processes
use the host supervisor. The lease lasts until confirmed cleanup. Cancellation,
timeout and uncertain startup must preserve the cleanup barrier.

### Request and evidence contracts

| Operation | Contract and authority |
| --- | --- |
| Model capabilities and availability | The Werk adapter negotiates the service, API version and advertised operations. Direct compatible endpoints use their configured capabilities and model inventory. Runtime support for a particular model can still fail at generation. |
| Tool intent and reply | The chat payload carries a function call ID, name and arguments; the worker validates the registered schema before dispatch. The tool reply cites that call ID. A model-supplied permission or sandbox field cannot grant authority. |
| Local execution receipt | A privileged call has a local `ToolInvocation`, its step, prepared scope, approval decision where applicable, lifecycle events and result. Process output reports exit status, cancellation, timeout and truncation. Job lifetime observations belong to the supervisor. Approved CPU, memory or process ceilings describe **limits**. Only measured counters can be reported as consumption. |
| Inference usage and cancellation | Chat carries model usage when available. Dropping an HTTP exchange ends the consumer's request; actual interruption of generation depends on the running adapter. Asynchronous media jobs have explicit, principal-bound cancellation. |
| Host cancellation and recovery | Run cancellation reaches managed tools. The host retains Job and workspace ownership until cleanup. Failed or uncertain cleanup must remain observable, including when a caller or broker is lost. |

Disclosure authorization covers task content, including file reads and command
output that may themselves contain private information. A tool owner must limit
its output at the source. The host's absolute workspace path and approval records
are unnecessary for model-side relative file operations. A capability advertised
by a provider can influence inference selection only.

## Windows alternatives

| Candidate | Cargo, descendants and named objects | Write and network boundary | Lifetime and operating cost |
| --- | --- | --- | --- |
| Dedicated account and earlier restricted token | The diagnostic Cargo build started its compiler and tests after the named semaphore was provisioned. | Wider restricting SIDs admit other write grants. Mediated DNS reached the controlled receiver. | Existing provision and Job are reusable. Implicit console hosts exceeded the strict live ceiling. |
| Exclusive private writer SID | Schema 5 prevents the cryptographic initialization needed by Rust before entry. Extra rights on synchronization objects cannot fix this device open. | The additional write check also applies to the device. | Uses existing token verification and ACL provisioning. Full workloads cannot be qualified. |
| AppContainer or LPAC | Default LPAC supports the shipped single-process profile. Descendant initialization and named-object access require separate proof for a tree. | A capability-free ordinary AppContainer still accepts broad, inherited discretionary writes at Low integrity. Packet filters alone do not establish brokered DNS denial. | Requires per-identity storage checks. Actual console hosts must still count towards the Job limit. |
| Narrow resource broker | Can grant scoped application requests. Unmodified Cargo, Rust and the Windows loader issue their own native I/O. | The broker cannot mediate an OS access that never enters its IPC interface. | Reusing authenticated IPC and handle cleanup adds less operational cost than introducing another privileged service. |
| Existing hybrid | Dedicated identity, restricted token, AppContainer, private desktop, host-owned Job and scoped network policy remain the candidate. | An independent inherited file and registry write boundary must permit required device initialization. Integration still needs observed DNS and process caps. | Retain the existing supervisor, schema-pinned provision and failure recovery. Qualification stays closed. |

The next native implementation must enforce the write scope independently of
the device access mask for every child, including broadly writable objects
outside the private directories. Check a small Rust workload before Cargo.
Then measure all three DNS generations between positive controls and both
two-member console cases on the same build. A cleaned-up process or an empty
packet trace after a refused launch supplies no positive containment evidence.
[Windows acceptance history](windows-process-tree-validation.md) and
[provisioned runtime contract](windows-process-tree-provisioning.md).

## Compatibility and migration

Routing is an opt-in host decision. Explicit Werk and compatible-provider modes
continue to use their configured connection. New HTTP sessions persist their
authorized recipient. For a history created before this binding existed, use
local inspection or export and explicitly choose the context for a new session.
Scripted sessions support offline use. The [routing guide](provider-routing.md)
covers named credentials, inline keys and the [host policy schema](provider-routing.schema.json).

Inference tool intents and usage reuse the existing Werk Protocol 1.0 wire
contract. The backend now observes a client's request lifetime during Candle
CPU decoding. Local tool grants, cancellations and receipts stay in the
existing worker engine. An inference-job ID confers no authority over a
native process job.

Both HTTP transports exercise the same engine test. It reads an authorized
file, waits for host approval before an accepted write, refuses a denied write,
and records a policy denial for an unapproved process. The test checks the
resulting files, the four correlated tool responses and the absence of the
host workspace root, approval identifiers and fixture credential in model
messages. It also covers replay of a classic session, which previously inserted
the host's absolute workspace path into its system prompt.
