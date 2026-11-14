# PSEC diagnostic policy encoder

This standalone host tool only writes policy bytes for the adjacent Windows budget
reproducer. It never creates a security environment, executes a payload, or enables
a werkWorker runtime profile. Its workspace and lockfile are separate from the
product; building the product does not build this tool.
Run the reproducer from a complete repository checkout. The diagnostic and its
nested encoder workspace are not included in the published werkWorker crate.

Requires Rust **1.89 or newer**. The pinned `mxc-sdk = 1.0.1` has no declared MSRV,
but calls `std::fs::File::try_lock` and `unlock`, stabilized in 1.89; a real 1.88
compile fails inside the unchanged SDK. This is a source-derived lower bound,
not a claim that the complete Windows dependency closure passed on exactly 1.89.
The standalone encoder and policy roundtrip were checked on Linux with Rust
1.98.0; the native reproducer records its Windows compiler separately.
The SDK crate checksum is
`7c9e2d05b3953fdb34c44e4de8d8f2b447f4444534cb95294b72fb297a468f61`.
`flatbuffers = 25.12.19` is also pinned. Keep `--locked` when building.

```powershell
cargo build --locked --manifest-path scripts/psec-budget-repro/policy-encoder/Cargo.toml
# Use the trusted lab paths prepared by reproduce-windows-psec-budget.ps1:
werkworker-psec-policy-encoder.exe --readonly C:\Windows\System32 `
  --readonly C:\synthetic-lab\runtime --readwrite C:\synthetic-lab\workspace `
  --readwrite C:\synthetic-lab\scratch --deny C:\ --out C:\synthetic-lab\baseline.bin
```

Each `--readonly`, `--readwrite` and `--deny` accepts one drive-absolute Windows
path and can be repeated. The output file must not exist. For the second policy,
add `--deny C:\Windows\System32\conhost.exe` and choose a new output filename.
Paths come exclusively from the trusted synthetic lab, not a workspace payload.
The encoder does not resolve aliases, establish filesystem isolation, or turn
filename denies into file-identity guarantees.

Both policies encode PSEC 1.1, no additional capabilities, Win32k disabled, UI mask
`0x3ff`, the supplied explicit path lists, denied network egress/ingress/host
loopback, and no proxy or peer exception. Creation flags belong to the native
reproducer, not the FlatBuffer. The encoder's unit test decodes the resulting
buffer and checks those security settings; passing it says nothing about native
kernel enforcement or the process-budget result.

The implementation uses Microsoft's exact pinned generated schema through the
SDK's **unstable doc-hidden compatibility exports**, also used as the integration
starting point in the user-referenced Codex adapter. It does not present them as
supported `v1` APIs, vendor an alternate schema, or add custom PSEC FFI.

Sources: [Microsoft SDK source and API boundary](https://github.com/microsoft/mxc/blob/fda29ac63bc306b7f97f3b86b4b63d44f7f40cf2/src/mxc-sdk/src/lib.rs),
[official generated schema](https://github.com/microsoft/mxc/tree/fda29ac63bc306b7f97f3b86b4b63d44f7f40cf2/src/mxc-sdk/src/core/process_security_environment_spec),
[MIT license](https://github.com/microsoft/mxc/blob/fda29ac63bc306b7f97f3b86b4b63d44f7f40cf2/LICENSE.md),
[Rust locking API stabilization](https://doc.rust-lang.org/stable/std/fs/struct.File.html#method.try_lock).

## Supported SDK tier discovery

`src/bin/mxc-probe.rs` uses the supported `v1::probe` API from the same pinned
SDK. `serde_json = 1.0.154` was already in the dependency closure and is pinned
explicitly for its report. From a native checkout, run:

```powershell
powershell.exe -NoProfile -File scripts/probe-windows-mxc.ps1 -Offline
```

The runner snapshots every local compiler input, uses a fresh NTFS lab and
records hashes, Windows build/revision and compiler versions. It requests narrow
read/write paths, a volume-root deny, no capabilities, denied egress/ingress/host
loopback, disabled UI and no inherited environment. It never creates a sandbox
or a workload. The selected `base-container` tier, empty warnings, no error and
`needsDaclAugmentation=false` are checked independently of the exit status. Any
other result returns 2. Exit 0 is discovery evidence only: it does not establish
the effective tier of a launched SDK workload, Job assignment, resource limits,
registry/IPC restrictions or confirmed cleanup.

The supported SDK 1.0.1 launch options cannot supply a caller-owned resource Job,
finite aggregate process/CPU/memory limits or a before-resume verification hook.
A successful probe therefore cannot authorize the proposed product profile.
