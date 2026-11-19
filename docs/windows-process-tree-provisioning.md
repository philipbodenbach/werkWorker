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

**The process-tree profile remains disabled pending native qualification.**
Selecting it returns `sandbox_unqualified` before workspace admission.
The schema-5 backend combines a zero-capability AppContainer with the
dedicated-account Job and network filters. Release qualification requires the
complete native tree suite and an installed offline Cargo invocation on the
same package. [Native validation results](windows-process-tree-validation.md)

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

In schema 4 a mediated `DnsQueryEx` reached the synthetic DNS server with
all seven filters installed. Schema 5 also places the actual workload in an
individual AppContainer with zero capabilities. The runner checks its package
SID, Low integrity, restricting SID and empty capability list on the suspended
token. The DNS acceptance test measures each of three generations between
two positive host queries.

Setup records the actual BFE sublayer weight and the identities of all seven
filters. The host checks their layer, condition, SID, action, weight and
persistence against the protected manifest at launch. Schema 5 also binds
the individual package SID, writer SID and nonzero interactive session.
Earlier experimental receipts require fresh provisioning.

Setup creates an empty SAM group for the individual write restriction and a
fresh profile for the dedicated user. A trusted helper running as that user
registers its unique AppContainer. The private temporary directory is the
package's Windows-selected `AC\Temp`. Together with the workspace, it receives
the dedicated user, package and writer grants and an inheritable Low integrity
label. The individual host receives a non-inheriting metadata, traversal and
synchronization grant on the new profile directories leading to that temp.
Before execution the runner checks the actual suspended token's effective
access to package storage, including the package registry.

The runner and host authenticate the package-private named-object directory
using the suspended token, session, object type, name, owner and DACL. The host
adds a non-inheriting `0x000f` grant there for the individual writer so Rustc
can create its jobserver semaphore. It restores the original DACL after all
retained processes signal and the Job drains. An unexpected DACL change keeps
the workspace lease held.

The payload uses a write-restricted primary token of the dedicated account.
The empty writer group is its sole restricting SID. Logon SIDs are deny-only
and only the traversal privilege is enabled.

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
membership in the exact owned Job, and recheck effective limits. In version 3 of
this handoff the host resumes the verified main thread. The runner waits for
the payload to exit. The host accepts a normal completion only when the retained
payload handle is signaled and its own exit code matches the runner's.
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


Setup enables the dedicated account after securing its credentials, pending
receipt and filters. It checks group membership, registers the package and
binds its private temporary directory before publishing `provisioning.json`.
On failure the helper records the account-disable and rollback outcomes.
Administrative recovery of incomplete provisioning uses the retained identity
and directory.

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
system environment. The host sends approved toolchain values only to the
dedicated runner. Provisioning checks the values against the allowed roots.
Before creating private USER objects the runner checks each environment name
against the same toolchain allowlist or one of the six fixed host fields. It
checks SystemRoot and WINDIR against the native Windows directory. TEMP, TMP,
HOME and USERPROFILE must all identify the dedicated package temp. The
runner checks its own primary token's user SID, the derived package SID and
the temp directory's file identity, then supplies LOCALAPPDATA from that
account's Windows profile.

The payload receives the approved workspace directory. For programs such as `cmd.exe`, a verbatim
drive path is converted to DOS spelling only after that spelling resolves back
to the identical canonical workspace path; a mismatch refuses execution.

## Host selection and approval

Store the host-selected configuration outside the workspace. Runtime verifies
the administrator-provisioned state before taking the private namespace grant.

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
launcher uses one temporary slot and exits before payload execution. At a limit
of two the root receives the child-process restriction at creation. The runner
checks this policy before resume. The native console cases count every live
member, including an implicit console host. Memory and
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
python scripts/validate-windows-process-tree.py suite --source . --manifest C:\werkWorker\coding-runtime\provisioning.json --evidence C:\werkWorker\fresh-suite-evidence --bind-address <assigned-nonloopback-ip>
```

The validator binds its TCP witness to the chosen local interface. The host
sends distinct controls before and after the boundary case. Each must receive
an acknowledgement. After the suite exits, the validator obtains a final
acknowledgement and checks that the listener accepted exactly those connections.

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

A separate preparation can be used when the administrator and acceptance host
run in different processes. In the normal user's session, first run
`scripts/test-windows-process-tree.ps1 -EvidenceRoot <new-dir> -PreflightOnly -RequirePsec`.
Use the verified host SID from `preflight/preflight-result.json` for both setup
requests. Have the administrator provision two fresh runtimes and empty
workspaces using the setup contract above in the same Windows session. Use the
runner from the candidate archive and an administrator-controlled copy of
the provisioning executable and reviewed requests. Record the archive SHA-256.
Then run this command in the ordinary user's x64 MSVC environment with the
locked Cargo dependencies cached:

```powershell
./scripts/test-windows-process-tree.ps1 -EvidenceRoot $newEvidence -BindAddress $assignedIPv4 `
    -Archive $candidateZip -ArchiveSha256 $reviewedDigest -RustProbe $originalProbe `
    -SuiteManifest $suiteProvision -InstalledManifest $installedProvision
```

This mode installs the archive into a new directory. It verifies both
manifests against the running Medium user's SID and session, matches the
provisioned runners to the installed package, and checks that workspaces are
empty and distinct before running the suite. Setup exit codes remain null
because provisioning took place separately. The evidence includes the verified
manifest and executable hashes. `GATES_PASSED` describes the two test commands
for this combination of source checkout and archive. Technical qualification
also requires evidence that both used the intended candidate executor and policy.

The checked-in `scripts/test-windows-process-tree.ps1 -EvidenceRoot <new-dir>
-BindAddress <assigned-nonloopback-ip>` automates package build, fresh install,
provisioning and the two acceptance stages. Provisioning requires an already
elevated token in the intended nonzero interactive session. Native tests and the
installed CLI then run as the same user's verified Medium token, with no UAC
prompt, inherited token handle or inherited Job handle. If the normal linked
host logon cannot be established, acceptance fails explicitly. When necessary,
the launcher reads only the same user's pinned system Explorer token and checks
its exact SID, session and linked-logon identity before duplication; it never
changes privileges. An owner-held outer kill-on-close Job drains the trusted
test-host tree, independently of each actual sandbox Job and its unchanged
limits. Product and diagnostic source builds use separate output directories.
The installed CLI runs on a second fresh provision even when the mandatory
suite fails. Both stages must pass. The CI artifact includes their exit codes
and the tested archive's SHA-256 in `qualification-result.json`. The original
workspace retains any crash receipt.

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

The host-death acceptance case runs last and preserves this receipt. Stop all
hosts using the provision and verify the disposition of every process belonging
to its exact user SID. A crash after the temporary package-directory grant can
also leave the writer ACE in place. Schema 5 checks for that condition on
admission. Retain the receipt and use a new provision if the original private
namespace descriptor and its restoration cannot be verified. The private temp
and workspace retain their build outputs and caches.

The reference architecture is the pinned OpenAI Codex implementation:
[trusted account runner](https://github.com/openai/codex/blob/95dcd1e8378021080bf380e42587ef891f611897/codex-rs/windows-sandbox-rs/src/elevated/runner_client.rs),
[account-scoped WFP setup](https://github.com/openai/codex/blob/95dcd1e8378021080bf380e42587ef891f611897/codex-rs/windows-sandbox-rs/src/wfp.rs).
The schema-4 DNS test reached its witness through the Windows DNS service
with all account WFP filters installed. Schema 5 also requires a verified
zero-capability AppContainer and a passing three-generation DNS test.
