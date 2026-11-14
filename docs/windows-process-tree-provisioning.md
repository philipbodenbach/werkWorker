# Dedicated-account Windows process-tree provisioning

The administrator helper is separate from runtime launch. It never requests UAC,
installs a service, starts a sandbox workload or selects the process-tree profile.
Run it only after reviewing its request on a native Windows host. Building the
helper or passing its read-only preflight is not native sandbox qualification.

This implementation starts at `main` (`8cbefc0`). The local PR #24 investigation
at `a2bed0a` and its earlier `ca8d664` evidence reject the LPAC capability
expansion and PSEC/MXC route; they are decision inputs, not an enabled backend.
The existing single-process LPAC mode remains the default.

## Setup request

Build `ww-windows-provision.exe` and `ww-windows-runner.exe` with the native MSVC
Rust toolchain. The setup executable accepts one JSON file:

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
cryptographically generated password. The password is stored as a DPAPI
LocalMachine ciphertext in `credential.dpapi`; its ACL allows only SYSTEM,
Administrators and the configured host SID. The plaintext is never written to a
log, command line or JSON manifest. Protected `provisioning.json` and
`invocation.lock` have the same trusted principals. The sandbox account receives
read/execute on the copied runner/toolchains and modify access on the dedicated
workspace/private temp. Existing user directories, host firewall profiles and
services are not modified.

A transaction adds a new persistent WFP sublayer and six SID-only block filters:
ALE_AUTH_CONNECT, ALE_AUTH_RECV_ACCEPT and ALE_RESOURCE_ASSIGNMENT, IPv4 and IPv6. There are no address,
protocol, port or loopback exceptions. Filters remain when a host process exits;
network restrictions are not implemented through environment variables. Actual
IPv4/IPv6, external-network, loopback, raw ICMP and DNS negative controls are still
required. SID-only CONNECT rules include the port-specific DNS/SMB matches in the
reference; RESOURCE_ASSIGNMENT also covers its raw-ICMP setup path. These rules
target traffic attributed to the sandbox account. The native suite must exercise
DNS resolution and host IPC/brokers as well as ordinary sockets before claiming
the full offline boundary. No service-traffic isolation is inferred from a
successful filter installation.

The payload uses a restricted primary token of the dedicated account, with
maximum privileges disabled and write restrictions for that account/logon.
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

Setup enables the account only after the protected receipt and filters are ready.
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

Select it with `ww.exe --native-sandbox-config C:\Reviewed\native.json` and
`--workspace C:\werkWorker\coding-workspace`. Omitting `windows_process_tree`
selects the existing LPAC backend. The SDK equivalent is
`NativeSandboxConfig { windows_process_tree: Some(WindowsProcessTreeConfig {
provisioning: manifest_path }), ..Default::default() }`. Only the trusted host
chooses this field. The profile requires workspace write authority; there is no
read-only downgrade or fallback after a provisioning error.

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
user CPU time are aggregate Job limits. Stdin is bounded to 1 MiB and each output
stream to the selected byte ceiling; output is still drained after truncation.
The full backend descriptor, manifest digest, account, explicit runtime roots,
environment and effective limits participate in the existing approval hash and
are rechecked before dispatch. `process.run`, `process.start`, `process.status`
and `process.cancel` use the same selected profile and JobManager lifecycle.

```powershell
ww.exe --native-sandbox-config C:\Reviewed\native.json `
  --process-limits C:\Reviewed\process-limits.json sandbox doctor
```

Doctor validates provisioned state and probes startup in the provisioned
workspace. Its `verified` list is deliberately limited to that probe; it does
not certify Cargo, descendants, network denial or host-death cleanup.

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
$env:REQUIRE_WINDOWS_PROCESS_TREE = '1'
cargo test --locked --offline --test windows_process_tree -- --nocapture --test-threads=1
```

The required gate fails when provisioning is missing; an optional `NOT RUN`
message is never a successful feature qualification. The harness enters through
the real Engine/tool interfaces and checks parent/child/grandchild receipts,
offline Cargo output/artifact, exact owned-Job membership/limits, bounded I/O,
synthetic file/registry/IPC/network controls, timeout, cancellation, dropped
caller, Engine close and abrupt host death. Existing single-process native tests
remain an independent required gate. Toolchain source copies and admin setup are
explicit prerequisites; a host-side Cargo build is not the sandbox acceptance.

## Cleanup and interrupted-host recovery

The trusted supervisor owns the Job, its handles, the workspace lease and the
provisioning lock until it observes a signaled root and an empty Job. It survives
the loss of its asynchronous caller. Kill-on-close handles abrupt host death;
neither launcher nor payload inherits an owned-Job handle or breakaway authority.
Before the first process is created, the host also flushes a protected
`incomplete-run.json` receipt. Only confirmed cleanup removes it. The OS releasing
a crashed host's lock therefore cannot silently admit another tree while cleanup
is uncertain: subsequent launches fail with `cleanup_required`.

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
The filter set here is an unconditional account network deny; Codex's published
WFP set complements its separate firewall policy and is not copied as a complete
offline boundary.
