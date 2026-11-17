# Windows process-tree validation — 2026-11-17

**The process-tree profile remains technically disabled.** The Rustc jobserver
failure is corrected in the backend and demonstrated through real `process.run`
from a fresh schema-4 provision. DNS broker isolation and the console process
ceiling are still unresolved. The complete mandatory acceptance has not passed;
`sandbox_unqualified` remains unconditional. No commit, push, remote PR edit or
hosted CI result is claimed for this continuation.

## Current continuation on `feat/windows-child-process`

The namespace fix provisions a non-inheriting, account-only `0x000f` grant on
the current session's `BaseNamedObjects\Restricted` directory. KernelBase needs
query, traverse, create-object **and create-subdirectory** when opening that
directory for a named semaphore. The earlier create-object-only experiments
therefore did not provide the required access. The ordinary host receives only
query/traverse/read-control (`0x00020003`) for exact launch-time verification.
The parent `BaseNamedObjects` directory is unchanged; no Everyone/Users creation
grant or sandbox WRITE_DAC/WRITE_OWNER is added. Setup serializes cooperating
provisioners, verifies owner/ACEs, and retains the original descriptor for failed
setup rollback. Ordinary launch only verifies and pins the namespace. Schema 4
binds this prerequisite and its session identity into the protected manifest and
the approval descriptor; session zero is unsupported and a new session/reboot
requires explicit fresh provisioning.

The native [Cargo result](evidence/windows-process-tree-2026-11-17-continuation/cargo-result.json)
and [complete selected-case log](evidence/windows-process-tree-2026-11-17-continuation/cargo-native.log)
record exit 0, `OFFLINE_NATIVE_CARGO_OK`, one passing dependency-free test,
the expected text artifact and a generated Windows executable. All four live
members of the concrete Job (runner, Cargo, test executable and Conhost) were
retained at the approved limit of 32; their zero-time wait results are all
signaled immediately at tool return. Cargo's harness no longer assumes exactly
three members: it verifies every actual member, all three required executable
identities and the unchanged approved ceiling. The separate full-budget console
checks remain strict. This is an explicitly identified diagnostic snapshot,
with only its separate source snapshot's qualification guard removed; the
delivered source has no bypass. These selected commands ran under an elevated
build controller with the dedicated payload token independently checked. They
are not ordinary-host or complete profile qualification. Native namespace
security units (2), provisioning units (4), final all-target Clippy with warnings
denied, formatting, the no-default-feature library and external SDK checks all
pass. All ten existing native integration targets compile; compilation is not
execution. See the [build checks](evidence/windows-process-tree-2026-11-17-continuation/namespace-build-checks.json)
and [SDK/compilation record](evidence/windows-process-tree-2026-11-17-continuation/sdk-native-compile-checks.json).

The freshly provisioned account still sends mediated DNS queries through the
Windows service despite the seven WFP blocks. Both `DnsQuery_W` and
`DnsQueryEx` return access denied with zero independent server arrivals under
full restricting thread tokens without Everyone/Restricted Code; host controls
before and after succeed. Those thread-token controls do not establish
compatible process startup or fix product isolation. See the
[account matrix](evidence/windows-process-tree-2026-11-17-continuation/dns-dedicated-token-controls.jsonl).
Adding Builtin Users as a full restricting SID could additionally admit Users
write grants and must not silently be represented as the existing write policy.

Zero-capability LPAC is also insufficient for the console ceiling. The
[C++](evidence/windows-process-tree-2026-11-17-continuation/console-lpac-cpp-live.log)
and [Rust](evidence/windows-process-tree-2026-11-17-continuation/console-lpac-rust-live.log)
controls retain both the payload and Conhost, verify membership in the exact Job
and observe both unsignaled at limit one, even though `AllocConsole` returns
access denied. All retained processes are subsequently signaled and the fresh
per-user AppContainer profiles are deleted. The prepared temporary KnownDlls
read-grant control has not run. The user explicitly declined KnownDlls changes;
this route is closed and no approval remains pending. No KnownDlls, DNS service
or registry permission change is part of the product patch.

CI now includes an unconditional `windows-process-tree` job. Its checked-in
`scripts/test-windows-process-tree.ps1` builds/installs the package, provisions
with the installed helpers, requires the entire native suite with a controlled
non-loopback witness, and creates a **second** fresh provision for the real
installed CLI/Cargo start. The suite's deliberately retained host-death receipt
is preserved. `scripts/validate-windows-process-tree.py` rejects missing evidence
markers and optional NOT RUN results and verifies the actual installed tool
result, output and Windows artifact. The job is expected to fail while the
qualification guard or remaining defects are present. The existing LPAC checks
remain independent.

The [bounded evidence summary and hashes](evidence/windows-process-tree-2026-11-17-continuation/summary.json)
identify this continuation. The sections below preserve the earlier checkout's
source identities, schema-3 provisions and historical results; their old local
target paths are not results from the current workspace.

## Final native checks in this continuation

The complete mandatory process-tree command was invoked with
`REQUIRE_WINDOWS_PROCESS_TREE=1` from the actual same-user Medium token, with
no administrative elevation. All six lifecycle scenarios pass, then the strict
console assertion fails: runner, payload and Conhost are simultaneously live in
the exact Job at approved limit **two**. Failure cleanup is confirmed. The
[unfiltered suite result](evidence/windows-process-tree-2026-11-17-continuation/mandatory-medium-suite-result.json)
and [log](evidence/windows-process-tree-2026-11-17-continuation/mandatory-medium-suite.log)
are a failed qualification, not a partial pass for the full feature.

A separate [actual DNS tool case](evidence/windows-process-tree-2026-11-17-continuation/dns-medium-native-result.json)
using the installed helper and the same Medium host also fails: the synthetic
server receives the sandbox nonce once. Host controls before and after both
succeed; all four retained process objects are signaled immediately at tool
return. The service-mediated gap is still present in the product design.

The independent existing baseline and security checks pass under the verified
Medium token: **54 distinct top-level tests**, zero failures and five deliberate
subprocess-fixture ignores. This comprises 35 integration tests, nine
cleanup/shutdown/parallel/CLI tests, eight pre-existing Windows runtime/security
units, and two new namespace-security units. The
[integration](evidence/windows-process-tree-2026-11-17-continuation/native-baseline-result.json),
[control](evidence/windows-process-tree-2026-11-17-continuation/native-control-units-result.json)
and [Windows unit](evidence/windows-process-tree-2026-11-17-continuation/windows-runtime-security-units-result.json)
records retain executable/source hashes and actual host-token/Job-drain evidence.
The original detached-child positive control exposed an integration race: it
closed its creation handles and polled files for only 2.5 seconds. It now retains
the original child handle, waits asynchronously for actual exit zero, verifies
both marker contents and cleans up that exact child on timeout/panic. Host-only
stdio is valid and explicitly allowlisted; the sandbox denial still reaches
`CreateProcessW` without an earlier NUL/attribute failure.

The acceptance launcher has separately passed a real
[abort control](evidence/windows-process-tree-2026-11-17-continuation/acceptance-host-abort-result.json):
its noninherited outer kill-on-close Job terminates the native command tree,
both retained creation handles are signaled and the Job is empty. It imposes no
UI or resource limits. The [Medium Cargo control](evidence/windows-process-tree-2026-11-17-continuation/cargo-medium-result.json)
still observes the actual product Job's UI mask `0xff`, approved limit 32,
Windows artifact and immediate all-member cleanup. Refusal diagnostics append
at EOF so they preserve the original native log.

The first manual debug package accidentally used mixed top-level EXEs from a
shared Cargo target directory. Its [product provenance is invalidated](evidence/windows-process-tree-2026-11-17-continuation/invalidated-mixed-package.json)
and those local archives are named `INVALID-PRODUCT-DIAGNOSTIC`. Their enabled
diagnostic execution is not accepted as a delivered-product result. The corrected
product build uses an exclusive output directory and verifies identical build
input hashes before/after. Its new debug ZIP has SHA-256
`6e17443bbfa54b6a526dbcb5c32e2ed7cca704f94524d202c215d9d3d9da8781`.
The [fresh installer smoke](evidence/windows-process-tree-2026-11-17-continuation/fresh-install-smoke-result.json)
matches every installed executable to that archive. A new provision from those
exact installed helpers and the
[real installed coding-profile start](evidence/windows-process-tree-2026-11-17-continuation/installed-profile-refusal-result.json)
confirm `process.run` returns `sandbox_unqualified`, `executed=false`, and no
Windows artifact. CLI exit zero alone is deliberately insufficient: the
acceptance helper rejects the failed tool result. The installed script follows
the real `task.plan`, input-read, behavior-verification and completion cycle.

CI's product builds now have an exclusive Cargo output directory; source
snapshots also use their own test target. The existing native CI additionally
runs `runtime::windows` so all Windows namespace/provisioning/runner units are
covered. The CI-orchestration tests (eight), installer tests (five passed with
seven platform skips), acceptance-script syntax and diff whitespace checks pass.
The broader Python discovery is not green on this Python 3.10 host because the
pre-existing release tooling imports Python 3.11's `tomllib`; CI installs Python
3.12. No hosted CI run, remote PR update or Git mutation occurred.

The user explicitly answered **"Keine KnownDlls-Änderungen"** on 17 November
2026. The prepared four-object KnownDlls control has not run and must not run
under this authorization. There is no outstanding request for KnownDlls approval.
The earlier automatic rejection remains historical context, not a reason to
request the same permission again. Source and installed product remain gated.

A subsequent read-only audit found no established supported replacement within
the existing backend that preserves ordinary Cargo child creation and the strict
live-process ceiling. The documented [child-process mitigation](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntddk/ns-ntddk-_process_mitigation_child_process_policy)
also denies needed ordinary children. The [general syscall filter](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntddk/ns-ntddk-_process_mitigation_system_call_filter_policy)
is explicitly unsupported, while the [FSCTL restriction](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-process_mitigation_system_call_disable_policy)
does not provide a general ConDrv device-access policy. A pre-attached console
or ConPTY is not an allocation prohibition: [FreeConsole](https://learn.microsoft.com/en-us/windows/console/freeconsole)
allows detachment and subsequent allocation. [Private object namespaces](https://learn.microsoft.com/en-us/windows/win32/sync/object-namespaces)
do not establish a private device view for the absolute ConDrv path used by
[Microsoft's console implementation](https://github.com/microsoft/terminal/blob/main/src/server/DeviceHandle.cpp).

For DNS, [application settings](https://learn.microsoft.com/en-us/windows/win32/api/windns/nf-windns-dnssetapplicationsettings)
select servers rather than enforce a sandbox boundary; [DnsQueryEx requests](https://learn.microsoft.com/en-us/windows/win32/api/windns/ns-windns-dns_query_request)
can supply their own server list. [AppContainer network configuration](https://learn.microsoft.com/en-us/windows/win32/api/networkisolation/nf-networkisolation-networkisolationsetappcontainerconfig)
manages loopback exceptions for actual AppContainers, not a mandatory broker
denial for the ordinary dedicated account. Microsoft currently documents
[thread compartment selection](https://learn.microsoft.com/en-us/windows/win32/api/netioapi/nf-netioapi-setcurrentthreadcompartmentid)
as reserved and not for use. [WFP connect redirection](https://learn.microsoft.com/en-us/windows-hardware/drivers/network/using-bind-or-connect-redirection)
requires a callout/proxy design and is not evidence that the original ALPC
request is denied. No new system probe or permission change was performed.
These findings do not prove impossibility; the DNS and console defects remain
unresolved and full feature qualification has not been achieved.

## Host and evidence identity

Native host: Windows 11 Pro 25H2 **26200.9974**, x64 Rust/MSVC, MSVC
**14.51.36231**, SDK **10.0.26100.0**, Python **3.10.6**, Windows PowerShell **5.1**.
Builds, tool calls and regression tests run with the ordinary host token. Account
provisioning and explicitly approved administrative diagnostics are separate.

Evidence is retained under `target/windows-process-tree-20261116/native/`.
The final disabled-source build and preserved baseline are pinned to
[`final-disabled-source-snapshot.json`](../target/windows-process-tree-20261116/native/final-disabled-source-snapshot.json),
SHA-256 `46ecdb48cf9be069d85e8b8613fc9ae21acb8564aec00890058f60ed7974389b`.
Native bins build, tree-test compilation and all-target Clippy pass as the
ordinary host; see
[`final-disabled-build-results.json`](../target/windows-process-tree-20261116/native/final-disabled-build-results.json).
This source includes the bootstrap-publication correction and unconditional
`sandbox_unqualified` guard. The regression snapshot has LF serialization and
SHA-256 `90ac153519e2ec5738ca8530b8377b027ec3b4de14b008fa0ea9a3f433d37e53`;
the release snapshot uses CRLF. Their 172 file/hash entries are identical, as
recorded in [`final-disabled-snapshot-equivalence.json`](../target/windows-process-tree-20261116/native/final-disabled-snapshot-equivalence.json).
Individual results record commands and source/log hashes. Earlier enabled
diagnostic builds are identified separately and are not part of the final
guarded delivery. The final release and installed-refusal results are below.

The integrated runtime diagnostics use existing provision D:

- Account `wwbox_5a149d8c936b`, SID
  `S-1-5-21-4275439644-3620842777-1714106797-1006`.
- Required manifest schema 3, seven persistent account WFP filters and recorded
  effective sublayer weight **65532**. Exact readback is required at normal start.
- Diagnostic manifest SHA-256
  `e8fffd26a67933ce86d5c4e5af5fa6dbb31742835b614a565559d0ee550c6493`;
  debug runner SHA-256
  `e23fc45f244af3082148f6c5a6ccc1a020732bcadbed3b04048ab20f840e58fe`.

D was initially provisioned from installed helpers, including correct built-in
Users membership. Its runner/receipt were subsequently updated under trusted
control for diagnostic iterations. These runs are not fresh-install acceptance
of the current release binaries.

## Delivered Windows build and installation smoke

The final Windows ZIP contains the technically disabled profile:
[`final-disabled-package/werkWorker-v0.3.3-windows-x86_64.zip`](../target/windows-process-tree-20261116/final-disabled-package/werkWorker-v0.3.3-windows-x86_64.zip),
SHA-256 `afe42063a3c3cb6ee83017989ac055f1b8239e7945db8b8ce7bd49a1e7ea6e61`.
This exact archive was hashed locally after copying the native result.

The native release build, package validator and installation into a fresh
`installed-final-disabled` directory pass as the normal host. Both commands and
both helpers are included; installed files match the archive. The provisioner
without administrative setup returns `admin_required`/exit 2; the runner without
its private protocol refuses with exit 125. Ordinary offline CLI sessions work
from the installation outside the checkout.

| Release executable | SHA-256 |
| --- | --- |
| `werk-worker.exe` | `729d41e8a3ada342264c76a8c598451779de33283b6c1e5020680df4658e1f23` |
| `ww.exe` | `ed4d38036a566387ffcbd5aea7b2fadfa738924c8a91064b13d7e1a7f0f603fc` |
| `ww-windows-provision.exe` | `1282d30d0f2c259338bb277fb58ff98943140fa88fb12aef88ac57bd390826a4` |
| `ww-windows-runner.exe` | `ad28353d3a17f453e2f67a39697577eea6b7b75f334eb0e205809301618d8850` |

See [`final-disabled-release-result.json`](../target/windows-process-tree-20261116/native/final-disabled-release-result.json)
and [`final-disabled-installed-smoke-result.json`](../target/windows-process-tree-20261116/native/final-disabled-installed-smoke-result.json).
The earlier `unqualified-diagnostic-package` ZIP and top-level historical ZIP are
retained evidence, not the final delivery.

The freshly installed `ww.exe` also receives explicit profile selections using
the existing D provisioning and grants. Doctor reports `available=false` and
`probe_verified=false` with `sandbox_unqualified`. Both actual CLI requests,
`process.run` and `process.start`, produce a recorded failed ToolResult with
`prepared=null`, `error_code=sandbox_unqualified` and `data.executed=false`.
Refusal occurs at preparation/admission, before builtin execution. The expected
payload artifacts remain absent, no cleanup marker appears, and D manifest and
runner hashes remain unchanged. The two-turn diagnostic scripts subsequently
end with `script_exhausted`/CLI exit 1; this is a refusal check, not a completed
coding-workflow pass. See
[`final-disabled-installed-refusal-result.json`](../target/windows-process-tree-20261116/native/final-disabled-installed-refusal-result.json).
No fresh profile provisioning or successful profile startup is claimed.

## Native results on the frozen enabled diagnostic implementation

| Check | Result and exact scope |
| --- | --- |
| Native product build, tree-test compilation, all-target Clippy | **PASS**, ordinary host; [`d-ledger-build-results.json`](../target/windows-process-tree-20261116/native/d-ledger-build-results.json) |
| Doctor and actual coding CLI workflow | **PASS**, existing diagnostic D; two `process.run` calls return exit 0 with visible output, workspace artifact and behavior verification |
| Real-tool private USER boundary | **PASS**, private station/desktop, UI255, hostile desktop creation/switch, outside-job window messaging and private synthetic clipboard controls |
| Real-tool Parent → Child → Grandchild boundaries | **PASS**, four exact Job members including the runner; permitted workspace/temp/toolchain reads, denied synthetic host-file/Registry/pipe access, host-logon-SID isolation and direct network checks |
| Immediate completion evidence | **PASS** in those selected UI/boundary cases: actual builtin return samples are `[0,0]` / `[0,0,0,0]` using `WaitForSingleObject(handle, 0)` |
| DNS broker isolation | **FAIL**: `DnsQueryEx` resolves a fresh synthetic loopback nonce and the independent listener receives it |
| Thread-identity network matrix | **FAIL** at the dedicated-user baseline DNS case; direct TCP/UDP IPv4/IPv6 attempts are denied with error 10013, but DNS still arrives |
| Windows `cargo test --offline` | **FAIL**: Rustc jobserver semaphore creation returns error 5; Cargo exits 101, with no qualifying test artifact |
| Post-resume mandatory lifecycle prefix | **PASS**, six cases on the later frozen diagnostic source: normal exit, parent exits with live descendants, timeout, `process.cancel`, run abort and Engine close |
| Selected D resource limits, 17 November | **PASS**, actual `process.run`: 128 MiB memory limit and one-second CPU limit; retained handles already signaled at tool return |
| Selected D bounded output, 17 November | **PASS**, 256 KiB on each output stream with 4096-byte capture, truncation, draining, artifact and immediate process-handle completion |
| Console/process limit | **FAIL**: `AllocConsole` succeeds and a live Conhost raises actual Job membership to 3 at process limit 2 |
| Full `REQUIRE_WINDOWS_PROCESS_TREE=1` acceptance | **FAIL**, stops at the strict console assertion; selected diagnostics do not substitute for the complete run |
| Selected abrupt host death and post-crash workspace refusal, 17 November | **PASS** on the separate historical diagnostic; four owned process handles end, workspace admission remains blocked; subsequent separate administrative recovery also passes |
| Fresh current-release provisioning and actual profile startup | **NOT QUALIFIED**; the delivered profile is disabled and installed requests are correctly refused |

The successful coding workflow is recorded in
[`diagnostic-positive-d-start-result.json`](../target/windows-process-tree-20261116/native/diagnostic-positive-d-start-result.json).
It used the debug CLI and existing D provisioning. The artifact SHA-256 is
`fde3ba8117b8a86d8c744bdf353ea1fd199aaac34babd6f34d8bb381c81801b0`.
Doctor's startup probe succeeded; that does not certify DNS or the full isolation
contract advertised by a descriptor.

The latest selected UI and boundary runs share
[`unqualified-diagnostic-source-snapshot.json`](../target/windows-process-tree-20261116/native/unqualified-diagnostic-source-snapshot.json),
SHA-256 `ae1979349da07c8e8679892f45f2b6cc3f17d96eb66b84f31d902b67b8be4937`,
the D manifest/runner hashes above, exit 0 and no remaining cleanup marker:

- [`d-ledger-selected-ui-result.json`](../target/windows-process-tree-20261116/native/d-ledger-selected-ui-result.json),
  log SHA-256 `6cb0d091fcfe3c0edb08b3be4548f37dfb5b92ead97986fbbe4768a3ecbacc16`.
- [`d-ledger-selected-boundaries-result.json`](../target/windows-process-tree-20261116/native/d-ledger-selected-boundaries-result.json),
  log SHA-256 `39a32f006b1512a1172bf5ab8a78e1b688fe4b2deeb766add7ee0ffde6e37b9f`.

The UI fixture uses a separate ordinary-host observer on the newly created
noninteractive station; the worker host never switches its own station.
Synthetic clipboard data remains unchanged. Payload clipboard open/read/clear
fail with error 5, desktop creation/switch fail with error 5, and the outside-job
window message fails with error 1400. The observer exits before payload cleanup.
No existing user clipboard contents are read or changed.

All three boundary generations show the inherited host logon SID deny-only and
absent from restricting SIDs. A newly reopened host-positive file protected only
for SYSTEM and that logon SID denies payload reads and writes with error 5.

The later mandatory run includes the bootstrap-publication correction, with
source snapshot SHA-256
`10181c38d92622408e8892ac63b62ea7c6ec26020ed83b0f61c7c0cdf783f816`.
It passes six lifecycle cases, then fails the strict console case before DNS.
`AllocConsole` returns success and the live Conhost is actually in the owned Job:
three active members violate the advertised limit of two. Trusted cancellation
and Engine close confirm cleanup, and no marker remains. See
[`postresume-required-tree-result.json`](../target/windows-process-tree-20261116/native/postresume-required-tree-result.json)
and [`postresume-required-tree.log`](../target/windows-process-tree-20261116/native/postresume-required-tree.log),
log SHA-256 `5e2dbab5423e79fcf1953a39bfa7b343ea475cf13f3a29646523ec2d7a1494c4`.
These results precede the mandatory profile-disable guard. Earlier C resource,
output and console diagnostics are not final-backend acceptance; the remaining
mandatory cases, including abrupt host death, did not execute in this run.

Further ordinary-host controls on 17 November reproduce the console failure
with UI masks `0xff`, `0x1ff`, `0x2ff` and `0x3ff`, and with two nested Jobs that
both have an active-process limit of two. The live Conhost belongs to both
Jobs. Removing `CREATEWINDOW` and `CREATEMENU` from a fresh private Desktop ACL
does not establish the limit either: `AllocConsole` fails, but an early retained
handle proves that a third live process existed before the helper exited.
These controls use the ordinary host identity, not the dedicated D token, and
do not qualify an alternative runtime. All eleven cases confirm cleanup of
their 33 retained process objects. See
[`allocconsole-controls-summary.json`](../target/windows-process-tree-20261116/native/allocconsole-controls-summary.json),
SHA-256 `31fff004dae51f19e4a42839d8b591604b66e0d2a0c54fc2fb191fa31a04b536`.

The subsequent selected D resource and output cases use the separately retained
test executable SHA-256
`14ed2280640593e759f8f6dd09dcbbf714592e58c1f6320510cfd85adfa699f0`.
This reconstructs the historical post-resume implementation; the only Rust delta
adds an explicit `host-death` diagnostic selector. All 180 selected-target build
inputs are pinned: the previous 172-entry source map plus eight unchanged
embedded profile/skill files, also checked against commit `2398f9f`. The guarded
product files remain unchanged. The two cases pass as the normal host and leave
no cleanup marker. See the
[resource result](../target/windows-process-tree-20261116/native/historical-selected-resources-20261117-679a51dc.json),
[output result](../target/windows-process-tree-20261116/native/historical-selected-output-20261117-f39a208e.json)
and [separate diagnostic build plan](../target/windows-process-tree-20261116/native/host-death-diagnostic-plan.json).
These selected passes do not turn the failed mandatory run into a pass.

The separate `host-death` case then passes in 7.39 seconds. Before the crash,
the exact Job contains runner, parent, child and grandchild with kill-on-close,
no breakaway and limit four. After killing the actual worker host, the test
waits on the four retained process objects and verifies zero-time completion;
its external observer also confirms termination of the worker host and launcher.
Subsequent file read/list/write preparation and execution, process preparation,
and previously prepared process execution/verification all fail with
`cleanup_required`. Status and cancel remain usable. See the
[host-death summary](../target/windows-process-tree-20261116/native/host-death-03f5-summary.json),
SHA-256 `3f25a538e9943376eae8e4ee1ef86bca1e3055a4aba576f1065d76d37b3790b0`.
The test intentionally left the marker. Its subsequent separate administrative
recovery passes: exact original process identities are absent or signaled, two
complete WTS inventories (466 processes each, with only the known kernel PIDs
0/4 exempted from SID resolution) contain no D-account or exact diagnostic-host
processes and no unresolved identities. The pinned marker's file identity,
contents and ACL match the test receipt; a protected backup precedes deletion
through that exact handle. The retained administrator process is signaled with
exit 0. Independent normal-host readback confirms marker absence, available
invocation lock and unchanged provisioning hashes. See the
[confirmed recovery summary](../target/windows-process-tree-20261116/native/host-death-confirmed-recovery-20261117-summary.json),
SHA-256 `e2c7c8db948711111abc8da8c421f100876d8e2235097ca411405a5b21469a54`.
The first normal controller attempt stopped before launching any test because
it incorrectly required administrative SID visibility. The corrected normal
preflight verifies exclusive admission and marker absence, explicitly records
partial visibility, and leaves the complete inventory requirement in the
administrator-only recovery. Native test assertions were not weakened.

## Cleanup contract and its availability limit

The implementation retains the host-owned Job, restricted USER objects,
workspace lease and protected invocation lock until confirmed cleanup. The
Job is assigned before execution, uses kill-on-close and forbids breakaway;
only the approved stdio handles enter the payload. A persistent unconfirmed
cleanup receipt blocks later workspace tool preparation/access, including file
tools and verification reads. Status/wait/cancel remain available.

Native isolated controls proved that `ActiveProcesses == 0` and an empty Job PID
list can occur while retained process handles still return `WAIT_TIMEOUT`.
Consequently the integrated cleanup uses a retained process-object ledger:
completion-port notifications discover candidates, and confirmation requires
exact lifetime `TotalProcesses` equality, all recorded handles signaled, Job
active count zero and confirmed private USER-object closure.

**IOCP delivery is not claimed to be lossless.** A lost `NEW_PROCESS`
notification, or a process object that can no longer be opened before it is
pinned, leaves cleanup unconfirmed. The lease/guard and crash barrier remain
held rather than treating the missing history as successful cleanup. The
lifetime history cap is **4096 objects**. No subtraction or credit from
`TotalTerminatedProcesses` fills a missing ledger entry. This can make an
otherwise finished invocation unavailable; final Cargo/lifecycle acceptance
must still prove practical operation under this policy.

Separate ordinary-host controls passed a three-generation tree, 49 lifetime
processes from fast commands, process-limit refusal, CPU and memory cases.
Deliberately suppressing one notification correctly failed confirmation despite
Job zero and signaled known handles. These are supporting controls, not the
provisioned feature gate. Exact scripts, hashes and outcomes are in
[`job-cleanup-controls-summary.json`](../target/windows-process-tree-20261116/native/job-cleanup-controls-summary.json).

## Remaining backend failures and diagnostic scope

DNS remains a concrete isolation failure. The strict real-tool DNS test reports
`status=0`, `record=true`, with the fresh sandbox nonce observed by the synthetic
`127.0.0.1:53` listener; normal-host controls succeed before and after. Six ALE
socket blocks plus the seventh `RPC_UM` account filter do not establish denial
of this service-brokered path. See
[`d-selected-remaining-results.json`](../target/windows-process-tree-20261116/native/d-selected-remaining-results.json),
[`d-selected-network-results.json`](../target/windows-process-tree-20261116/native/d-selected-network-results.json)
and their logs. These strict failures have not been relaxed or reclassified.

Subsequent isolated controls with the current D receipt also show that removing
Everyone from the write-restricted SID list does not stop DNS: the fresh query
still succeeds and reaches the listener. A fully restricted User+Users token
without `WRITE_RESTRICTED` fails before a usable payload receipt; its lack of DNS
traffic is not denial evidence. Neither token experiment is a product fix.

A stronger Low-integrity candidate was then exercised with the same dedicated
account and restricting SIDs. Its actual payload token is
`S-1-16-4096`; User32 initialization and the labels of the newly created private
station, desktop and three stdio pipes are verified. Only fresh test objects
are labeled; existing system, workspace and host objects remain unchanged.
Nevertheless the fresh DNS query succeeds and independently arrives once, with
healthy host controls before and after. `AllocConsole` also succeeds and a
retained live Conhost raises actual membership from two to three at limit two.
All three process objects are subsequently signaled, lifetime accounting agrees,
private USER handles close and the marker is removed. Thus lowering integrity
to Low does not fix either failure. See the
[Low-integrity summary](../target/windows-process-tree-20261116/native/low-integrity-e74c-summary.json)
and [source/build/run plan](../target/windows-process-tree-20261116/native/d-low-integrity-probe-plan.json).
The diagnostic EXE SHA-256 is
`e74c378ed7b936e09b577dc68405273f892d1524adb374badb91989799909a1a`;
log SHA-256 is `206c273ad37d91d2082b01ad6f85ed0df9e0628f65e9163a0ba4013c80af35ac`.
Earlier attempts stopped before any payload because a label operation changed
inherited-ACE flags on the new receipt directory. Creating that new directory
with an explicit protected ACL resolved the test setup without weakening the
exact before/after security check. No Low-integrity change enters the product.

The final stricter MIC control changes only the derived payload and those fresh
private object labels to Untrusted (`S-1-16-0`), retaining the Medium bootstrap,
the same DACLs, restricted groups, Job and seven account filters. This payload
also initializes User32 successfully. DNS still arrives once with successful
host controls, and a live Conhost again raises membership from two to three at
limit two. Cleanup confirms all three retained handles, active count zero,
private USER closure and marker removal. Thus neither Low nor Untrusted MIC
repairs the measured failures. See the
[Untrusted summary](../target/windows-process-tree-20261116/native/untrusted-integrity-a3b4-summary.json)
and [exact candidate diff and build plan](../target/windows-process-tree-20261116/native/d-untrusted-integrity-probe-plan.json).
Its executable SHA-256 is
`a3b4e78dc651ae827cc67b4a705bedfb5d88274a70ec8ce410b289d8bf69160b`.
This is a separate diagnostic, not an enabled product profile.

Cargo's current blocker is named-object access, not the resolved User32 startup
failure. The strict tool run records Rustc's `failed to create jobserver` with
`ERROR_ACCESS_DENIED`, followed by Cargo exit 101; see
[`d-selected-cargo-result.json`](../target/windows-process-tree-20261116/native/d-selected-cargo-result.json)
and [`d-selected-cargo.log`](../target/windows-process-tree-20261116/native/d-selected-cargo.log).
Read-only namespace diagnostics find no dedicated-user grant on the session
`BaseNamedObjects` root and deny create-object access. They do not justify
re-enabling the host logon SID or broadening inherited access.

The account-scoped firewall experiment **ran on 17 November and did not fix
DNS isolation**. After real UAC consent, the administrator helper installed and
read back exactly two temporary `LocalUserAuthorizedList` rules for D: outbound
TCP/UDP block to `127.0.0.1:53`. Separate normal-host probes before, during and
after the rules each observed one fresh dedicated-payload DNS query arriving at
the independent listener. All six ordinary-host controls succeeded. The three
probes use the same frozen diagnostic EXE, SHA-256
`fd4b4ce5ec55fc486ac895f6c694431a859f610709b9ea058e1e11ce8ab28e73`;
they do not bypass the current product guard. See the
[A/B/A comparison](../target/windows-process-tree-20261116/native/d-firewall-dns-comparison-cycle-after-20261117-8da4b46c.json).

The same administrator process removed the two exact rules after confirmed
probe cleanup and exited 0. Independent normal-host readback confirms both
rules absent, D manifest/runner unchanged, the invocation lock available and
no incomplete-run marker. See the
[admin-cycle result](../target/windows-process-tree-20261116/native/dns-firewall-cycle-8da4b46c64604b1fb39ac0319a075382/result.json),
[rule-absence readback](../target/windows-process-tree-20261116/native/d-firewall-cycle-after-inspect-8da4b46c64604b1fb39ac0319a075382.json)
and [final state](../target/windows-process-tree-20261116/native/d-firewall-cycle-final-state-8da4b46c64604b1fb39ac0319a075382.json).
Earlier cancelled UAC attempts remain historical evidence; administrative
consent is no longer the blocker for this completed experiment. The final
product guard and ZIP remain unchanged.

A temporary namespace-root grant was actually tested through `process.run`.
The only new ACE granted the dedicated SID create-object access (mask 4), without
inheritance, on the session `BaseNamedObjects` root. The unnamed semaphore
control succeeds, but all four named variants still fail with error 5, including
the Rust jobserver-style name. This disproves that narrow candidate as a Cargo
fix. The guard restored and read back the exact original descriptor, then exited
0; the cleanup marker is absent. Original descriptor SHA-256:
`651abf0989b4a336ef1a9748c9919134b0c17b9fac81e5ece5f574c828fdc77c`.
See the [actual tool receipt](../target/windows-process-tree-20261116/native/d-namespace-sem-actual-tool-receipt.json),
[tool result](../target/windows-process-tree-20261116/native/d-namespace-sem-tool-failed-tools.json)
and [confirmed restoration](../target/windows-process-tree-20261116/native/d-namespace-grant-hold-result.json).
No namespace grant is part of the product fix.

The subsequent administrator read-only inspection identifies the separate
`\Sessions\1\BaseNamedObjects\Restricted` Directory. Its original descriptor
SHA-256 is `93374df446fd1e7c5d0c471c60cfae51967b8840480df64a4dc0ab4b3c589b93`.
Everyone has query/traverse only; Restricted Code has query/traverse/read-control
only. Neither ACE grants create-object access, and the D SID has no ACE.
The ordinary host cannot read or change this DACL. See the
[native readback](../target/windows-process-tree-20261116/native/restricted-namespace-admin-readback-20261117.json).
The subsequently executed, temporary non-inheriting D-only create-object ACE
on this Directory also fails as a standalone fix. The actual frozen
`process.run` payload still creates only the unnamed semaphore; all four named
variants return error 5. Cargo is not attempted after this failed prerequisite.
See the [five-case payload receipt](../target/windows-process-tree-20261116/native/restricted-only-semaphore-matrix-81a89685.json)
and [actual tool export](../target/windows-process-tree-20261116/native/d-restricted-sem-tool-start-result.json).
The native guard restores the exact original descriptor and exits 0; the
administrator process handle is also signaled with exit 0. See the
[restoration receipt](../target/windows-process-tree-20261116/native/namespace-restored-81a89685-1e36-47db-bd05-fa667c1e8ab1.json)
and [controller result](../target/windows-process-tree-20261116/native/namespace-restricted-controller-81a896851e3647dbbd05fa667c1e8ab1-result.json).

The combined root-plus-Restricted variant **ran on 17 November after explicit
user authorization and actual Windows UAC consent**. Each Directory received
only the dedicated user's non-inheriting create-object ACE (mask 4). Both exact
grant receipts and live owners (retained handles, PID, creation time and image)
were checked before each test; configuration, approvals and diagnostic binaries
were pinned. Results were saved in fresh directories without overwriting earlier
attempts. The Restricted owner entered its documented recovery-hold state while
waiting for the second grant; it retained the Directory handle and backup until
the explicit restore request.

The actual `process.run` semaphore matrix still creates only the unnamed
control; all four named variants fail with error 5 and the payload exits 125.
After confirmed cleanup, a separate actual `process.run` invokes the provisioned
Rust/MSVC toolchain with `cargo test --offline -- --nocapture` in a fresh,
dependency-free crate. Cargo exits 101: Rustc's jobserver creation fails with
error 5, followed by Rustc exit `0xc0000409`. No expected test output or workspace
artifact is produced; the separate artifact-verification tool call reports
`not_found`. The scripted host sessions subsequently end with
`script_exhausted`; the exported tool results retain the actual payload errors.
These are negative runtime results, not a successful coding workflow.

The root ACL is restored first, with exact original descriptor readback and
actual guard exit 0. The Restricted ACL then receives the same checks; its guard
and the retained administrator process both exit 0, the latter at
**14:26:07.508 UTC**. Both original descriptor hashes match those above. Final
normal-host readback at **14:27:06.779 UTC** confirms the invocation file can be
opened exclusively and closed, the cleanup marker is absent, and the D
manifest/runner hashes remain unchanged. See the
[combined summary](../target/windows-process-tree-20261116/native/combined-namespace-68bc-summary.json),
[semaphore tool export](../target/windows-process-tree-20261116/native/combined-tool-sem-d7221b1dde1142f4b9994c0aca78d619/result.json),
[Cargo tool export](../target/windows-process-tree-20261116/native/combined-tool-cargo-7172d5b6bf114d979c2a3bd8dc2d6e7f/result.json)
and [final state](../target/windows-process-tree-20261116/native/combined-grant-final-state-68bc5f04.json).
No temporary namespace grants remain. Administrative approval is no longer
pending for this completed experiment; it disproves this narrow ACL combination
as a Cargo fix. No namespace ACL repair is integrated into the product.

The separate WMI read-only diagnostic has also executed. Ordinary-host controls
before and after connect to local `ROOT\CIMV2` and read both process classes;
namespace `GetSD` returns `0x80041003` for the normal host. The dedicated payload,
with actual token and private USER handles verified, fails earlier at in-process
`CoCreateInstance(WbemLocator)` with `0x80070005`. This does not attribute denial
to the RPC filter or establish the namespace ACL. No process-creation method
was invoked, and all retained handles were cleaned up with no marker remaining.
See [`d-station-wmi-readonly-summary.json`](../target/windows-process-tree-20261116/native/d-station-wmi-readonly-summary.json).
This is a narrow unavailable-path result, not proof that all process-start
brokers are excluded.

## Preserved single-process baseline

The latest native rerun uses the final disabled source (regression snapshot
`90ac1535…`, identical source entries to release snapshot `46ecdb48…`) and the
ordinary host with the existing required single-process suite enabled. All eight commands
succeed:

| Check | Result |
| --- | --- |
| Existing integration suites | 35 top-level tests passed, 4 existing ignores; includes all 7 required `windows_sandbox` tests |
| Windows runtime units | 8 passed, 1 existing ignore |
| Job cleanup / Engine shutdown / parallel / CLI configuration | 3 / 3 / 2 / 1 passed |
| No-default-feature library and external SDK consumer | Both offline checks passed |

That is **52 top-level tests**. One nested fixture emits an additional passing
line in the integration log; it is not counted as another test. Commands,
source identity and log hashes are recorded in
[`final-disabled-native-regression-results.json`](../target/windows-process-tree-20261116/native/final-disabled-native-regression-results.json).
The existing single-process test executable also has no static User32 import;
see [`final-disabled-singleprocess-import-result.json`](../target/windows-process-tree-20261116/native/final-disabled-singleprocess-import-result.json).

## Commands and remaining acceptance gates

Native builds run in the x64 MSVC developer environment with
`CARGO_NET_OFFLINE=true`. The successful package/install commands are recorded
in the release result file. The mandatory feature command remains:

```powershell
$env:CARGO_NET_OFFLINE = 'true'
$env:WW_WINDOWS_PROCESS_TREE_PROVISION = "$lab\runtime-d\provisioning.json"
$env:WW_WINDOWS_PROCESS_TREE_CARGO = "$lab\runtime-d\toolchains\0\bin\cargo.exe"
$env:WW_WINDOWS_PROCESS_TREE_EXTERNAL_ENDPOINT = '<controlled reachable non-loopback listener>'
$env:REQUIRE_WINDOWS_PROCESS_TREE = '1'
cargo test --locked --offline --test windows_process_tree -- --nocapture --test-threads=1
```

Selected diagnostics use `WW_TREE_DIAGNOSTIC_CASE` and the explicitly ignored
`diagnose_native_process_tree_case` entry with `--ignored --exact`. Each invokes
the same strict case helper, but no selected pass qualifies the full suite.
Neither `--no-run`, a successful Doctor nor a package-only smoke is sufficient.
The current source also refuses these entry points with `sandbox_unqualified`;
there is no environment, feature or diagnostic exception to the guard.

Before re-enabling the profile, correct and repeat the real-tool DNS, console
limit and offline Cargo cases, including expected output and artifact; then run
the entire required suite with all lifecycle, console, resource, bounded-output and abrupt-host-end
checks. Provision a fresh workspace from the exact current installed helpers
and verify actual installed profile startup. Keep the profile technically
disabled until these gates pass. Setup and host selection are documented in
[windows-process-tree-provisioning.md](windows-process-tree-provisioning.md).
