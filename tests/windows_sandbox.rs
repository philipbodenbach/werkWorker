//! Native LPAC/Job Object conformance. Each denial has a host-side control.
#![cfg(windows)]

use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use werk_worker::{
    core::hash,
    runtime::{JobManager, Sandbox, Workspace},
};

struct Fixture {
    workspace: Workspace,
    parent: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Self {
        let parent = tempfile::tempdir().unwrap();
        let workspace = Sandbox::default()
            .create_native_workspace(&parent.path().join("workspace ü 空"))
            .await
            .unwrap();
        std::fs::copy(
            std::env::current_exe().unwrap(),
            workspace.path.join("fixture.exe"),
        )
        .unwrap();
        Self { workspace, parent }
    }
    fn argv(&self) -> Vec<String> {
        vec![
            self.workspace
                .path
                .join("fixture.exe")
                .to_string_lossy()
                .into_owned(),
            "--ignored".into(),
            "--exact".into(),
            "sandbox_payload".into(),
            "--nocapture".into(),
        ]
    }
    fn input(&self, value: Value) {
        std::fs::write(self.workspace.path.join("payload.json"), value.to_string()).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        Sandbox::default()
            .remove_native_workspace_profiles(&self.workspace)
            .unwrap();
    }
}

#[test]
#[ignore = "subprocess fixture, invoked only inside the native sandbox"]
fn sandbox_payload() {
    let input: Value = serde_json::from_slice(&std::fs::read("payload.json").unwrap()).unwrap();
    match input["mode"].as_str().unwrap() {
        "access" => {
            assert_eq!(
                std::fs::read_to_string("allowed.txt").unwrap(),
                "workspace data"
            );
            assert!(std::fs::read(input["outside"].as_str().unwrap()).is_err());
            use windows_sys::Win32::Networking::WinSock::{WSACleanup, WSADATA, WSAStartup};
            let mut winsock: WSADATA = unsafe { std::mem::zeroed() };
            // A capability-free LPAC may deny Winsock initialization itself.
            // Rust's lazy initializer panics on that denial, so inspect it first.
            let initialized = unsafe { WSAStartup(0x202, &mut winsock) } == 0;
            if initialized {
                assert!(
                    TcpStream::connect_timeout(
                        &input["server"].as_str().unwrap().parse().unwrap(),
                        Duration::from_millis(250)
                    )
                    .is_err()
                );
                unsafe {
                    WSACleanup();
                }
            }
            let writable = std::fs::write("process-created.txt", "created by LPAC").is_ok();
            assert_eq!(writable, input["writable"].as_bool().unwrap());
            assert!(
                !std::env::var("USERPROFILE")
                    .unwrap()
                    .eq_ignore_ascii_case(input["host_home"].as_str().unwrap())
            );
            assert!(std::env::var("WW_SECRET_CONTROL").is_err());
            let temporary = std::path::PathBuf::from(std::env::var("TEMP").unwrap());
            let profile = std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap());
            assert!(
                temporary.starts_with(&profile),
                "{temporary:?} not in {profile:?}"
            );
            assert!(
                std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap())
                    .starts_with(&profile)
            );
            std::fs::write(temporary.join("private-control.txt"), "private temp")
                .unwrap_or_else(|error| panic!("private temp {temporary:?}: {error}"));
            assert_eq!(
                std::fs::read_to_string(temporary.join("private-control.txt")).unwrap(),
                "private temp"
            );
            if let Some(pipe) = input["host_pipe"].as_str() {
                assert!(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(pipe)
                        .is_err(),
                    "LPAC reached a host pipe without an IPC grant"
                );
            }
            if let Some(handle) = input["host_handle"].as_u64() {
                println!("HANDLE_PROBE_STARTED");
                std::io::stdout().flush().unwrap();
                use windows_sys::Win32::Storage::FileSystem::{
                    FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
                };
                let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
                let mut flags = 0;
                let valid = unsafe {
                    windows_sys::Win32::Foundation::GetHandleInformation(
                        handle as usize as *mut _,
                        &mut flags,
                    )
                } != 0;
                let queried = if valid {
                    unsafe {
                        GetFileInformationByHandleEx(
                            handle as usize as *mut _,
                            FileIdInfo,
                            (&mut info as *mut FILE_ID_INFO).cast(),
                            std::mem::size_of::<FILE_ID_INFO>() as u32,
                        )
                    }
                } else {
                    0
                };
                if queried != 0 {
                    assert!(
                        input["handle_volume"].as_u64() != Some(info.VolumeSerialNumber)
                            || input["handle_id"] != json!(info.FileId.Identifier),
                        "Inherited the trusted host's file handle"
                    );
                }
            }
            println!("PAYLOAD_ACCESS_OK");
        }
        "stdin" => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).unwrap();
            assert_eq!(input, "first line\nGrüße 世界\n");
            let arguments: Vec<String> = std::env::args().collect();
            for expected in [
                "",
                "space value",
                "quote\"value",
                "trailing \\",
                "Grüße 世界",
            ] {
                assert!(
                    arguments.iter().any(|value| value == expected),
                    "{arguments:?}"
                );
            }
            println!("PAYLOAD_STDIN_ARGV_OK");
        }
        "output" => {
            std::io::stdout().write_all(&vec![b'x'; 65536]).unwrap();
            std::io::stderr().write_all(&vec![b'e'; 65536]).unwrap();
        }
        "memory" => {
            let mut allocation = Vec::<u8>::new();
            let allocated = allocation.try_reserve_exact(128 * 1024 * 1024).is_ok();
            assert_eq!(allocated, input["allowed"].as_bool().unwrap());
            println!("PAYLOAD_MEMORY_CHECKED");
        }
        "cpu" => {
            println!("PAYLOAD_CPU_STARTED");
            std::io::stdout().flush().unwrap();
            let mut value = 1u64;
            loop {
                value = std::hint::black_box(value.wrapping_mul(1664525).wrapping_add(1013904223));
            }
        }
        "owner-acl" => {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::write("owned-by-child.txt", "child-created file").unwrap();
            assert!(
                std::fs::OpenOptions::new()
                    .access_mode(windows_sys::Win32::Storage::FileSystem::WRITE_DAC)
                    .open("owned-by-child.txt")
                    .is_err(),
                "child must not rewrite ACLs to grant the read-only SID write access"
            );
            println!("PAYLOAD_OWNER_ACL_DENIED");
        }
        "sleep" => {
            std::fs::write("process-ready.txt", "started").unwrap();
            println!("STARTED");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(Duration::from_millis(
                input["delay_ms"].as_u64().unwrap_or(10_000),
            ));
            std::fs::write("late-write.txt", "must not happen").unwrap();
        }
        "descendant" => {
            use std::os::windows::fs::OpenOptionsExt;
            let own_exe = std::env::current_exe().unwrap();
            std::fs::OpenOptions::new()
                .access_mode(0x20)
                .open(&own_exe)
                .unwrap();
            // Denial is a declared native mode, including ordinary, detached,
            // and breakaway attempts; it is not a failed cleanup claim.
            for flags in [0, 0x00000008 | 0x00000200, 0x01000000 | 0x00000008] {
                assert!(
                    spawn_detached(flags, None).is_err(),
                    "child creation unexpectedly succeeded"
                );
            }
            println!("PAYLOAD_CHILD_CREATION_DENIED");
        }
        _ => panic!("unknown fixture"),
    }
}

fn spawn_detached(flags: u32, cwd: Option<&std::path::Path>) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::*};
    let program: Vec<u16> = std::env::current_exe()?
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut argv: Vec<u16> = "fixture.exe --ignored --exact detached_payload --nocapture"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let cwd = cwd.map(|value| {
        value
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    });
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        CreateProcessW(
            program.as_ptr(),
            argv.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            flags,
            std::ptr::null(),
            cwd.as_ref().map_or(std::ptr::null(), |path| path.as_ptr()),
            &startup,
            &mut process,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

#[test]
#[ignore = "detached subprocess fixture"]
fn detached_payload() {
    std::fs::write("detached-ready.txt", "started").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write("detached-write.txt", "host child completed").unwrap();
}

#[tokio::test]
#[ignore = "trusted host fixture carrying a synthetic environment secret"]
async fn host_environment_payload() {
    use std::os::windows::io::AsRawHandle;
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
    use windows_sys::Win32::{
        Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT, SetHandleInformation},
        Storage::FileSystem::{FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx},
    };
    assert_eq!(
        std::env::var("WW_SECRET_CONTROL").unwrap(),
        "synthetic-host-only"
    );
    let workspace = Workspace::open(std::env::var("WW_FIXTURE_WORKSPACE").unwrap())
        .await
        .unwrap();
    let payload = workspace.path.join("payload.json");
    let mut data: Value = serde_json::from_slice(&std::fs::read(&payload).unwrap()).unwrap();
    let host_file = std::fs::File::open(data["outside"].as_str().unwrap()).unwrap();
    assert_ne!(
        unsafe {
            SetHandleInformation(
                host_file.as_raw_handle(),
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            )
        },
        0
    );
    let mut flags = 0;
    assert_ne!(
        unsafe { GetHandleInformation(host_file.as_raw_handle(), &mut flags) },
        0
    );
    assert_ne!(flags & HANDLE_FLAG_INHERIT, 0);
    let mut identity: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    assert_ne!(
        unsafe {
            GetFileInformationByHandleEx(
                host_file.as_raw_handle(),
                FileIdInfo,
                (&mut identity as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        },
        0
    );
    data["host_handle"] = json!(host_file.as_raw_handle() as usize as u64);
    data["handle_volume"] = json!(identity.VolumeSerialNumber);
    data["handle_id"] = json!(identity.FileId.Identifier);
    let pipe_name = format!(r"\\.\pipe\werkWorker-conformance-{}", uuid::Uuid::new_v4());
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)
        .unwrap();
    let control = ClientOptions::new().open(&pipe_name).unwrap();
    server.connect().await.unwrap();
    drop(control);
    server.disconnect().unwrap();
    let waiting = server.connect();
    tokio::pin!(waiting);
    tokio::select! {
        result = &mut waiting => panic!("unexpected host pipe connection: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(10)) => {},
    }
    data["host_pipe"] = json!(pipe_name);
    std::fs::write(&payload, data.to_string()).unwrap();
    let args = vec![
        workspace
            .path
            .join("fixture.exe")
            .to_string_lossy()
            .into_owned(),
        "--ignored".into(),
        "--exact".into(),
        "sandbox_payload".into(),
        "--nocapture".into(),
    ];
    let output = Sandbox::default()
        .run(&workspace, false, &args, ".", CancellationToken::new())
        .await
        .unwrap();
    // Invalid HANDLE queries can raise STATUS_INVALID_HANDLE under Windows'
    // strict handle checks. That is the expected missing-handle denial too;
    // the marker is emitted only after every file/network/IPC/env assertion.
    assert!(
        (output.exit_code == Some(0) && output.stdout.contains("PAYLOAD_ACCESS_OK"))
            || (output.exit_code == Some(0xc0000008u32 as i32)
                && output.stdout.contains("HANDLE_PROBE_STARTED")),
        "{output:?}"
    );
    // Prove the very same pipe still exists and accepts a trusted host client.
    let _control = ClientOptions::new().open(&pipe_name).unwrap();
    waiting.await.unwrap();
}

#[tokio::test]
#[ignore = "trusted host fixture for host-death and nested-job validation"]
async fn managed_host_payload() {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::{
        JobObjects::{AssignProcessToJobObject, CreateJobObjectW},
        Threading::GetCurrentProcess,
    };
    let workspace = Workspace::open(std::env::var("WW_FIXTURE_WORKSPACE").unwrap())
        .await
        .unwrap();
    let _outer_job = if std::env::var("WW_HOST_CASE").as_deref() == Ok("nested") {
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(!raw.is_null());
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        assert_ne!(
            unsafe { AssignProcessToJobObject(job.as_raw_handle(), GetCurrentProcess()) },
            0
        );
        Some(job)
    } else {
        None
    };
    let args = vec![
        workspace
            .path
            .join("fixture.exe")
            .to_string_lossy()
            .into_owned(),
        "--ignored".into(),
        "--exact".into(),
        "sandbox_payload".into(),
        "--nocapture".into(),
    ];
    let output = Sandbox::default()
        .run(&workspace, true, &args, ".", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(output.exit_code, Some(0), "{output:?}");
}

#[tokio::test]
async fn provisioning_is_explicit_and_never_changes_an_existing_workspace() {
    let existing = tempfile::tempdir().unwrap();
    std::fs::write(existing.path().join("keep"), "original").unwrap();
    assert!(
        Sandbox::default()
            .create_native_workspace(existing.path())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(existing.path().join("keep")).unwrap(),
        "original"
    );
    let unprepared = Workspace::open(existing.path()).await.unwrap();
    let result = Sandbox::default()
        .run(
            &unprepared,
            true,
            &["cmd.exe".into(), "/d".into(), "/c".into(), "exit 0".into()],
            ".",
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.unwrap_err().code, "sandbox_workspace_unprepared");
}

#[tokio::test]
async fn native_access_network_and_readonly_have_positive_controls() {
    let fixture = Fixture::new().await;
    fixture
        .workspace
        .write("allowed.txt", b"workspace data".to_vec(), None)
        .await
        .unwrap();
    let secret = fixture.parent.path().join("host-secret.txt");
    std::fs::write(&secret, "synthetic host secret").unwrap();
    assert_eq!(
        std::fs::read_to_string(&secret).unwrap(),
        "synthetic host secret"
    );
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = server.local_addr().unwrap();
    TcpStream::connect(address).unwrap(); // The negative child probe has a reachable positive control.
    for writable in [false, true] {
        fixture.input(json!({"mode":"access", "outside":secret, "server":address.to_string(), "host_home":std::env::var("USERPROFILE").unwrap(), "writable":writable}));
        let output = Sandbox::default()
            .run(
                &fixture.workspace,
                writable,
                &fixture.argv(),
                ".",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(output.exit_code, Some(0), "{output:?}");
        assert!(output.stdout.contains("PAYLOAD_ACCESS_OK"), "{output:?}");
    }
    fixture.input(json!({"mode":"access", "outside":secret, "server":address.to_string(), "host_home":std::env::var("USERPROFILE").unwrap(), "writable":false}));
    let control = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "host_environment_payload",
            "--nocapture",
        ])
        .env("WW_SECRET_CONTROL", "synthetic-host-only")
        .env("WW_FIXTURE_WORKSPACE", &fixture.workspace.path)
        .output()
        .unwrap();
    assert!(
        control.status.success(),
        "{}{}",
        String::from_utf8_lossy(&control.stdout),
        String::from_utf8_lossy(&control.stderr)
    );
    assert_eq!(
        fixture
            .workspace
            .read("process-created.txt", 100)
            .await
            .unwrap(),
        b"created by LPAC"
    );
    let old = fixture.workspace.read("allowed.txt", 100).await.unwrap();
    fixture
        .workspace
        .write("allowed.txt", b"updated".to_vec(), Some(hash(old)))
        .await
        .unwrap();
}

#[tokio::test]
async fn child_file_ownership_does_not_allow_permission_escalation() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new().await;
    fixture.input(json!({"mode":"owner-acl"}));
    let output = Sandbox::default()
        .run(
            &fixture.workspace,
            true,
            &fixture.argv(),
            ".",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.exit_code, Some(0), "{output:?}");
    assert!(output.stdout.contains("PAYLOAD_OWNER_ACL_DENIED"));
    // The trusted host retains ACL control over the same actual object.
    std::fs::OpenOptions::new()
        .access_mode(windows_sys::Win32::Storage::FileSystem::WRITE_DAC)
        .open(fixture.workspace.path.join("owned-by-child.txt"))
        .unwrap();
}

#[tokio::test]
async fn explicit_stdin_unicode_arguments_and_output_bounds() {
    let fixture = Fixture::new().await;
    fixture.input(json!({"mode":"stdin"}));
    let mut argv = fixture.argv();
    for argument in [
        "",
        "space value",
        "quote\"value",
        "trailing \\",
        "Grüße 世界",
    ] {
        argv.push(argument.into());
    }
    let result = Sandbox::default()
        .run_with_stdin(
            &fixture.workspace,
            false,
            &argv,
            ".",
            CancellationToken::new(),
            "first line\nGrüße 世界\n",
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert!(result.stdout.contains("PAYLOAD_STDIN_ARGV_OK"));
    fixture.input(json!({"mode":"output"}));
    let mut sandbox = Sandbox::default();
    sandbox.limits.output_bytes = 512;
    let result = sandbox
        .run(
            &fixture.workspace,
            false,
            &fixture.argv(),
            ".",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert!(result.truncated);
    assert!(result.stdout.len() <= 512 && result.stderr.len() <= 512);
}

#[tokio::test]
async fn timeout_cancel_and_child_process_denial_are_enforced() {
    let fixture = Fixture::new().await;
    // The exact child fixture runs successfully outside the LPAC, including
    // a detached process group, so sandbox denial has a positive host control.
    for flags in [0, 0x00000008 | 0x00000200] {
        spawn_detached(flags, Some(&fixture.workspace.path)).unwrap();
        for _ in 0..250 {
            if fixture.workspace.path.join("detached-write.txt").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(fixture.workspace.path.join("detached-ready.txt").exists());
        assert!(fixture.workspace.path.join("detached-write.txt").exists());
        std::fs::remove_file(fixture.workspace.path.join("detached-ready.txt")).unwrap();
        std::fs::remove_file(fixture.workspace.path.join("detached-write.txt")).unwrap();
    }
    fixture.input(json!({"mode":"sleep"}));
    let mut sandbox = Sandbox::default();
    sandbox.limits.timeout_ms = 200;
    let result = sandbox
        .run(
            &fixture.workspace,
            true,
            &fixture.argv(),
            ".",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(result.timed_out && !result.cancelled);
    assert!(!fixture.workspace.path.join("late-write.txt").exists());
    let jobs = JobManager::default();
    let job = jobs
        .start(
            "native-run",
            Sandbox::default(),
            fixture.workspace.clone(),
            true,
            fixture.argv(),
            ".".into(),
        )
        .await
        .unwrap();
    for _ in 0..100 {
        let log = jobs
            .log("native-run", &job, "stdout", None, 4096)
            .await
            .unwrap();
        if log["text"].as_str().unwrap().contains("STARTED") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    jobs.status("native-run", &job, true).await.unwrap();
    let result = jobs
        .wait("native-run", &job, 5000, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result["output"]["cancelled"], true, "{result}");
    fixture.input(json!({"mode":"descendant"}));
    let result = Sandbox::default()
        .run(
            &fixture.workspace,
            true,
            &fixture.argv(),
            ".",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert!(result.stdout.contains("PAYLOAD_CHILD_CREATION_DENIED"));
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert!(!fixture.workspace.path.join("detached-write.txt").exists());
}

#[tokio::test]
async fn host_death_kills_the_job_and_existing_host_jobs_can_nest() {
    let fixture = Fixture::new().await;
    fixture.input(json!({"mode":"sleep", "delay_ms":500}));
    let host = |case: &str| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "managed_host_payload",
                "--nocapture",
            ])
            .env("WW_FIXTURE_WORKSPACE", &fixture.workspace.path)
            .env("WW_HOST_CASE", case);
        command
    };
    let control = host("nested").output().unwrap();
    assert!(
        control.status.success(),
        "{}{}",
        String::from_utf8_lossy(&control.stdout),
        String::from_utf8_lossy(&control.stderr)
    );
    assert!(fixture.workspace.path.join("late-write.txt").exists());
    std::fs::remove_file(fixture.workspace.path.join("late-write.txt")).unwrap();
    std::fs::remove_file(fixture.workspace.path.join("process-ready.txt")).unwrap();
    let mut process = host("crash")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..250 {
        if fixture.workspace.path.join("process-ready.txt").exists() {
            break;
        }
        if let Some(status) = process.try_wait().unwrap() {
            panic!("host exited before child startup: {status}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let started = fixture.workspace.path.join("process-ready.txt").exists();
    process.kill().unwrap();
    process.wait().unwrap();
    assert!(started, "child did not reach the ready control");
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert!(!fixture.workspace.path.join("late-write.txt").exists());
}

#[tokio::test]
async fn job_memory_and_cpu_limits_are_enforced_by_windows() {
    let fixture = Fixture::new().await;
    for (limit, allowed) in [(1024 * 1024 * 1024, true), (64 * 1024 * 1024, false)] {
        fixture.input(json!({"mode":"memory", "allowed":allowed}));
        let mut sandbox = Sandbox::default();
        sandbox.limits.memory_bytes = limit;
        let output = sandbox
            .run(
                &fixture.workspace,
                false,
                &fixture.argv(),
                ".",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(output.exit_code, Some(0), "{output:?}");
        assert!(output.stdout.contains("PAYLOAD_MEMORY_CHECKED"));
    }
    fixture.input(json!({"mode":"cpu"}));
    let mut sandbox = Sandbox::default();
    sandbox.limits.cpu_seconds = 1;
    sandbox.limits.timeout_ms = 10_000;
    let output = sandbox
        .run(
            &fixture.workspace,
            false,
            &fixture.argv(),
            ".",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(output.stdout.contains("PAYLOAD_CPU_STARTED"));
    assert!(!output.timed_out && !output.cancelled, "{output:?}");
    assert_ne!(output.exit_code, Some(0), "{output:?}");
}
