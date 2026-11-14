# Windows process-tree qualification — blocked

The proposed host-selected coding process-tree profile remains **disabled and
unimplemented**. No Windows process-tree configuration, approval migration or
Doctor capability is installed. The product still exposes its existing
single-process LPAC backend. This investigation precedes runtime changes, on
`feat/windows-child-process` based on `8cbefc0`.

## Native target and prior evidence

The qualification target on 14 November 2026 is Windows 11 Pro 25H2 x64,
**26200.9974**, native Rust/Cargo **1.98.1**, Visual Studio **18 Community**.
Tests use fresh local NTFS directories and separate Cargo target directories.
WSL transports commands only; the executables and isolation APIs are native
Windows. No existing workspace ACL, host service, account or feature is changed.

The user-referenced [investigation at ca8d664](https://github.com/philipbodenbach/werkWorker/blob/ca8d6647cf8bcdea063928ee64aaefcf7c1182b1/docs/windows-child-reproducer.md)
already rejects two smaller approaches:

* Zero-capability LPAC child creation fails at the SideBySide registry open.
  Adding `registryRead` permits child creation but also exposes host-user
  registry keys. It does not preserve the existing authority.
* The restricted-token/PSEC experiments identify cross-Job process access,
  filesystem alias problems and, even under the narrower single-tree contract,
  implicit console helpers exceeding a finite Job process budget. An explicit
  `conhost.exe` file deny does not prevent its implicit creation.

These are rejection evidence, not implementations to restore wholesale. The
standalone PSEC reproducer is recovered from that commit and extended with
atomic Job-list controls, full limit readback and build provenance.

## Smallest supported MXC candidate

The supported public MXC candidate is `v1::Containment::ProcessContainer`.
Microsoft's [version support table](https://github.com/microsoft/mxc/blob/6a89e69c72104bdf83b1f682a2f576b529b8bd65/docs/backends/process-container/os-version-support.md)
lists Windows 11 24H2 **26100.9278** and 25H2 **26200.9278** for process
isolation. The target is newer, but version matching alone is not qualification.
The published SDK is pinned to **mxc-sdk 1.0.1**, without default features, in
an independent unpublished workspace; the product's dependencies/MSRV are unchanged.

The native supported-v1 discovery reports `tier: "base-container"`,
`needsDaclAugmentation: false`, no warnings and no error for explicit synthetic
runtime/work/scratch paths, a volume-root deny, denied networking and disabled
UI. This is **prelaunch selection only**, as required by the
[public probe contract](https://learn.microsoft.com/en-us/windows/apps/develop/mxc/platform-support).
It does not prove the isolation of an executed workload. The machine-readable
report explicitly says `probe_only: true`, `workload_launched: false` and
`sandbox_qualified: false`. A fallback tier, DACL augmentation, warning or error
rejects the prerequisite check; it cannot silently qualify a weaker backend.

The supported [configuration](https://github.com/microsoft/mxc/blob/fda29ac63bc306b7f97f3b86b4b63d44f7f40cf2/src/mxc-sdk/src/configs/process_container.rs)
and [spawn options](https://github.com/microsoft/mxc/blob/fda29ac63bc306b7f97f3b86b4b63d44f7f40cf2/src/mxc-sdk/src/options.rs)
do not expose a caller-owned resource Job, finite aggregate process/CPU/memory
limits, a before-resume inspection hook, or confirmed whole-Job cleanup.
ProcessContainer also lacks the public stateful container lifecycle. Consequently
there is no supported integration here that establishes all required guarantees.
No SDK workload is launched and no effective **MXC workload** isolation is claimed.

The separate native experiment calls the PSEC API directly with the pinned SDK's
generated schema through its **unstable compatibility exports**. Its actual PSEC
creation and restricted payload observations must not be presented as proof that
the supported MXC dispatcher launched that tier, or as a supported product API.

## Reproduce the rejection

From a complete checkout on native x64 Windows with the installed MSVC/Rust
toolchains and cached dependencies:

```powershell
powershell.exe -NoProfile -File scripts/probe-windows-mxc.ps1 -Offline
powershell.exe -NoProfile -File scripts/reproduce-windows-psec-budget.ps1 -Offline
```

Each command creates and retains a fresh lab with source snapshots, logs and
SHA-256 manifests. Neither command executes an arbitrary requested program.
The discovery command returns 0 only for its strict prerequisite result; it
does not return a security qualification. The PSEC diagnostic returns **1**
for a measured containment violation, **2** for incomplete evidence or cleanup,
and 0 only for absence of a violation in these particular cases.

The PSEC diagnostic first establishes atomic Job assignment without PSEC using
the same suspended fixture and stdio-only handle list. That unrestricted control
is terminated without executing its payload. The PSEC atomic case checks actual
membership before resume and never executes the payload if membership is absent.
Separate, explicitly labeled diagnostic cases assign the root while suspended
to measure the historical budget failure; this is not a product fallback.

Budget observations query the exact owned Job and its configured limits. The
host pins each reported member with query/synchronize handles, checks membership
and aliveness, identifies the image and retains those handles through cleanup.
Payloads wait on a host-controlled stdin gate during measurement. The host
verifies kill-on-close, finite process/memory/user-CPU limits and absence of both
breakaway flags. Cleanup requires a signaled root, zero active Job members and
all observed member handles signaled; uncertainty stops subsequent cases.

Requested file/network policy is not itself enforcement evidence. These cases
measure the helper's direct file denial, Win32k mitigation, AppContainer and
non-elevated token flags, Job membership/counts and their own cleanup. They do
not establish all descendant file/registry/network/IPC boundaries, exact LPAC
status, host-death cleanup or an actual product coding invocation.

### Fresh native result

The final controlled run returns **1** with
`atomic_assignment_ignored=1 budget_exceeded=1 incomplete=0`.

| Native case | Observed result |
| --- | --- |
| Same suspended launch without PSEC, atomic `JOB_LIST` | Actual membership true; no payload resumed |
| PSEC plus atomic `JOB_LIST` | Creation and membership query succeed, membership **false**; no payload resumed |
| Explicit suspended assignment: detached root, cap 1 | 1 active member |
| Explicit suspended assignment: headless root, cap 1 | **2** active members, including `conhost.exe` |
| Explicit suspended assignment: detached root + headless child, cap 2 | **3** active members, including `conhost.exe` |
| Same tree with explicit `conhost.exe` image deny, cap 2 | **3** active members; direct read/execute denied with Win32 5 |

All six Jobs/cases confirm cleanup, including the atomic positive control and
the PSEC process that never resumed. The observation does not establish why
PSEC omits the requested atomic Job assignment. The explicit-assignment cases
independently reproduce the promised process-budget failure, so changing only
the assignment mechanism is insufficient.

The [native log](evidence/windows-process-tree-2026-11-14/psec-native.log),
[MXC discovery report](evidence/windows-process-tree-2026-11-14/mxc-discovery.json)
and [source/binary hashes](evidence/windows-process-tree-2026-11-14/manifest.json)
are retained with the patch. Full source snapshots, build logs and diagnostic
executables are retained locally in
`/home/philipbodenbach/werkWorker-windows-process-tree-evidence-20261114/`.
The earlier atomic-only run returned 2 because it lacked the positive control;
it remains diagnostic history and is not the final qualification result.

An independent [cleanup fault control](evidence/windows-process-tree-2026-11-14/cleanup-control.log)
deliberately creates a suspended root outside the owned Job. The reviewed
reproducer reclaims it using only its retained creation handle, confirms the
handle signaled and keeps cleanup marked failed. No payload is resumed and no
PID is reopened. The [native harness source](evidence/windows-process-tree-2026-11-14/cleanup-control.cpp)
and hashes are retained; fallback reclamation cannot turn a containment failure
into a pass.

## Product guarantees still required before integration

The existing runtime retains atomic Job assignment, its explicit stdio handle
list, kill-on-close, a non-breakaway Job and an effective cap of one. A passing
single-process suite does not prove these stronger process-tree requirements:

| Gate | Status |
| --- | --- |
| Supported API with all required Job controls and actual effective MXC workload isolation | Not established; public API lacks required controls |
| Atomic Job assignment before execution | PSEC member=false; non-PSEC positive control member=true |
| Actual finite aggregate process budget, including implicit helpers | Fresh PSEC rejection: 2 at cap 1; 3 at cap 2, even with helper image deny |
| Parent → child → grandchild through `process.run` | Not run; no admitted tree backend |
| `cargo test --offline` inside the sandbox with explicitly provisioned Rust/MSVC | Not run; host Cargo checks are not this acceptance test |
| Tree `process.start` cancellation | Not offered; current single-process cancellation remains tested |
| Tree exit, timeout, cancellation, dropped caller and host-death cleanup | Not qualified |
| Workspace lease retained until confirmed tree cleanup | Not qualified; implementation work remains |

The cleanup audit identifies concrete work for any future admitted candidate:
`Child::drop` currently stops waiting when a Job query fails; the native child
does not own the workspace lease. `JobManager::cleanup` removes jobs before
awaiting, so cancellation can lose the wait ownership, and a panicked join can
skip later joins. Repeated/concurrent `Engine::close` can report completion after
the closed flag is set but before drainage. Anonymous-pipe tasks also need bounded,
cancellation-safe drainage. These paths must retain ownership and the lease on
uncertain cleanup before a tree profile can be offered. Earlier cleanup work in
`ca8d664` is review material, not automatically accepted code.

After those gates pass, the profile must be selected only by trusted host
`NativeSandboxConfig`, bound into the full prepared execution descriptor and
rechecked before dispatch. Doctor must distinguish startup discovery from tested
tree capabilities. Until then, configuration/approvals/Doctor continue to describe
only the existing LPAC mode; a larger requested process ceiling does not enable
children. The `windows-2022` single-process CI job and its tests remain unchanged.

## Validation scope

A fresh native Windows baseline build and all existing native integration
targets pass: **35 integration tests**, **2 parallel/lease tests** and **1 CLI
configuration test**. Four ignored tests are existing subprocess payload fixtures,
not skipped acceptance gates. Native no-default-features library, external SDK
consumer and all-target Clippy with warnings denied also pass, offline.

Native debug and `x86_64-pc-windows-msvc` release builds pass, including both
binary version checks. The final packaging-only manifest was copied to the
native checkout before the final release build; all 126 runtime/test/SDK/CI
source files remain byte-identical. The
[command results](evidence/windows-process-tree-2026-11-14/native-validation-summary.json)
and [source equivalence](evidence/windows-process-tree-2026-11-14/final-source-equivalence.json)
are included. The release ZIP is retained at
`/home/philipbodenbach/werkWorker-windows-native-validation-20261114-15246469/werk-worker-windows-x86_64.zip`,
SHA-256 `965e16f1b708ff3c9e79d4ab52cb198f4b3988b65e4d5e7fdd6945c322f05224`.
It contains the existing single-process product, not a process-tree backend.

The PSEC C++ diagnostic compiles with MSVC `/W4 /WX`; the supported MXC probe
builds offline and its three invalid-input controls return 2. The standalone
encoder roundtrip test and Clippy pass. Eight existing CI-orchestration tests,
format/diff checks and the crate-file-list check pass; neither standalone
diagnostic nor its nested build cache is shipped in the product crate.

The baseline's coding fixture uses `process.run` with a precompiled expression
checker. It does not run Cargo inside the sandbox. Its host-death test checks a
ready marker and absence of a delayed write, not retained handles for every tree
member. The existing native `build-windows.ps1` wrapper additionally requires
Python 3.11; this host has Python 3.10, so native Cargo/MSVC builds and Rust tests
are recorded separately from that wrapper. No Windows Server 2022 execution or
green hosted CI result is inferred from this Windows 11 run.
