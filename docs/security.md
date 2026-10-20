# Trust boundaries and sandbox matrix

The host principal owns engine setup, provider secrets, installed Rust tools,
grants and the control channel. Model replies, skills, workspace instructions,
documents, HTTP responses and tool output are untrusted data. A skill cannot
register tools or change grants. Trusted Rust tool plugins can execute host
code and must be reviewed/installed by the host; they are not sandboxed plugins.

Three independent gates apply: operating-system isolation, explicit resource
capabilities, and approval of a prepared invocation. A denial or unavailable
sandbox is never retried through a host shell. Internal questions/plans/allowed
skill metadata require no extra external permission.

The interactive host may opt into `HostPolicy.request_approvals`. This exposes
valid registered actions for approval even without prewritten resource grants.
Every non-internal action returns **Ask** in this mode, even with FullAccess or
a previously reusable scope. Hard denies, path validation and read-only child
restrictions are checked first. This is host authority, not a model instruction:
`interaction.ask` answers cannot enable it. The TUI confirmation invokes the
trusted `session.enable_tool_approvals` API, records the host command, expands
registered tool/skill availability and persists the policy in the same session.
Existing skill snapshots are retained; newly available skills are snapshotted.
No action executes merely because this mode was enabled.

## Linux backend

Bubblewrap creates user, mount, PID, network, IPC and UTS namespaces with
read-only `/usr`, fresh `/proc` and `/dev`, private `/tmp`, empty `/home` and
`/run`, no host root/home/Docker socket and no inherited environment. Only the
explicit workspace is bound, read-only unless the host assigned write access.
The workspace is mounted from its pinned descriptor (`--bind-fd`), preventing
a path-swap between validation and mount. All host descriptors are marked
close-on-exec except this consumed mount descriptor. Host stdin is not passed.
`no_new_privs` and dropped capabilities apply before untrusted execution.

Files use `openat2` with `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_XDEV`.
Relative paths only; no traversal, symlinks, special files or foreign mounts.
File and process tools see the same allocated workspace. Edits compare the
observed hash and atomically rename in a pinned parent. Worker-owned mutations
and writable processes hold exclusive operation leases across overlapping
workspaces in one engine; reads and inference can overlap across sessions.
Background processes retain their lease until actual cleanup.
External writers are not filesystem transactions: an external process modifying
the same inode at the instant after comparison can race an edit. Use an isolated
staged workspace where concurrent untrusted writers are possible.

Processes have bounded wall time/output, RLIMIT_AS (1 GiB default), CPU (30 s),
file size (16 MiB), open descriptors (128), core size (zero) and a finite
RLIMIT_NPROC ceiling. Linux charges NPROC to the real UID, including host
threads: the effective ceiling is the observed UID task baseline plus 128,
not a private 128-process cgroup quota. CPU/address-space/file-size limits are
per process; aggregate memory, total workspace disk consumption and per-run CPU
accounting are **not cgroup quotas**. This backend shares the host kernel and
is not VM isolation. It is unsuitable for claiming aggregate resource containment
against deliberately hostile fork-heavy workloads. The wall deadline and PID
namespace teardown still bound managed lifetime and reap descendants.

No network is provided to processes. A fixed approved program/argv can execute
arbitrary code from its workspace; the approval UI therefore shows actual mount,
write and network rights, not a claim to understand subprocess semantics.
Shell execution is possible only as explicitly requested program/argv (for
example `/bin/sh -c ...`) and is fully visible in its concrete approval.

| Platform/backend | Supported/tested |
|---|---|
| Linux + Bubblewrap 0.6.1 or newer + kernel openat2/close_range | Implemented; real WSL2 Linux tests performed locally |
| Linux CI runner | Required isolation job installs Bubblewrap and fails on any skip |
| Outer restricted sandbox without namespace/netlink permissions | Fails closed; tests explicitly report unavailable |
| Docker Desktop in this development WSL | Unavailable; no Docker fallback implemented |
| macOS/Windows native SDK/TUI packages | Native packaging and CI checks configured; file/process isolation unavailable and fails closed |

## Brokers and credentials

Generic `http.fetch` permits only GET/HEAD, disallows redirects, userinfo and
fragments, disables inherited proxies, bounds DNS/time/body, rejects private,
loopback, metadata, mapped/transition/reserved targets and pins every connection
to validated DNS results. The low-level trusted `runtime::fetch` API can receive
an explicit exact private origin; the generic model tool never enables it.

Werk and the example ticket connector use separate fixed-origin broker clients.
Remote credentials require validated HTTPS. Optional literal-loopback HTTP is
explicit. Redirects never receive credentials. Auth is resolved separately from
serializable config; broker keys are absent from tool inputs and sandbox env.
Provider errors never include response bodies/URLs. Known credentials are
redacted from response data, including secret strings split across SSE fragments.
No telemetry export or cloud fallback is configured. Arbitrary secrets a user
puts directly in their own prompt/file are still their data; this is not a
universal DLP system.

Stores/artifacts are local private files. Session creation and reopening reject workspaces
containing configured SQLite/artifact storage, host root and host home. The CLI
defaults to XDG_STATE_HOME/werk-worker (or ~/.local/state/werk-worker),
and LOCALAPPDATA/werk-worker on Windows. Custom
store wrappers must preserve `SessionStore::protected_paths`. Artifact IDs are not host paths; access checks
session ownership, hash, seven-day retention and size. Quotas: 4 MiB/artifact,
1,024 artifacts/store. Expired data is denied, not silently re-exposed. Operator
cleanup/export is explicit; secure erasure and automatic garbage collection are
not claimed.

## Delegated workers

Children inherit grants and may only narrow tools/workspace write access. Fresh
conversations are context separation, not separate filesystem containers. Parent
revocation, cancellation and expiry remain effective for new child tool starts;
reusable approvals are not inherited. Child results are untrusted data. Active
children, nesting and total tree model requests are bounded. A child cannot
address another root through the delegation tools. `read_only` restricts workspace
writes, not independently granted network-write brokers. See [subagents](subagents.md).

## Interactive setup and project metadata

Global connection files contain no API-key values; typed keys remain in memory,
while an environment **name** can be remembered. The project format cannot select
a server origin or credential source. Project/model profiles and instruction files
are lower-trust context and never become grants or sandbox settings. Test commands
are suggestions, not startup hooks. TUI access choices are explicit, session-only
and still require per-action approval; existing `--grants` takes precedence.

Project metadata reads use the pinned workspace reader, reject symlinks and path
traversal, and are bounded. They currently require Linux when files exist; other
platforms fail closed. Sources/hashes/text are frozen into each CLI session;
subagents inherit them without gaining authority. See [setup](setup.md) and
[project configuration](project-configuration.md) for scope and platform limits.
