//! Real Apple Silicon tests of the explicitly limited Seatbelt mode.
//! These tests never skip a failed isolation probe; run natively on macOS 15+.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::{os::fd::AsRawFd, path::PathBuf, process::Command, time::Duration};
use tokio_util::sync::CancellationToken;
use werk_worker::runtime::{NativeSandboxConfig, Sandbox, Workspace};

struct Fixture {
    root: tempfile::TempDir,
    workspace: Workspace,
    probe: PathBuf,
    sandbox: Sandbox,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace_path = root.path().join("workspace ü with spaces");
        std::fs::create_dir(&workspace_path).unwrap();
        let probe = workspace_path.join("native probe");
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/native_process_probe.rs");
        let built = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2024", "-C", "debuginfo=0"])
            .arg(source)
            .arg("-o")
            .arg(&probe)
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let workspace = Workspace::open(&workspace_path).await.unwrap();
        let sandbox = Sandbox {
            native: NativeSandboxConfig {
                macos_path_based_workspace: true,
            },
            // The test host already has a VM map before fork. The backend must
            // reject a requested limit it cannot lower, rather than ignore it.
            limits: werk_worker::runtime::ProcessLimits {
                memory_bytes: 1024 * 1024 * 1024 * 1024,
                ..Default::default()
            },
            ..Default::default()
        };
        Self {
            root,
            workspace,
            probe,
            sandbox,
        }
    }

    async fn run(&self, writable: bool, arguments: &[&str]) -> werk_worker::runtime::ProcessOutput {
        let argv: Vec<_> = std::iter::once(self.probe.to_string_lossy().into_owned())
            .chain(arguments.iter().map(|value| (*value).to_owned()))
            .collect();
        self.sandbox
            .run(
                &self.workspace,
                writable,
                &argv,
                ".",
                CancellationToken::new(),
            )
            .await
            .expect("native sandbox must be available and enforce required limits")
    }

    fn outside(&self, arguments: &[&str]) -> std::process::Output {
        Command::new(&self.probe)
            .args(arguments)
            .current_dir(&self.workspace.path)
            .output()
            .unwrap()
    }
}

#[tokio::test]
async fn native_mode_requires_explicit_trusted_host_selection() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(root.path()).await.unwrap();
    let error = Sandbox::default()
        .run(
            &workspace,
            false,
            &["/usr/bin/true".into()],
            ".",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "sandbox_requirement_unavailable");
}

#[tokio::test]
async fn impossible_address_space_limit_fails_before_untrusted_code() {
    let mut fixture = Fixture::new().await;
    fixture.sandbox.limits.memory_bytes = 1;
    let argv = vec![
        fixture.probe.to_string_lossy().into_owned(),
        "write".into(),
        "must-not-run".into(),
        "escaped setup failure".into(),
    ];
    let error = fixture
        .sandbox
        .run(
            &fixture.workspace,
            true,
            &argv,
            ".",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "sandbox_requirement_unavailable");
    assert!(!fixture.workspace.path.join("must-not-run").exists());
}

#[tokio::test]
async fn read_write_stdin_argv_and_exit_status_are_native() {
    let fixture = Fixture::new().await;
    let contract = fixture
        .sandbox
        .execution_description(&fixture.workspace, false)
        .await
        .unwrap();
    assert_eq!(contract["configuration_version"], 2);
    assert_eq!(
        contract["host_requirements"],
        "unprivileged-uid;host-port-IKOT_HOST-only"
    );
    let mut legacy = contract.clone();
    legacy["configuration_version"] = 1.into();
    assert_eq!(
        fixture
            .sandbox
            .verify_execution_description(&fixture.workspace, false, &legacy)
            .await
            .unwrap_err()
            .code,
        "sandbox_contract_changed"
    );
    let echo = fixture
        .run(
            false,
            &["echo", "", "space value", "Grüße 世界", "$(not-a-shell)"],
        )
        .await;
    assert_eq!(echo.exit_code, Some(0), "{}", echo.stderr);
    assert_eq!(
        echo.stdout,
        "0:\n11:space value\n14:Grüße 世界\n14:$(not-a-shell)\n"
    );
    let argv = vec![fixture.probe.to_string_lossy().into_owned(), "stdin".into()];
    let input = "literal stdin\n'\"$(); 世界";
    let output = fixture
        .sandbox
        .run_with_stdin(
            &fixture.workspace,
            false,
            &argv,
            ".",
            CancellationToken::new(),
            input,
        )
        .await
        .unwrap();
    assert_eq!(output.stdout, input);
    assert_eq!(output.exit_code, Some(0));
    let written = fixture
        .run(true, &["write", "created.txt", "native contents"])
        .await;
    assert_eq!(written.exit_code, Some(0), "{}", written.stderr);
    assert_eq!(
        fixture.workspace.read("created.txt", 100).await.unwrap(),
        b"native contents"
    );
    assert_eq!(
        fixture.run(false, &["read", "created.txt"]).await.stdout,
        "native contents"
    );
    let denied = fixture
        .run(false, &["write", "created.txt", "forbidden"])
        .await;
    assert_ne!(denied.exit_code, Some(0));
    assert_eq!(
        fixture.workspace.read("created.txt", 100).await.unwrap(),
        b"native contents"
    );
    // 125 is a valid application exit, distinct from the supervisor's setup pipe.
    assert_eq!(
        fixture.run(false, &["exit", "125"]).await.exit_code,
        Some(125)
    );
}

#[tokio::test]
async fn outside_files_network_and_host_ipc_have_positive_controls() {
    let fixture = Fixture::new().await;
    let secret = fixture.root.path().join("synthetic-host-secret");
    std::fs::write(&secret, "only a test marker").unwrap();
    let secret = secret.to_str().unwrap();
    assert!(fixture.outside(&["read", secret]).status.success());
    assert_ne!(
        fixture.run(false, &["read", secret]).await.exit_code,
        Some(0)
    );
    assert_ne!(
        fixture.run(false, &["exec-read", secret]).await.exit_code,
        Some(0)
    );
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = tcp.local_addr().unwrap().to_string();
    assert!(fixture.outside(&["tcp", &address]).status.success());
    assert_ne!(
        fixture.run(false, &["tcp", &address]).await.exit_code,
        Some(0)
    );
    let socket = fixture.root.path().join("host.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let socket = socket.to_str().unwrap();
    assert!(fixture.outside(&["unix-socket", socket]).status.success());
    assert_ne!(
        fixture.run(false, &["unix-socket", socket]).await.exit_code,
        Some(0)
    );
    let secret_fd = std::fs::File::open(secret).unwrap();
    // Deliberately inheritable host descriptor: the native child must close it.
    assert_eq!(
        unsafe { libc::fcntl(secret_fd.as_raw_fd(), libc::F_SETFD, 0) },
        0
    );
    let number = secret_fd.as_raw_fd().to_string();
    assert!(fixture.outside(&["inherited-fd", &number]).status.success());
    assert_ne!(
        fixture
            .run(false, &["inherited-fd", &number])
            .await
            .exit_code,
        Some(0)
    );
}

#[tokio::test]
async fn forks_detaches_and_posix_spawn_are_denied_before_descendant_code() {
    let fixture = Fixture::new().await;
    let positive = fixture.workspace.path.join("outside-detached");
    assert!(
        fixture
            .outside(&["fork-detach", positive.to_str().unwrap()])
            .status
            .success()
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        positive.exists(),
        "positive control must prove detach/write works on host"
    );
    let marker = fixture.workspace.path.join("sandbox-detached");
    assert_eq!(
        fixture
            .run(true, &["fork-detach", marker.to_str().unwrap()])
            .await
            .exit_code,
        Some(77)
    );
    assert!(!marker.exists());
    assert!(
        fixture
            .outside(&["spawn", "/usr/bin/true"])
            .status
            .success()
    );
    assert_eq!(
        fixture
            .run(false, &["spawn", "/usr/bin/true"])
            .await
            .exit_code,
        Some(77)
    );
}

#[tokio::test]
async fn timeout_and_cancellation_reap_before_reporting_completion() {
    let mut fixture = Fixture::new().await;
    fixture.sandbox.limits.timeout_ms = 100;
    let timed = fixture
        .run(true, &["late-write", "ready", "late", "1000"])
        .await;
    assert!(timed.timed_out && !timed.cancelled);
    assert!(!fixture.workspace.path.join("late").exists());
    fixture.sandbox.limits.timeout_ms = 10_000;
    let cancel = CancellationToken::new();
    let child_cancel = cancel.clone();
    let ready = fixture.workspace.path.join("cancel-ready");
    let argv = vec![
        fixture.probe.to_string_lossy().into_owned(),
        "late-write".into(),
        "cancel-ready".into(),
        "cancel-late".into(),
        "1000".into(),
    ];
    let monitor = async {
        for _ in 0..500 {
            if ready.exists() {
                cancel.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("native probe never started");
    };
    let (output, _) = tokio::join!(
        fixture
            .sandbox
            .run(&fixture.workspace, true, &argv, ".", child_cancel),
        monitor
    );
    let output = output.unwrap();
    assert!(output.cancelled && !output.timed_out);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(!fixture.workspace.path.join("cancel-late").exists());
    assert!(!fixture.workspace.path.join("late").exists());
}

#[tokio::test]
async fn host_exit_fixture() {
    let Some(root) = std::env::var_os("WERK_MACOS_HOST_EXIT_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let workspace = Workspace::open(&root).await.unwrap();
    let sandbox = Sandbox {
        native: NativeSandboxConfig {
            macos_path_based_workspace: true,
        },
        limits: werk_worker::runtime::ProcessLimits {
            memory_bytes: 1024 * 1024 * 1024 * 1024,
            timeout_ms: 10_000,
            ..Default::default()
        },
        ..Default::default()
    };
    // This embedding-host subprocess receives the synthetic secret, while its
    // sandboxed process must receive only the backend's fixed environment.
    assert_eq!(
        std::env::var("WERK_NATIVE_TEST_SECRET").unwrap(),
        "synthetic environment marker"
    );
    let probe = root.join("native probe");
    // Register an actual synthetic Mach send right in this isolated host.
    // A regular child must inherit it and send the marker to our receive right.
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn mach_port_allocate(task: u32, right: i32, port: *mut u32) -> i32;
        fn mach_port_insert_right(task: u32, name: u32, right: u32, kind: u32) -> i32;
        fn mach_ports_register(task: u32, ports: *mut u32, count: u32) -> i32;
        fn mach_msg(
            message: *mut std::ffi::c_void,
            option: u32,
            send_size: u32,
            receive_limit: u32,
            receive_name: u32,
            timeout: u32,
            notify: u32,
        ) -> i32;
    }
    let task = unsafe { mach_task_self_ };
    let mut host_port = 0;
    assert_eq!(unsafe { mach_port_allocate(task, 1, &mut host_port) }, 0);
    assert_eq!(
        unsafe { mach_port_insert_right(task, host_port, host_port, 20) },
        0
    );
    assert_eq!(unsafe { mach_ports_register(task, &mut host_port, 1) }, 0);
    let outside_mach = Command::new(&probe).arg("mach-state").output().unwrap();
    assert_eq!(outside_mach.status.code(), Some(77));
    assert!(
        String::from_utf8(outside_mach.stdout)
            .unwrap()
            .contains("registered=true")
    );
    let mut received = [0_u32; 32];
    assert_eq!(
        unsafe {
            mach_msg(
                received.as_mut_ptr().cast(),
                2 | 0x100,
                0,
                std::mem::size_of_val(&received) as u32,
                host_port,
                1000,
                0,
            )
        },
        0
    );
    assert_eq!(
        received[5], 1112,
        "positive control did not reach host Mach endpoint"
    );
    let inside_mach = sandbox
        .run(
            &workspace,
            false,
            &[probe.to_string_lossy().into_owned(), "mach-state".into()],
            ".",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(inside_mach.exit_code, Some(0), "{}", inside_mach.stderr);
    assert_eq!(
        inside_mach.stdout.trim(),
        "registered=false;bootstrap=false;exceptions=false"
    );
    let outside = Command::new(&probe)
        .args(["env", "WERK_NATIVE_TEST_SECRET"])
        .output()
        .unwrap();
    assert_eq!(outside.status.code(), Some(77));
    assert_eq!(
        String::from_utf8(outside.stdout).unwrap().trim(),
        "synthetic environment marker"
    );
    let inside = sandbox
        .run(
            &workspace,
            false,
            &[
                probe.to_string_lossy().into_owned(),
                "env".into(),
                "WERK_NATIVE_TEST_SECRET".into(),
            ],
            ".",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(inside.exit_code, Some(0), "{}", inside.stderr);
    assert!(inside.stdout.is_empty());
    let argv = vec![
        root.join("native probe").to_string_lossy().into_owned(),
        "late-write".into(),
        "crash-ready".into(),
        "crash-late".into(),
        "1500".into(),
    ];
    sandbox
        .run(&workspace, true, &argv, ".", CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn supervisor_survives_embedding_host_sigkill_and_prevents_late_write() {
    let fixture = Fixture::new().await;
    let mut host = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "host_exit_fixture", "--nocapture"])
        .env("WERK_MACOS_HOST_EXIT_FIXTURE", &fixture.workspace.path)
        .env("WERK_NATIVE_TEST_SECRET", "synthetic environment marker")
        .spawn()
        .unwrap();
    let ready = fixture.workspace.path.join("crash-ready");
    for _ in 0..500 {
        if ready.exists() {
            break;
        }
        assert!(
            host.try_wait().unwrap().is_none(),
            "fixture host failed before starting a process"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ready.exists());
    host.kill().unwrap();
    host.wait().unwrap();
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert!(!fixture.workspace.path.join("crash-late").exists());
}

// All host-port mutations live in a separate test process. Never change the
// ports of the parallel test harness or its embedding Tokio runtime.
mod host_ports {
    // mach/task_special_ports.h: selector 2, distinct from IKOT_HOST (type 3).
    pub const TASK_HOST_PORT: i32 = 2;
    unsafe extern "C" {
        pub static mach_task_self_: u32;
        pub fn mach_port_deallocate(task: u32, name: u32) -> i32;
        pub fn mach_port_kobject(task: u32, name: u32, kind: *mut u32, address: *mut u64) -> i32;
        pub fn mach_port_allocate(task: u32, right: i32, port: *mut u32) -> i32;
        pub fn mach_port_insert_right(task: u32, name: u32, right: u32, kind: u32) -> i32;
        pub fn task_get_special_port(task: u32, which: i32, port: *mut u32) -> i32;
        pub fn task_set_special_port(task: u32, which: i32, port: u32) -> i32;
        pub fn mach_msg(
            message: *mut std::ffi::c_void,
            options: u32,
            send: u32,
            receive: u32,
            port: u32,
            timeout: u32,
            notify: u32,
        ) -> i32;
    }

    pub unsafe fn endpoint() -> u32 {
        let task = unsafe { mach_task_self_ };
        let mut port = 0;
        // No allocation/panic in the atfork child callback.
        if unsafe { mach_port_allocate(task, 1, &mut port) } != 0
            || unsafe { mach_port_insert_right(task, port, port, 20) } != 0
        {
            unsafe { libc::_exit(119) }
        }
        port
    }

    pub extern "C" fn replace_after_fork() {
        unsafe {
            let port = endpoint();
            if task_set_special_port(mach_task_self_, TASK_HOST_PORT, port) != 0 {
                libc::_exit(119);
            }
        }
    }
}

#[tokio::test]
async fn custom_host_port_fixture() {
    let Some(mode) = std::env::var_os("WERK_MACOS_HOST_PORT_FIXTURE") else {
        return; // subprocess entry point; invoked by both mandatory parent tests
    };
    use host_ports::*;
    let fixture = Fixture::new().await;
    let task = unsafe { mach_task_self_ };
    let mut normal = 0;
    assert_eq!(
        unsafe { task_get_special_port(task, TASK_HOST_PORT, &mut normal) },
        0
    );
    #[allow(deprecated)]
    let host_self = unsafe { libc::mach_host_self() };
    assert_ne!(host_self, 0);
    assert_eq!(
        normal, host_self,
        "fixture selector must identify the actual host port"
    );
    let released = unsafe { mach_port_deallocate(task, host_self) };
    assert_eq!(released, 0);
    let endpoint = unsafe { endpoint() };
    let installed = unsafe { task_set_special_port(task, TASK_HOST_PORT, endpoint) };
    if installed != 0 {
        // A protected developer Mac must enforce the kernel's own denial.
        // Hosted CI requires the custom-port case so it cannot pass by taking
        // this branch instead of reproducing its observed SIP configuration.
        assert_ne!(
            std::env::var("WERK_REQUIRE_CUSTOM_MACH_HOST_PORT").as_deref(),
            Ok("1"),
            "CI must exercise custom host-port injection; status={installed}"
        );
        assert_eq!(installed, 8, "expected KERN_NO_ACCESS from SIP");
        assert_eq!(fixture.run(false, &["exit", "0"]).await.exit_code, Some(0));
        return;
    }

    // Positive control: the installed custom slot is a working IPC endpoint in
    // this isolated parent. Verify its type before sending a userspace message.
    let mut custom = 0;
    let queried = unsafe { task_get_special_port(task, TASK_HOST_PORT, &mut custom) };
    let mut kind = u32::MAX;
    let mut address = 0_u64;
    let inspected = unsafe { mach_port_kobject(task, custom, &mut kind, &mut address) };
    let sent = if queried == 0 && inspected == 0 && kind == 0 {
        let mut message = [19_u32, 24, custom, 0, 0, 1113];
        unsafe { mach_msg(message.as_mut_ptr().cast(), 1 | 0x10, 24, 0, 0, 1000, 0) }
    } else {
        -1
    };
    let released = unsafe { mach_port_deallocate(task, custom) };

    // BSD fork refreshes the child's security tokens and replaces its host
    // slot with the ordinary kernel host port. Check that behavior directly;
    // sending the userspace marker to that kernel port would trigger a fatal
    // Mach message-type guard. The atfork negative case below injects its
    // custom port after this kernel reset, before the supervisor validates it.
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe {
            let child_task = mach_task_self_;
            let mut child_host = 0;
            if task_get_special_port(child_task, TASK_HOST_PORT, &mut child_host) != 0 {
                libc::_exit(118);
            }
            let mut child_kind = u32::MAX;
            let mut child_address = 0_u64;
            if mach_port_kobject(child_task, child_host, &mut child_kind, &mut child_address) != 0 {
                libc::_exit(117);
            }
            if child_kind != 3 {
                // IKOT_HOST, not the custom userspace endpoint
                libc::_exit(116);
            }
            if mach_port_deallocate(child_task, child_host) != 0 {
                libc::_exit(115);
            }
            libc::_exit(0);
        }
    }
    assert_eq!(
        unsafe { task_set_special_port(task, TASK_HOST_PORT, normal) },
        0
    );
    assert!(child > 0);
    let mut status = 0;
    let waited = loop {
        let result = unsafe { libc::waitpid(child, &mut status, 0) };
        if result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break result;
    };
    assert_eq!(waited, child);
    let exit = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
    let signal = libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status));
    assert_eq!(
        exit,
        Some(0),
        "fork host-port control failed: exit={exit:?}, signal={signal:?}, raw_status={status}"
    );
    assert_eq!(queried, 0);
    assert_eq!(custom, endpoint);
    assert_eq!(inspected, 0);
    assert_eq!(kind, 0, "installed host slot must be a userspace endpoint");
    assert_eq!(sent, 0, "custom host endpoint must accept the marker");
    assert_eq!(released, 0);
    let mut received = [0_u32; 32];
    let received_status = unsafe {
        mach_msg(
            received.as_mut_ptr().cast(),
            2 | 0x100,
            0,
            std::mem::size_of_val(&received) as u32,
            endpoint,
            1000,
            0,
        )
    };
    assert_eq!(received_status, 0);
    assert_eq!(
        received[5], 1113,
        "custom host endpoint must receive the marker"
    );

    // The same normal host successfully runs isolated code before tampering.
    assert_eq!(fixture.run(false, &["exit", "0"]).await.exit_code, Some(0));
    if mode == "after-fork" {
        assert_eq!(
            unsafe { libc::pthread_atfork(None, None, Some(replace_after_fork)) },
            0
        );
    } else {
        assert_eq!(mode, "before-launch");
        assert_eq!(
            unsafe { task_set_special_port(task, TASK_HOST_PORT, endpoint) },
            0
        );
    }
    let argv = vec![
        fixture.probe.to_string_lossy().into_owned(),
        "write".into(),
        "must-not-run".into(),
        "escaped host port".into(),
    ];
    let result = fixture
        .sandbox
        .run(
            &fixture.workspace,
            true,
            &argv,
            ".",
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        unsafe { task_set_special_port(task, TASK_HOST_PORT, normal) },
        0
    );
    let error = result.unwrap_err();
    assert_eq!(error.code, "sandbox_requirement_unavailable");
    assert!(error.message.contains("stage=9, detail=0"), "{error:?}");
    assert!(!fixture.workspace.path.join("must-not-run").exists());
    // No sandbox payload may send to the host's custom endpoint either.
    assert_eq!(
        unsafe {
            mach_msg(
                received.as_mut_ptr().cast(),
                2 | 0x100,
                0,
                std::mem::size_of_val(&received) as u32,
                endpoint,
                0,
                0,
            )
        },
        0x10004003
    ); // MACH_RCV_TIMED_OUT
}

fn check_custom_host_port(mode: &str) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "custom_host_port_fixture", "--nocapture"])
        .env("WERK_MACOS_HOST_PORT_FIXTURE", mode)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{mode}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn custom_host_port_is_rejected_before_launch() {
    check_custom_host_port("before-launch");
}

#[test]
fn custom_host_port_in_fork_snapshot_is_rejected() {
    check_custom_host_port("after-fork");
}
