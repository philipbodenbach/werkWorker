# Dedicated-account Windows process-tree provisioning

The administrator helper is separate from runtime launch. It never requests UAC,
installs a service, starts a sandbox workload or selects the process-tree profile.
Run it only after reviewing its request on a native Windows host. Building the
helper or passing its read-only preflight is not native sandbox qualification.

This local continuation starts at PR #24's `2398f9f`; the original implementation
started at `main` (`8cbefc0`). The PR #24 investigation
at `a2bed0a` and its earlier `ca8d664` evidence reject the LPAC capability
expansion and PSEC/MXC route; they are decision inputs, not an enabled backend.
The existing single-process LPAC mode remains the default.

**The process-tree profile is not qualified and is technically disabled.**
Native tests reproduce a DNS-broker escape from network denial, `AllocConsole`
creating a third Job member despite an active-process limit of two, and a Cargo
jobserver semaphore failure. An explicit selection, even with valid
provisioning, returns `sandbox_unqualified` before workspace admission or
execution; Doctor cannot report this profile available. There is no environment,
debug or feature-flag bypass. The setup contract below describes the candidate;
provisioning does not enable it. The Windows ZIP includes both helpers, but a
successful basic installation does not establish provisioned profile acceptance.
See [the measured results and remaining gates](windows-process-tree-validation.md).

## Setup request

Install the Windows ZIP, which includes `ww-windows-provision.exe` and
`ww-windows-runner.exe` alongside both host commands, or build these helpers with
the native MSVC Rust toolchain. Use the installed runner as `runner_source` when
validating a fresh installation. The setup executable accepts one JSON file:

```powershell
ww-windows-provision.exe --request C:\Reviewed\process-tree-setup.json
```

The process must already have an elevated administrator token. Without one it
returns `admin_required` before reading the request or changing host state. The
host account remains the ordinary user; record that individual account SID in
`host_sid`, even when a different administrator performs setup.

Example request (replace paths and the host SID with reviewed local values):

```json
{
  "provisioning_root": "C:\\werkWorker\\coding-runtime",
  "workspace": "C:\\werkWorker\\coding-workspace",
  "runner_source": "C:\\Reviewed\\ww-windows-runner.exe",
  "host_sid": "S-1-5-21-REPLACE-WITH-HOST-USER-SID",
  "toolchain_sources": [
    "C:\\Prepared\\rust",
    "C:\\Prepared\\msvc",
    "C:\\Prepared\\windows-sdk"
  ],
  "toolchain_environment": {
    "PATH": "{toolchain0}\\bin;{toolchain1}\\bin\\Hostx64\\x64",
    "RUSTC": "{toolchain0}\\bin\\rustc.exe",
    "CARGO_HOME": "{temp}\\cargo",
    "INCLUDE": "{toolchain1}\\include;{toolchain2}\\Include",
    "LIB": "{toolchain1}\\lib\\x64;{toolchain2}\\Lib"
  }
}
```

Paths and layout are examples; real MSVC/SDK versions have versioned include/lib
subdirectories. Supply only the exact compiler/runtime/cache inputs needed by the
workload. Setup copies the explicitly named trees into new numbered toolchain
roots; it does not expose or change ACLs on an entire developer profile. Reparse
points are rejected. Source directories remain untouched. Both output roots must
be new, with existing local parents. Environment entries are limited to PATH,
LIB, INCLUDE, LIBPATH, CARGO_HOME, RUSTUP_HOME and RUSTC. Their paths must stay in
the new toolchain, workspace or private-temp roots.

## Created state

Setup creates a generated `wwbox_…` local account, initially disabled, with a
cryptographically generated password. While it is disabled, setup adds its exact
SID to the built-in Users group (`S-1-5-32-545`), resolving that group's localized
name from its SID. `NetUserAdd` alone left the native test account without this
membership and with a `USER_PRIV_GUEST` classification. Runtime continues to
require `USER_PRIV_USER` and rejects any local group other than Users.
The password is stored as a DPAPI
LocalMachine ciphertext in `credential.dpapi`; its ACL allows only SYSTEM,
Administrators and the configured host SID. The plaintext is never written to a
log, command line or JSON manifest. Protected `provisioning.json` and
`invocation.lock` have the same trusted principals. The sandbox account receives
read/execute on the copied runner/toolchains and modify access on the dedicated
workspace/private temp. Existing user directories, host firewall profiles and
services are not modified.

A transaction adds a new persistent WFP sublayer and seven account-scoped block
filters. Six cover ALE_AUTH_CONNECT, ALE_AUTH_RECV_ACCEPT and
ALE_RESOURCE_ASSIGNMENT for IPv4 and IPv6, with no address, protocol, port or
loopback exception. The seventh blocks RPC calls matching the dedicated client
account at RPC_UM through REMOTE_USER_TOKEN. This is an account-specific rule;
other users' RPC and DNS configuration are unchanged.

The RPC rule attempts to address a native failure of the six socket-attribution rules:
`DnsQueryEx` transmitted and resolved a fresh synthetic query through the Windows
DNS path despite those rules. RPC filtering with a client security descriptor is
[documented by Microsoft](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/netsh-rpc).
The current native test still receives that query with all seven filters present;
the additional rule is not an effective DNS-broker boundary on this host.
All seven filters must pass exact runtime readback, including layer, condition,
SID, block action, priority and persistence. Actual direct-network, loopback and
DNS-broker controls still have to pass; installing a rule is not proof of its
runtime effect. Offline Cargo must also prove compatibility with the RPC denial.

Setup requests the maximum sublayer priority, but BFE may assign the closest
available value ([Microsoft's provider-installation example](https://learn.microsoft.com/en-us/windows/win32/fwp/installing-a-provider)).
Manifest schema 3 requires all seven filters and records the actual
`network_sublayer_weight` read back after
installation. The host-approved descriptor and Doctor expose it, and every
launch must match that exact protected value. Older six-filter receipts and receipts without this field are rejected. The assigned weight is not a claim that this is the host's highest
priority sublayer; the mandatory native network controls remain required.

The payload uses a restricted primary token of the dedicated account, with
maximum privileges disabled and write-restricting SIDs limited to that account
and Everyone. Inherited logon SIDs are explicitly deny-only and cannot grant
access to host-session objects.
Only traverse privilege is enabled. This is an ACL-based account boundary;
ordinary Windows read grants are not a strict executable/filesystem allowlist.
Before starting that payload, the trusted runner protects its own process,
threads and primary token with explicit DACLs. It also sets a protected default
for objects created by future runner threads before enumerating existing threads.
The sandbox account receives only process query and synchronization; it cannot reopen these objects for
injection, handle duplication or token duplication. OWNER RIGHTS removes implicit
owner permission to rewrite those DACLs. The trusted host retains process query,
synchronization and termination rights. The runner verifies the applied DACLs
and the suspended payload's restricted token before permitting execution.

The trusted dedicated-account bootstrap creates a fresh noninteractive Window
Station and UUID-named Desktop, then associates only its own process/thread with
them. It reports their handles over authenticated pipes. The host immediately
owns each successful duplicate, checks the exact creation ACLs, then sets and
reads back protected ACLs owned by the host: SYSTEM/host have full access, while
the sandbox has station `0x2037b` and desktop `0x20087`. These sandbox grants omit
DELETE, WRITE_DAC, WRITE_OWNER and station clipboard access. A native control
proved that the Job write-clipboard flag alone still allowed EmptyClipboard;
the separate station ACL denies both opening and clearing it. Existing USER objects and their ACLs are never
changed. The host itself never switches station or desktop.

Before acknowledging this stage, the host applies and reads back the eight
selected Job UI restrictions (`0xff`). SDK 10.0.26100.0 also defines IME (`0x100`)
and INJECTION (`0x200`), with `JOB_OBJECT_UILIMIT_ALL=0x3ff`; those additional
bits were not applied or qualified by these tests. The runner then derives the restricted payload token,
seals its own objects and creates the payload suspended with an explicit private
station/desktop and exactly three inherited stdio handles. A second authenticated
handoff lets the host retain the payload, verify its initial suspension and
membership in the exact owned Job, and recheck effective limits before resume.
The trusted supervisor retains both USER resources and the workspace lease until
the complete process-handle ledger matches the Job lifetime process count,
every recorded process is signaled, the Job is empty and USER closure succeeds.

Native controls proved that Win32k system-call lockdown caused Cargo and Rustc
DLL initialization to fail with `0xC0000142`; the same binaries started without
that mitigation. Cross-account explicit-desktop bootstrap also failed User32
initialization. NULL startup alone was unsafe because it copied the enabled host
logon SID and a broad WinSta0 handle. Only trusted bootstrap code uses that initial
association. The actual payload uses the new station and disabled logon SIDs.
The candidate now passes native USER initialization/handle checks and selected
real-tool UI and file/registry/IPC/direct-socket controls. It must still pass the
complete resource, DNS-broker, Cargo and lifecycle acceptance. Failure
at either authenticated barrier refuses startup without a fallback.


Setup enables the account only after the protected receipt and filters are ready.
It then validates the actual enabled/password-required standard-user state and
complete group membership before returning success.
An incomplete setup returns the retained directory/account and attempts to keep
the account disabled; it never emits a successful receipt for runtime use. Do not
reuse partial output directories or remove filters while an account can run.
Administrative recovery/removal of partial provisioning is explicit.

## Read-only launch checks

The host loads the exact configured manifest for the exact workspace, pins
trusted files and ancestor directories, verifies their owner/ACL and runner hash,
checks the local account SID and nonprivileged group membership, and queries each
actual WFP filter. A copied manifest, missing filter, different SID, unexpected
filter shape, denied WFP query or changed runner is a launch error. A receipt does
not substitute for those checks. The loader grants no global WFP permissions: if
the ordinary host cannot query the required WFP objects, that is a concrete setup
blocker that must be resolved explicitly before qualification.

The runtime must retain its workspace lease and protected invocation lock until
owned-Job drain and all I/O cleanup are confirmed. Provisioning alone does not
prove cross-user startup, creation-time Job membership, descendant restrictions,
resource budgets or cleanup after host death. Keep the profile disabled until the
native acceptance harness establishes those guarantees, including offline Cargo
with the explicitly supplied Rust/MSVC toolchain.

The host queries its exact owned Job handle and verifies the suspended runner's
membership before execution. `QueryInformationJobObject(NULL)` in the runner
instead describes its [immediate Job](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-queryinformationjobobject).
On the native cross-account path that query returned zero limits while the same
process belonged to the correctly limited host-owned Job. A runner-local NULL
query therefore cannot validate this contract. No Job handle is passed into the
tree: retaining one there would defeat kill-on-close when the host dies. The
acceptance suite queries the exact host-owned Job and exercises its limits.

For the private-desktop runner, the ordinary host cannot assume direct access to
the other account's primary token. The trusted launcher uses the already
provisioned credentials for a local interactive logon, briefly impersonates that
account solely to open the actual suspended child's token with `TOKEN_QUERY`,
and reverts before any reply or subsequent I/O. A failed revert terminates the
launcher; the independent host supervisor still owns the Job and cleanup. The
authenticated version-4 handoff retains that query-only token until the host has
duplicated it without inheritance and checked its SID, primary-token type and
non-elevated state. The host never impersonates the account and no token ACL is
expanded. This follows the documented
[cross-account token access rules](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocesstoken)
and [explicit-credential impersonation](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-impersonateloggedonuser).

Both trusted startup stages use System32 as their working directory and a minimal
system environment. The payload receives its separately approved toolchain
environment and workspace directory. For programs such as `cmd.exe`, a verbatim
drive path is converted to DOS spelling only after that spelling resolves back
to the identical canonical workspace path; a mismatch refuses execution.

## Host selection and approval

Normal startup does not provision accounts, adjust ACLs, request elevation or
repair policy. Store this configuration outside the workspace:

```json
{
  "windows_process_tree": {
    "provisioning": "C:\\werkWorker\\coding-runtime\\provisioning.json"
  }
}
```

The configuration spelling is `ww.exe --native-sandbox-config C:\Reviewed\native.json` and
`--workspace C:\werkWorker\coding-workspace`. Omitting `windows_process_tree`
selects the existing LPAC backend. The SDK equivalent is
`NativeSandboxConfig { windows_process_tree: Some(WindowsProcessTreeConfig {
provisioning: manifest_path }), ..Default::default() }`. Only the trusted host
chooses this field. The current qualification guard rejects this selection with
`sandbox_unqualified`, including otherwise valid provisioning. Once qualified,
the profile requires workspace write authority; there is no read-only downgrade
or fallback after a provisioning error.

`--process-limits C:\Reviewed\process-limits.json` selects the existing finite
limits. A coding example is:

```json
{
  "timeout_ms": 120000,
  "output_bytes": 262144,
  "memory_bytes": 4294967296,
  "cpu_seconds": 120,
  "processes": 16,
  "browser_processes": 512
}
```

The process ceiling includes the trusted runner. During startup a trusted
launcher uses one temporary slot and exits before payload execution. Memory and
user CPU time are aggregate Job limits. The `process.run` tool accepts at most
32768 UTF-8 bytes of stdin; direct SDK execution has a 1 MiB transport ceiling.
Each output stream uses the selected byte ceiling and is still drained after
truncation.
The full backend descriptor, manifest digest, account, explicit runtime roots,
environment and effective limits participate in the existing approval hash and
are rechecked before dispatch. `process.run`, `process.start`, `process.status`
and `process.cancel` use the same selected profile and JobManager lifecycle.

```powershell
ww.exe --native-sandbox-config C:\Reviewed\native.json `
  --process-limits C:\Reviewed\process-limits.json sandbox doctor
```

The current Doctor returns unavailable for this profile. The candidate's
underlying Doctor validates provisioned state and probes startup in the
provisioned workspace; that startup probe cannot certify Cargo, descendants,
network denial or host-death cleanup. It must not override the qualification
guard merely because a command starts successfully.

## Native acceptance

Use a fresh provisioning workspace for this test: it writes synthetic fixtures
and a dependency-free Cargo project. Prepare a real Rust toolchain directory
(`rustc --print sysroot`), the matching MSVC tools, and Windows SDK inputs. Do not
use rustup proxy executables without their toolchain. In a normal x64 Developer
PowerShell, build and test from a native NTFS source checkout:

```powershell
cargo build --locked --offline --target x86_64-pc-windows-msvc --bins
$env:WW_WINDOWS_PROCESS_TREE_PROVISION = 'C:\werkWorker\coding-runtime\provisioning.json'
$env:WW_WINDOWS_PROCESS_TREE_CARGO = 'C:\werkWorker\coding-runtime\toolchains\0\bin\cargo.exe'
$env:WW_WINDOWS_PROCESS_TREE_EXTERNAL_ENDPOINT = '<controlled-reachable-nonloopback-ip>:<tcp-port>'
$env:REQUIRE_WINDOWS_PROCESS_TREE = '1'
cargo test --locked --offline --test windows_process_tree -- --nocapture --test-threads=1
```

The external endpoint must be a listener you control and that the ordinary host
can reach. The suite verifies that positive control before testing denial from
the sandbox. It also creates its own local socket/IPC controls; it does not
change host DNS configuration.

The delivered guard also makes the required process-tree gate fail until the
backend is corrected and qualified. Previously recorded candidate runs use
explicitly frozen diagnostic builds, not a runtime bypass in the delivered
binary. The required gate fails when provisioning is missing; an optional `NOT RUN`
message is never a successful feature qualification. The harness enters through
the real Engine/tool interfaces and checks parent/child/grandchild receipts,
offline Cargo output/artifact, exact owned-Job membership/limits, bounded I/O,
synthetic file/registry/IPC/network controls, timeout, cancellation, dropped
caller, Engine close and abrupt host death. Existing single-process native tests
remain an independent required gate. Toolchain source copies and admin setup are
explicit prerequisites; a host-side Cargo build is not the sandbox acceptance.

For regular `process.run` completion, the harness checks retained process handles
with a zero timeout immediately after the real tool returns, before another
provider turn or fixture read. Timeout, cancellation and Engine close also
require signaled handles without a cleanup grace period. Abrupt host termination
is a separate case with a bounded observation deadline and a required persistent
cleanup marker. See [the recorded validation status](windows-process-tree-validation.md)
for completed checks and any outstanding qualification blocker.

## Cleanup and interrupted-host recovery

The trusted supervisor owns the Job, its handles, the workspace lease and the
provisioning lock until every process object in its complete retained ledger is
signaled, the Job is empty, and its independent Desktop and Window Station handles
are successfully closed. A host-only completion-port monitor starts before the
first Job assignment and retains each discovered process object, verified against
the exact owned Job. Notifications are only discovery hints. After sampling every
process handle, the host requires the authoritative lifetime `TotalProcesses` to
equal the number of unique retained objects. Missing events, inaccessible objects,
monitor failures, or more than 4096 lifetime processes keep cleanup unconfirmed;
no count is subtracted for a limit event. This history cap is bound into approvals.
Native controls showed that `ActiveProcesses == 0` and an empty current PID list
can both precede process-handle signaling, so neither alone is a completion
barrier. The supervisor survives the loss of its asynchronous caller.
Kill-on-close handles abrupt host death;
neither launcher nor payload inherits an owned-Job handle or breakaway authority.
Before the first process is created, the host also flushes a protected
`incomplete-run.json` receipt. Only confirmed cleanup removes it. The OS releasing
a crashed host's lock therefore cannot silently admit workspace tools while
cleanup is uncertain: subsequent tool preparation and execution fail with
`cleanup_required`. The engine acquires the cross-host invocation lock before
workspace preparation reads and retains the same guard through approval and
execution. Background jobs pass that guard to their trusted cleanup supervisor.
Status, log, wait and cancel remain available. Direct builtin calls reacquire the
guard at each entry point; arbitrary trusted SDK or external filesystem access
is outside this cooperative tool contract.

The host-death acceptance case deliberately runs last and leaves this receipt.
Recovery is a separate trusted operation: stop all hosts using this provision,
verify that no processes of its exact `user_sid` remain, and only then remove the
receipt. If identity/process inspection or termination cannot be confirmed, keep
the receipt and the profile disabled. This version does not automatically repair
crash state. The private temp directory remains provisioned; cleanup confirms
process/handle termination, not deletion of all build outputs or caches.

The reference architecture is the pinned OpenAI Codex implementation:
[trusted account runner](https://github.com/openai/codex/blob/95dcd1e8378021080bf380e42587ef891f611897/codex-rs/windows-sandbox-rs/src/elevated/runner_client.rs),
[account-scoped WFP setup](https://github.com/openai/codex/blob/95dcd1e8378021080bf380e42587ef891f611897/codex-rs/windows-sandbox-rs/src/wfp.rs).
The filter set here requests unconditional denial for the dedicated account,
but the measured DNS-broker path still bypasses it. Codex's published WFP set
complements a separate firewall policy; the WFP rules alone are not a proven
offline boundary.
