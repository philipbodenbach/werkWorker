# Native workspace and process isolation

Implementation baseline: `d44266552d053fbaec8e80a37581e880a9e10135`, clean
`feat/expand-sandbox` checkout. No version, release, commit or remote repository
operation is part of this change.

The implementation keeps `runtime::Workspace`, `Sandbox`, `ProcessLimits`,
`ProcessOutput`, `ProcessJob` and `JobManager` as the shared entry points.
CLI/TUI, the public Rust SDK and JSONL still use one engine and tool registry.
HostPolicy authorizes an operation; an operating-system backend must separately
be able to enforce its execution contract. Approvals never enable a missing
backend. Browser isolation remains a separate capability.

## Implementation steps

1. Preserve Linux descriptor access and Bubblewrap; add native workspace modules.
2. Add explicit macOS and Windows process backends with bounded output and cleanup.
3. Bind actual backend contracts to prepared invocations and reusable approvals.
4. Use object identities for protected paths, workspace leases and saved policy.
5. Extend the existing three CI jobs with native conformance tests and document
   actual results separately from configured or cross-compiled tests.

## Capabilities and requirements

| Capability | Linux | macOS Apple Silicon | Windows x86_64 |
|---|---|---|---|
| Workspace files | `openat2`, no symlinks/foreign mounts | Descriptor-relative local APFS/HFS+ | Handle-relative fixed-drive local NTFS |
| Durable saved authority / session reopen | Validated ext4 semantics plus immutable `statx` birth time and device/inode | Unavailable; fresh live sessions only | NTFS allocation-sequence file identity |
| Process isolation | Bubblewrap namespaces | Explicitly selected Seatbelt single-process mode, macOS 15+; authenticated system task-access exception | LPAC single-process mode plus Job Object; explicitly provisioned workspace |
| Workspace exposed to process | Inode-bound `/workspace` mount | Native path filter and pinned CWD; external rename assumption | Native path, pinned ancestor handles and dedicated ACLs |
| Read-only mode | Read-only mount | Seatbelt write denial | Separate read-only AppContainer SID |
| Direct network | Isolated network namespace | Denied by profile | No network capabilities |
| Child processes | Same namespace; teardown kills descendants | Fork/spawn denied; exec retains restrictions | Explicit child-creation denial; Job process cap 1 |
| Host process exit | Bubblewrap parent-death/PID namespace teardown | Independent supervisor; not kernel kill-on-close | Noninherited kill-on-close Job Object |
| One-shot browser check | Existing separate Linux backend | Unavailable | Unavailable |
| Persistent browser | Existing private network/proxy backend | Unavailable | Unavailable |

Implementation is not evidence of native conformance. The macOS mode has a
narrower path, IPC and lifecycle contract than the Linux backend and does not
provide VM isolation.
See the validation record below before treating a capability as verified.

## Native workspace access

All file tools, discovery, project rules/profiles, verification reads and
browser workspace reads use the pinned Workspace interface. On Windows,
root identity includes the volume and file object identity, not a path hash.
Directory handles deny delete sharing and retain the ancestor chain. Reparse
points, alternate streams, device/UNC/drive-relative paths, ambiguous trailing
spaces/dots and reserved names are rejected. Unsupported filesystems and
case-sensitive NTFS directories fail closed.

macOS walks from pinned directory descriptors with `O_NOFOLLOW`, checks
device boundaries and verifies directory edges. Linux keeps its existing
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV` behavior. Atomic writes
retain create-only and expected-hash checks and remove temporary write files
on failure. Atomic publication is not a transaction against arbitrary external
writers. A pinned object does not become a different object because its path
was renamed; native process path filters have the additional limitation below.

Workspace leases compare root and ancestor object identities. Protected
store/artifact paths are checked by opening their existing ancestor objects.
Saved CLI policy uses a versioned object-identity key. Legacy path-keyed files
are not automatically imported, and a replacement directory cannot inherit
the old directory's permissions.

Live object identity and durable identity serve different purposes. Linux
`dev:ino` alone can be reused immediately after deletion. Durable Linux identity
therefore includes descriptor-derived `statx` birth time, which ordinary writes
cannot change. Durable reuse is currently limited to ext4 semantics with this
field available; unknown/coarse creation-time formats are not accepted.
Other Linux filesystems retain live file/process support
but reject saved-authority reuse and persisted session reopening. Existing
sessions with legacy Linux `dev:ino` identities require a fresh session; there
is no automatic permissions migration. Windows keeps the NTFS sequence-bearing
file ID, excluding mutable CreationTime from lease decisions.

macOS currently has no validated durable incarnation proof available to the
unprivileged host: its exposed generation number is zero, and creation time is
writable. New live sessions work, but reopening a persisted session and saving
workspace permissions fail with `workspace_identity_unavailable`. Existing
saved workspace permissions are not imported. Choose session-only permissions
and **Do not save** during connection setup. Live process approvals additionally
bind an allocation nonce, so they cannot be replayed into a later allocation
that happens to reuse an inode. This is an explicit remaining platform gap.
See Apple's [filesystem attribute contract](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/getattrlist.2.html)
and [file identifier persistence restriction](https://developer.apple.com/documentation/foundation/urlresourcevalues/fileidentifier),
and Linux [inode creation time](https://man7.org/linux/man-pages/man7/inode.7.html)
and [`statx` availability](https://man7.org/linux/man-pages/man2/statx.2.html).

The CLI retains the selected Workspace while loading policy and creating the
session. Saving defaults checks the session identity and writes project data
through the same pinned object. SDK hosts can use the additive
`create_session_in_workspace` method for this admission pattern.
Linux/macOS leases coordinate engine-owned operations, not arbitrary external
directory moves during a run; retained Windows ancestor handles prevent those
moves. External writers are not participants in the cooperative lease protocol.

## macOS: explicit constrained mode

The host must supply a native configuration file explicitly:

```json
{
  "macos_path_based_workspace": true,
  "macos_system_task_access": true
}
```

```sh
ww --native-sandbox-config /trusted/native-sandbox.json sandbox doctor
```

The SDK equivalent is `Sandbox { native: NativeSandboxConfig {
macos_path_based_workspace: true, macos_system_task_access: true },
..Default::default() }`. Both options default to false and are host-owned;
workspace/project preferences cannot enable them. The first accepts the external
rename assumption. The second accepts the authenticated system task-access port
exception described below. A legacy configuration containing only the first
option is rejected before payload execution. Secure native file operations
remain independently usable.

The mode uses the system `/usr/bin/sandbox-exec` with a fixed deny-default
profile and separately supplied path parameters. It grants only the selected
workspace, private temporary storage and listed read-only system/runtime
resources. The process sees native CWD and temporary paths; argv, script text
and stdin are never rewritten. No blanket home or user-cache access is added.
Fork and spawn are denied; a program may exec while retaining its restrictions.
Ordinary compilers/build tools that require subprocesses are therefore not
generally supported by this constrained mode.

An independent trusted supervisor owns the single untrusted child, applies
limits before exec, observes cancellation/host exit and reaps before reporting
completion. Unexpected supervisor failure is not covered by a kernel
kill-on-close guarantee. Applying RLIMIT_AS can fail if the embedding process
already has a larger virtual address space; failure prevents execution.

Seatbelt subtree filters bind paths, not directory objects. Pinned descriptors
and monitoring cannot make arbitrary external root/ancestor renames atomic.
The opt-in is suitable only when external writers do not replace that
namespace. It must not be represented as protection against hostile external
renames or as general macOS sandbox parity.

`sandbox-exec` is deprecated and its profile language is not a supported
application API. App Sandbox entitlements are a different mechanism. Native
tests on the supported macOS version are mandatory; no SIP or global protection
setting is changed. Root/set-id hosts remain rejected. macOS backend
configuration version **7** checks the actual inherited host port instead of
requiring a global SIP setting: `task_get_special_port(TASK_HOST_PORT)` obtains
the right and `mach_port_kobject` queries its kernel object type through the
task port. Only `IKOT_HOST` (3), the ordinary unprivileged kernel host port, is
accepted. User endpoints (`IKOT_NONE`), privileged host ports (`IKOT_HOST_PRIV`),
missing/dead rights and query failures reject execution. No message is sent to
the potentially hostile host endpoint. The copied send right is released.

This check runs during preparation/doctor and again in the supervisor's fork
snapshot, before creating the untrusted child. XNU creates a fresh IPC namespace
and copies selected special ports; the BSD fork credential update then replaces
the host slot with the kernel host port for the child's UID. A child atfork hook
can change it again, so the supervisor still checks its actual slot. Changing
the embedding host's port later cannot replace that snapshot. See Apple's
[port inheritance](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/osfmk/kern/ipc_tt.c),
[fork credential update](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/bsd/kern/kern_fork.c),
[host-port reset](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/osfmk/kern/task.c),
[kernel object query](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/osfmk/ipc/mach_debug.c)
and [object types](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/osfmk/kern/ipc_kobject.h).
The query returns the object type on release kernels too; the backend does not
use the kernel address, which release kernels hide. Port failures retain the
Mach result or unexpected object type and identify the failing stage.
Version 1–6 approval descriptions differ and must be prepared and approved
again. The file-content/network rules and one-process limit are unchanged.

The task-access exception is exactly the system `com.apple.taskgated` endpoint
in `TASK_ACCESS_PORT` (selector 9). macOS does not permit revoking its live slot,
and the Seatbelt getter rule does not reliably prevent copying out its send
right. Version 7 therefore permits that authenticated right to remain readable
and usable by the payload. It does **not** promise complete host IPC denial or
equivalence to a separate VM. Workloads requiring that stronger boundary must
use a different execution environment; no VM backend is supplied by this mode.

Before accepting the exception, the launcher obtains the candidate right from
the kernel and independently looks up the fixed service name through the
privileged bootstrap protocol. The reply must carry a kernel-supplied Mach
audit trailer identifying launchd (PID 1, effective UID 0), and its recipient
PID and process incarnation must match the caller. The returned service right
must equal the candidate in the same Mach namespace. A live send right or
`IKOT_NONE` alone is insufficient evidence. No authentication message is sent
to the candidate task-access endpoint. Missing/dead rights, lookup failure or
timeout, malformed replies, untrusted reply identities and mismatched endpoints
reject execution. A bootstrap right identical to the candidate is rejected
before sending. Failed sends and partial receives release returned Mach rights;
interruptions fail closed instead of restarting the timeout.

The bounded lookup uses stack buffers and Mach calls, rather than XPC objects,
so the check also runs on the final fork snapshot before untrusted execution.
The legacy bootstrap wire interface is private: an incompatible OS reply fails
closed rather than bypassing authentication. Apple's
[bootstrap lookup implementation](https://github.com/apple-oss-distributions/launchd/blob/main/liblaunch/libbootstrap.c)
describes privileged lookup authentication; the backend additionally requires
launchd's PID and the expected reply recipient. Approval descriptions record
the exact service exception and authentication method, so earlier approvals
cannot silently authorize it.

After authentication, the child clears registered ports and verifies that its
bootstrap and exception slots are empty. Only populated slots need mutation;
the complete exception mask is checked again after clearing. Exec discards
ordinary copied-out Mach names while preserving the selected special slots and
the Seatbelt policy. The system host-port check and explicit special-port setter
denial remain in place. Native tests retain an outside live-right control,
verify the allowed access right survives exec, require the setter's exact
`KERN_DENIED`, and separately test rejection of untrusted authentication inputs.
See XNU's [special-port handling and namespace initialization](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/osfmk/kern/ipc_tt.c)
and [exec task creation](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/bsd/kern/kern_exec.c).
Version 4 additionally permits metadata on the system symlinks `/var` and `/tmp`
and the exact ancestors of the workspace and private temporary directory.
XNU checks [symlink reads during path lookup](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/bsd/vfs/vfs_lookup.c)
before it can reach the already permitted canonical workspace. Apple's WebKit
profile likewise permits metadata on those two aliases. Neither their contents
nor sibling directories become readable. Version 5 permits `file-read-data` on
the literal root directory `/` for dyld/libignition startup. This exposes only
root directory entry names, not descendant file contents or writes; it is not a
`subpath "/"` grant. The execution description records this permission and
invalidates earlier approvals. Version 6 also permits the exact read-only sysctl
`hw.pagesize_compat`. Apple's libc uses the numeric `HW_PAGESIZE` selector for
[`getpagesize()`](https://github.com/apple-oss-distributions/Libc/blob/main/gen/FreeBSD/getpagesize.c),
which XNU registers as `pagesize_compat` in
[`kern_mib.c`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_mib.c).
Allowing only the separate named `hw.pagesize` sysctl does not cover that call.
Without this permission, `sysconf(_SC_PAGESIZE)` fails with `EPERM` and Rust's
guard-page initialization can abort before `main`. The profile hash and version
invalidate earlier approvals; the added rule grants no sysctl writes or wider
sysctl reads.
On abrupt host
exit, private temporary files may remain even after the supervisor reaps the child.
See [Apple DTS on sandbox-exec](https://developer.apple.com/forums/thread/661939)
and [XNU process event definitions](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/event.h).

## Windows: dedicated workspace provisioning

An explicit host operation creates a **new, empty** local NTFS workspace:

```powershell
ww sandbox create-workspace C:\Work\worker-project
ww --workspace C:\Work\worker-project sandbox doctor
```

The SDK exposes `Sandbox::create_native_workspace(path)`. Existing directories
are rejected. The host can then place the intended project in this directory;
there is no hidden execution copy, automatic sync-back or recursively modified
user workspace. Files and processes operate on the same selected directory.
Process execution checks the dedicated ACL policy before launching.

Separate workspace-derived AppContainer identities provide read-only and
writable modes. The child is LPAC, opts out of All Application Packages,
receives no network capabilities and is atomically assigned to a restricted
Job Object through process-creation attributes before user code can run.
The job handle is never inherited; breakaway is not allowed. Startup verifies
the suspended token before resuming execution.
Only explicit stdin/stdout/stderr
handles are inherited. The environment contains private profile/temp paths and
the explicitly allowed Windows runtime values, not host credentials.

The zero-capability LPAC could not start child processes on the native validation
host. This backend therefore explicitly forbids child creation before launch
and sets a Job process cap of one, including ordinary, detached and breakaway
attempts. It supports native single-process tools, not ordinary multiprocess
compilers/build systems. A larger requested process ceiling does not enable
children; the effective cap of one is part of the approval descriptor.

The dedicated-account Windows coding process-tree profile is implemented
separately and disabled by default. See [provisioning and native acceptance](windows-process-tree-provisioning.md)
for the current path, and the historical [candidate qualification](windows-process-tree-qualification.md)
for the LPAC/PSEC rejection evidence and MXC tier-discovery limits.

The Job Object enforces job memory, job CPU and process-count limits;
wall time and output are bounded by the host. Linux-specific per-file size and
descriptor-count rlimits are not claimed on Windows. LPAC access to operating
system resources follows their Windows ACL baseline, including any host object
already granted to All Restricted Application Packages. It is not a path-deny
overlay for unusually permissive host ACLs. No new broad host ACL grant is added.
The backend supports
system executables and programs in the dedicated workspace; a developer's
entire profile or toolchain cache is not automatically exposed.

AppContainer profiles persist for that workspace and mode. After all engines
and jobs are closed, the host can explicitly remove them with
`ww sandbox remove-workspace-profiles PATH` or
`Sandbox::remove_native_workspace_profiles(&workspace)`. This is not an
authorization revocation: it does not invalidate live tokens, remove workspace
SID grants or prove future execution impossible. It removes registered profile
state/storage and does not delete project files. Use the host policy to deny
future process actions and close active engines before cleanup. The directory's
dedicated ACL is not silently restored to a different permission policy.
Windows manages temporary storage inside the AppContainer profile. It is shared
by that workspace/mode and persists until explicit profile removal; it is not
an ephemeral per-invocation directory. Job cleanup terminates processes but does
not automatically remove this retained private data.

## Prepared calls, recovery and diagnostics

Prepared process arguments include contract version 2, backend configuration,
workspace object identity, write access, runtime/temp resources, network/IPC
mode and required limits. These fields participate in the existing approval
hash. Immediately before dispatch the trusted context is described again;
missing legacy contracts or changed authority are rejected. Preparation does
not run untrusted programs or modify ACLs.

Linux launcher configuration requires a trusted absolute Bubblewrap path; bare
PATH lookup is rejected. Its canonical path and digest are part of preparation
and checked again before dispatch. The host must protect its installed launcher
from concurrent replacement; this is not a filesystem transaction against a
host administrator replacing the executable during dispatch.

Reusable process approvals now bind the **complete prepared arguments**,
including argv, stdin and sandbox contract. Old unscoped process approvals are
not accepted. File-tool workspace approvals retain their separate semantics.
Explicit host hard denies, child restrictions and revocation still apply.
Unknown-outcome recovery never automatically replays a process.

`Sandbox::doctor()` and `WorkerEngine::sandbox_doctor()` return a capability
report even when only file operations are available. Each entry distinguishes
implementation, host availability and active probe results. The report lists
which checks were actually performed; a startup probe is not proof of every
isolation property. Browser entries are not actively probed by Doctor.
Doctor normally uses private synthetic workspaces. The optional dedicated-account
Windows coding profile uses its explicitly provisioned workspace for startup;
see [provisioning and acceptance](windows-process-tree-provisioning.md).
The selected real workspace is checked
by session admission and process preparation, not by this generic host probe.
Callers requiring process startup can use `require_process_backend()`.

`Sandbox` gains the public `native` configuration field. Existing explicit
struct literals should use `..Default::default()` or supply
`NativeSandboxConfig::default()`. Existing import paths, ProcessLimits and
ProcessOutput remain. The optional Windows coding profile builds separate
`ww-windows-provision` and `ww-windows-runner` helpers; admin setup is explicit.
Library-only builds still do not require the TUI feature.

CLI hosts may load `ProcessLimits` through `--process-limits /trusted/limits.json`,
separately from agent `--limits` budgets. Unknown fields and zero mandatory
limits fail closed. Limits are included in approvals and are not adjusted to
make a program start. In particular, Darwin virtual mappings can exceed the
default 1 GiB `memory_bytes`; setrlimit failure prevents execution. Native tests
use an explicit finite 1 TiB address-space bound, not a claim of resident-memory
containment. SDK hosts set the existing `Sandbox.limits` field directly.

## Validation record

The existing CI jobs run native workspace, approval-contract and platform
tests; macOS and Windows additionally run their actual process backends.
`REQUIRE_SANDBOX=1` is required. A failed native backend is not turned into a
successful skip. Dedicated fixtures use synthetic secrets and host-side
positive controls before checking denial.

Native CI collects all selected integration targets with `--no-fail-fast` and
retains failures across separate Cargo commands. SDK/lint checks also run after
an earlier test failure unless the job is cancelled. The macOS job prints its
architecture, user identity and `csrutil status`; no step changes SIP settings.
Both native check steps use GitHub's `shell: python` and start Cargo directly
with argument lists, preserving the toolchain environment. Git Bash on Windows
can prepend its own `Git\usr\bin\link.exe`, which is a file-link utility rather
than the MSVC linker; a native shell comparison reproduced the resulting
`extra operand` link failure. The same Rust source compiled and ran through
native Python with the MSVC linker. Tests of failure collection alone do not
validate that linker selection. See GitHub's
[shell execution contract](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepsshell).
After this correction, both Python blocks were extracted unchanged from the
workflow and executed natively after cleaning the product's own build outputs.
All 35 Windows integration tests, two parallelism tests, the CLI test, Clippy,
no-default library and external SDK checks passed. This used Windows 11 build
26200, Rust 1.98.1 and Python 3.10.6; it is not a rerun of the hosted Server 2022
job with Python 3.12.

The [2026-11-12 CI run for `60ef7c6`](https://github.com/philipbodenbach/werkWorker/actions/runs/43179493598)
exposed two separate failures. Windows stopped at a license-header assertion
that required LF checkout bytes; the check now compares the first logical line
and accepts both LF and CRLF. A native Windows 11 CRLF reproduction failed
before the correction and passed afterwards; all 35 Windows integration tests
then passed, with four existing subprocess fixtures invoked by their parent
tests. This does not claim a new Server 2022 CI result.

macOS 15.8.2 ARM64 rejected the combined UID/SIP requirement before sandbox
execution. The first log did not identify which condition failed. The subsequent
native run with the expanded diagnostics establishes normal UID/EUID 501 and
GID/EGID 20, but an active CSR mask of `0x0000006f` and successful
`csr_check(CSR_ALLOW_KERNEL_DEBUGGER)` (result 0, errno 0). The debugger bit is
set and its effective permission is granted, so this host lacks the required
protection against custom Mach host ports required by configuration version 1.
That gate prevented all process conformance tests from reaching Seatbelt.

Configuration version 2 replaced that gate with the per-process kernel object
check described above. `macos_process` includes a positive control sending a
real message through the isolated parent's injected custom host port, followed
by rejection tests before launch and after the host preflight (using a child
atfork hook). Neither may execute the payload's marker write. The normal kernel
port must also permit a successful isolated invocation. CI sets
`WERK_REQUIRE_CUSTOM_MACH_HOST_PORT=1` on macOS, requiring actual injection on the
SIP-relaxed hosted runner. A SIP-protected developer Mac instead checks the
kernel's `KERN_NO_ACCESS` denial and successful normal execution. No CI test is
skipped and no SIP setting is modified. Existing file, network, IPC, fork,
timeout, cancellation and host-death tests remain mandatory.

The subsequent native CI log for `7edc428` exposed a selector error in version
2: the implementation and fixtures used `3` (`TASK_NAME_PORT`) where
`TASK_HOST_PORT` is `2`. This explains both object type `20` (`IKOT_TASK_NAME`)
and injection status `4` (`KERN_INVALID_ARGUMENT`). The selector is corrected in
both places; `IKOT_HOST = 3` remains the required kernel object type. See Apple's
[special-port selectors](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/osfmk/mach/task_special_ports.h).
The existing native library-test command now also compiles static assertions
against the runner's actual SDK. Runtime and fixture checks compare the selected
right with the independent `mach_host_self()` trap. Locally, compiling against
Apple's published header accepts the correction and rejects the old selector.

Linux tests and an Apple-target compile-only extraction cannot establish full
native sandbox acceptance; a passing
`macos-15` integration run remains required. The complete project's Apple
cross-build needs an Apple SDK/C toolchain unavailable on this Linux host.
Windows' previously fixed native Python/MSVC workflow is retained.

The subsequent native run for `e275a17` passes all three host-port/SDK unit tests
and both approval-contract tests. It exposes a separate fixture bug: BSD fork
resets the custom host slot, so sending a user-queue marker to the resulting
kernel port triggered a Mach guard and SIGKILL. The fixture now sends the marker
while its parent slot actually contains the synthetic endpoint; its ordinary
fork child only verifies the restored kernel host type. See Apple's
[Mach message policy](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/osfmk/ipc/ipc_policy.c).

That run reported only a generic Mach cleanup failure. The added per-operation
diagnostics in `60477df` established the actual blocker: stage 12 observed a
**live** inherited `TASK_ACCESS_PORT`; the separate dead-sentinel fixture's
attempted overwrite returned `KERN_NO_ACCESS` (8). This is an incompatible backend
assumption, not a transient runner failure. Another fork, plain exec or `posix_spawn` port action cannot
revoke that slot; those paths inherit it or call the same protected setter.

Version 3 removed the impossible revocation requirement and attempted to enforce
the boundary with explicit Seatbelt getter/setter denials. The invalid dead-slot
overwrite test was replaced by native positive/negative access tests. The
remaining Mach cleanup diagnostics and fork-snapshot test stayed active. This
established the cause of the setup failure, but did not establish successful
payload execution or getter enforcement. Version 7 supersedes that getter
assumption with the authenticated exception described above.

The native run of `914d313` passed all four Mach setup unit tests, but exposed
the next startup failures: argv paths under `/var/folders` returned
`sandbox-exec: execvp() ... Operation not permitted` (exit 71), while calls using
canonical workspace paths sometimes ended without an exit code. The latter
means signal termination, whose signal/reason was not retained in that log.
The previous file/network/IPC negative test falsely accepted either startup
failure as isolation because it checked only for a nonzero result.

Version 4 corrects the alias/ancestor metadata omission. The negative tests now
require both payload exit 77 and its operation-error marker; failed launch,
crash and timeout cannot pass. A mandatory startup test separately runs a system
executable, a canonical workspace executable and its `/tmp` alias. A library
test runs the exact profile directly on a fixed, host-built echo payload and
reports signal/status; on failure it compares the explicit special-port deny
rules one at a time. Those controls never replace the production result.
CI also collects narrowly filtered Sandbox denial and crash diagnostics after
a native failure. No Cryptex or IPC permissions are added without evidence.
The [native run of `592f7a7`](https://github.com/philipbodenbach/werkWorker/actions/runs/43431980307/job/131907468156)
now identifies the signal as `SIGABRT` with termination namespace `0x23`, code 2.
Apple defines namespace 35 as
[OS_REASON_LIBIGNITION](https://github.com/apple-oss-distributions/xnu/blob/xnu-11215.81.4/bsd/sys/reason.h),
which belongs to dyld startup. The exact meaning of its code 2 is not established.
The native logs repeatedly show `deny file-read-data /` immediately before the
abort, including controls with the special-port rules removed. Version 5 adds
only the literal-root directory read described above, preserving the Mach rules
and requiring the existing native startup/isolation tests to pass. No successful
native macOS run of version 5 is claimed.

Version 6 local validation on 2026-11-13 used macOS 26.6.2 / Apple Silicon and
Rust 1.99.0. The version 5 startup failure reproduced with a Rust guard-page
initialization panic. A C control measured `sysconf(_SC_PAGESIZE) = -1`, `EPERM`;
adding only `hw.pagesize_compat` changed it to 16384 and let the Rust payload run.
The existing exact-profile startup regression now passes. The lifecycle fixture
also waits for its readiness text to arrive through `process.log` before opening
the process gate or cancelling it; a file marker alone did not synchronize the
asynchronous pipe reader. An intentionally incomplete first log page exercises
that bounded observation loop.

Version 6 still failed two native isolation tests: the access-port getter test
and the host-exit fixture's inherited-port check. Both observed a live
`TASK_ACCESS_PORT` despite the explicit getter denial. Adding a self filter or a
broader Mach deny did not fix that behavior. Denying the entire getter MIG routine returns
`KERN_DENIED`, but also aborts libxpc initialization because it needs the bootstrap
selector. Clearing the live access slot returns `KERN_NO_ACCESS`; requesting that
clear through `posix_spawn` fails with `EINVAL`. No such workaround is installed.
These results disproved version 6's complete access-port denial claim; they did
not establish a sandbox escape.

Version 7 replaces that claim with a separately selected system-service
exception and authenticates the inherited endpoint before launch. Native
conformance now checks the retained right, setter denial, authentication rejection
cases and the remaining IPC cleanup boundary. The host-exit fixture must still
reach and verify actual supervisor cleanup. Authentication and test updates are
not evidence of a passing CI run; validation results must be recorded separately.

Version 7 was validated locally on 2026-11-13 on Apple Silicon macOS 26.6.2
with Rust 1.99.0. With `REQUIRE_SANDBOX=1`, all 45 required native integration
tests and all 14 macOS backend unit tests passed, including both previously
failing access-port/lifecycle cases. After the final launcher and authentication
changes, the 16 macOS process tests and four native lifecycle tests passed again.
Authentication tests cover the genuine service, arbitrary live endpoints,
null/dead/kernel ports, forged replies, bounded lookup failure, repeated
send-timeout resource cleanup, bootstrap/candidate aliasing and the final fork
snapshot. A separate native relay probe confirmed that forwarding a reply
right produces a launchd response bound to the relay's PID, which the recipient
check rejects. Formatting, all-targets Clippy with warnings denied, the
no-default-features library, separate locked SDK consumer, two parallel tests,
two CLI configuration tests and eight CI-workflow tests also passed.
The local host has SIP enabled; custom-host-port tests exercised the expected
SIP-denial branch. GitHub's macOS 15 runner, including its mandatory custom-port
injection branch, has not been rerun for version 7.

Local version 4 checks passed on 2026-11-13: 31 real Linux integration tests,
eight workflow/diagnostic tests, formatting, all-targets Clippy with warnings
denied, the no-default-features library, the separate SDK consumer and the
extracted Apple ARM64 type/Clippy checks. These checks do not execute Seatbelt.

The same CI run exposed two Windows fixture problems before their LPAC checks:
the coding control accepted access failure instead of its intended arithmetic
assertion, and the lifecycle control had been moved into the low-labeled
workspace. Windows starts ordinary processes at no higher integrity than their
executable file; see Microsoft's
[Mandatory Integrity Control documentation](https://learn.microsoft.com/en-us/windows/win32/secauthz/mandatory-integrity-control).
Host controls now use byte-verified executables outside that labeled workspace;
the real sandbox runs still use the workspace copy. No production ACL or token
changes are required. On 2026-11-13, native Windows 11/MSVC reproduced all five
failures at `592f7a7`; with the fixture correction, all 35 required native
integration tests passed. The four technical Windows payload fixtures remain
invoked by their parent tests. GitHub's Windows Server 2022 rerun is still pending.
Native Windows all-targets Clippy with warnings denied, the no-default-features
library and separate locked SDK consumer passed on Windows build 26200 with
Rust/Cargo 1.98.1. The five affected Linux integration tests also passed; the
updated macOS module passed the extracted Apple ARM64 type/Clippy check.
Before/after logs are `/tmp/ww-native-control-before.log` and
`/tmp/ww-native-control-after.log`; Windows lint/SDK results are in
`/tmp/ww-native-control-checks.log`. These Windows checks do not validate Seatbelt.

Version 3 local validation on 2026-11-13 passed: 31 Linux integration tests with
real Bubblewrap and `REQUIRE_SANDBOX=1`, three CI-workflow tests, formatting,
all-targets Clippy with warnings denied, no-default-features library and the
separate locked SDK consumer. The extracted Apple ARM64 source harness also
passed Clippy with warnings denied, and the payload compiled to Apple ARM64
metadata. These cross-target checks do not run Seatbelt or link the full macOS
application; no native macOS run of version 3 was available locally. The
existing mandatory CI command already includes the new integration test.

Validation completed locally on 2026-11-10 against the uncommitted checkout.
Linux used Rust 1.98.0 on WSL2 kernel 6.18.58.1 with a real Bubblewrap backend.
Windows used native MSVC x64 execution on Windows build 26200 (Windows 11),
Rust/Cargo 1.98.1; WSL interop launched the native tools but was not the sandbox.
The declared Rust 1.88 minimum was not separately compiled.

| Executed check | Result |
|---|---|
| `cargo fmt --check`, `cargo check --locked --all-targets` | Passed |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed on Linux and native Windows |
| `cargo test --locked --all-targets`, `REQUIRE_SANDBOX=1` | 473 test executions passed, 0 failed; 27 existing opt-in replay/live-service cases ignored |
| Real Linux isolation suite | All 9 passed, including missing-launcher rejection, host sentinel/network/resource and descendant cleanup checks |
| Chromium `browser_control` + `browser_mode`, `REQUIRE_BROWSER=1` | 23 passed; 4 opt-in external live-site probes excluded |
| Actual Chromium observation Python suite | 14 passed, no skip |
| TUI PTY smoke suite | 8/8 passed on final built CLI |
| Python tooling/installer unit suite | 105 cases, 89 passed and 16 platform/browser-gated; the 14 browser observation cases were subsequently run explicitly |
| Scripted evals and public examples | All 8 evals passed; coding, automation and SDK examples completed |
| No-default-features library and separate locked SDK consumer | Passed on Linux and native Windows |
| Native Windows integration suite | 35 required tests passed; 4 ignored helper entry points were explicitly exercised by their parent tests |
| Native Windows focused units | 4 passed (leases, argv quoting and CLI configuration) |
| Windows final CLI/tool-text follow-up | 9 integration checks and all-targets Clippy passed |
| Source crate / isolated source install / real offline archive installer | 31 checks passed; both aliases, embedded resources, active doctor, actual process exit and persisted verification receipt checked |
| Final packaged source inventory | All 52 `src` files byte-match the checkout |

The 35 Windows tests comprise contracts (3), coding (1), lifecycle (4), native
policy (9), workspace (6), platform (3), sandbox contract (2) and Windows process
isolation (7). They exercise actual NTFS handles/ACLs, LPAC tokens, Job Objects,
positive host controls, private temporary writes, denied network/IPC/handle
inheritance, resource limits and cleanup. Native Windows Server 2022 CI has not
been run locally; passing the Windows 11 run does not claim that CI result.

The Linux inode-incarnation regression observed real device/inode reuse on the
first replacement in an isolated run, a different immutable creation timestamp,
and rejection of the old SQLite session. The full suite also verifies that
ordinary content changes preserve the durable identity. Another independent
run did not force reuse within its bounded 512 attempts; it still checked each
replacement and reported that distinction.

The Apple ARM64 source check includes the actual Workspace facade, both native
macOS modules and macOS process test source in an isolated harness. Typecheck
and Clippy with warnings denied passed; the shared std-only probe also compiled
for Apple ARM64. The full cross-build lacked the Apple C compiler/SDK required
by native dependencies. **No native macOS test, macOS package installation or
complete macOS CI run was performed.** Windows archive installation was not
part of the local package smoke; the native Windows binaries and SDK were tested.

Local evidence:

- Rust/TUI/browser/Python logs: `/tmp/werkworker-native-linux-check/`.
- Installer/package summary: `/tmp/werkworker-package-release-check-br4vve02/summary.json`.
- Verified source crate: `/tmp/werkworker-package-release-check-br4vve02/verified-source-package.crate`.
- The five-member archive is a **local debug validation artifact**, not a
  published release. Its SHA256 is
  `04e4cd3f38dd8fcb0ca8551ec09c9d9a27532931bf8e6cd19ad72f0043a3d560`.

No installed user command, version, commit, remote branch or release was
changed. All implementation changes remain local and uncommitted. Later edits
to this validation narrative do not change the verified executable sources.

## Remaining acceptance gaps

The complete requested platform acceptance is **not yet satisfied**. macOS still
requires native execution evidence, cannot safely restore persisted workspace
authority with the available identity proof, and offers only the explicitly
selected path-based single-process mode. Both native process modes reject child
creation, so ordinary multiprocess toolchains remain unsupported. Native browser
isolation is unavailable. Windows retains profile/temp state until explicit
cleanup and follows existing LPAC-accessible host ACLs; macOS supervision has no
kernel kill-on-close guarantee if the supervisor itself fails. These are stated
contracts and missing capabilities, not successful parity tests.

## Changed files

The shared engine, tool registry, HostPolicy, durable event store, JobManager and
LiveOutput are reused. New modules implement native file access, native process
launch/supervision and approval-bound execution contracts. The existing CI jobs
are extended; no helper executable or second executor is introduced.

The following paths differ from the baseline (including untracked new files):

<details>
<summary>Local file inventory</summary>

```text
.github/workflows/ci.yml
Cargo.lock
Cargo.toml
README.md
docs/architecture.md
docs/authoring.md
docs/cli.md
docs/cross-platform-sandbox.md
docs/de/index.html
docs/index.html
docs/project-configuration.md
docs/releases.md
docs/sdk.md
docs/security.md
docs/subagents.md
docs/testing.md
docs/validation.md
examples/coding.rs
examples/eval.rs
examples/sdk-consumer/Cargo.lock
scripts/tests/permissions_tui_smoke.py
src/api.rs
src/browser.rs
src/browser_view.rs
src/browser_workspace.rs
src/cli_config.rs
src/cli_setup.rs
src/cli_tui.rs
src/context.rs
src/core.rs
src/main.rs
src/parallel.rs
src/policy.rs
src/project.rs
src/runtime.rs
src/runtime/macos.rs
src/runtime/sandbox_contract.rs
src/runtime/windows.rs
src/runtime/workspace_macos.rs
src/runtime/workspace_windows.rs
src/tools.rs
tests/agent_tools.rs
tests/browser_control.rs
tests/completion.rs
tests/fixtures/native_process_probe.rs
tests/harness.rs
tests/isolation.rs
tests/macos_process.rs
tests/native_coding.rs
tests/native_lifecycle.rs
tests/native_policy.rs
tests/native_workspace.rs
tests/output_recovery.rs
tests/permissions.rs
tests/platform.rs
tests/process_input.rs
tests/read_loop_recovery.rs
tests/sandbox_contract.rs
tests/subagent_models.rs
tests/subagents.rs
tests/windows_sandbox.rs
tests/work_cycle.rs
tests/working_state.rs
```

</details>
