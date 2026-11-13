//! Synthetic native process used by sandbox conformance tests, never shipped.
//! Build with `rustc --edition=2024 native_process_probe.rs -o <workspace>/probe`.
use std::{
    io::{self, Read, Write},
    time::Duration,
};

fn attempt(operation: impl FnOnce() -> io::Result<()>) {
    if let Err(error) = operation() {
        eprintln!("probe denied/error: {error}");
        std::process::exit(77);
    }
}

#[cfg(target_os = "macos")]
const KERN_DENIED: i32 = 53; // mach/kern_return.h; SDK-checked by the native library tests

#[cfg(target_os = "macos")]
fn task_access_state() -> (i32, bool) {
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn task_get_special_port(task: u32, which: i32, port: *mut u32) -> i32;
        fn mach_port_type(task: u32, name: u32, kind: *mut u32) -> i32;
        fn mach_port_deallocate(task: u32, name: u32) -> i32;
    }
    let task = unsafe { mach_task_self_ };
    let mut access = 0;
    let access_get = unsafe { task_get_special_port(task, 9, &mut access) };
    if access_get != 0 && access != 0 {
        std::process::exit(79);
    }
    let mut access_live = false;
    if access_get == 0 && access != 0 && access != u32::MAX {
        let mut kind = 0;
        if unsafe { mach_port_type(task, access, &mut kind) } != 0 {
            std::process::exit(79);
        }
        access_live = kind & 0x10000 != 0; // MACH_PORT_TYPE_SEND
        if unsafe { mach_port_deallocate(task, access) } != 0 {
            std::process::exit(79);
        }
    }
    // This observes only the right's liveness. Authentication of the retained
    // system endpoint belongs to the trusted launcher, before this code runs.
    (access_get, access_live)
}

#[cfg(target_os = "macos")]
fn mach_special_ports() {
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn task_set_special_port(task: u32, which: i32, port: u32) -> i32;
    }
    let (access_get, access_live) = task_access_state();
    // Clearing this isolated payload's bootstrap slot is a valid operation
    // outside Seatbelt. Inside, the same call must return the exact MAC denial.
    let bootstrap_set = unsafe { task_set_special_port(mach_task_self_, 4, 0) };
    println!("access_get={access_get};access_live={access_live};bootstrap_set={bootstrap_set}");
}

#[cfg(target_os = "macos")]
fn mach_state() {
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn mach_ports_lookup(task: u32, ports: *mut *mut u32, count: *mut u32) -> i32;
        fn task_get_special_port(task: u32, which: i32, port: *mut u32) -> i32;
        fn task_get_exception_ports(
            task: u32,
            mask: u32,
            masks: *mut u32,
            count: *mut u32,
            ports: *mut u32,
            behaviors: *mut i32,
            flavors: *mut i32,
        ) -> i32;
        fn mach_msg(
            message: *mut Header,
            option: u32,
            send_size: u32,
            receive_limit: u32,
            receive_name: u32,
            timeout: u32,
            notify: u32,
        ) -> i32;
    }
    #[repr(C)]
    struct Header {
        bits: u32,
        size: u32,
        remote: u32,
        local: u32,
        voucher: u32,
        id: i32,
    }
    let task = unsafe { mach_task_self_ };
    let mut ports = std::ptr::null_mut();
    let mut count = 0;
    if unsafe { mach_ports_lookup(task, &mut ports, &mut count) } != 0
        || ports.is_null()
        || count > 32
    {
        std::process::exit(79);
    }
    let registered = unsafe { std::slice::from_raw_parts(ports, count as usize) };
    let inherited = registered.iter().copied().find(|port| *port != 0);
    if let Some(port) = inherited {
        let mut message = Header {
            bits: 19,
            size: 24,
            remote: port,
            local: 0,
            voucher: 0,
            id: 1112,
        };
        // Positive control sends a real message through the inherited right.
        let sent = unsafe { mach_msg(&mut message, 1 | 0x10, 24, 0, 0, 1000, 0) };
        if sent != 0 {
            std::process::exit(78);
        }
    }
    let mut bootstrap = 0;
    let bootstrap_status = unsafe { task_get_special_port(task, 4, &mut bootstrap) };
    if bootstrap_status != 0 && bootstrap_status != KERN_DENIED {
        std::process::exit(79);
    }
    let (access_status, access_live) = task_access_state();
    let access_retained = access_status == 0 && access_live;
    let mut masks = [0_u32; 32];
    let mut exceptions = [0_u32; 32];
    let mut behaviors = [0_i32; 32];
    let mut flavors = [0_i32; 32];
    let mut count = 32;
    if unsafe {
        task_get_exception_ports(
            task,
            0x3ffe,
            masks.as_mut_ptr(),
            &mut count,
            exceptions.as_mut_ptr(),
            behaviors.as_mut_ptr(),
            flavors.as_mut_ptr(),
        )
    } != 0
        || count > 32
    {
        std::process::exit(79);
    }
    let exceptional = exceptions[..count as usize].iter().any(|port| *port != 0);
    println!(
        "registered={};bootstrap={};access_retained={access_retained};exceptions={exceptional}",
        inherited.is_some(),
        bootstrap != 0
    );
    if inherited.is_some() || bootstrap != 0 || !access_retained || exceptional {
        std::process::exit(77);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    match mode {
        #[cfg(target_os = "macos")]
        "mach-state" => mach_state(),
        #[cfg(target_os = "macos")]
        "mach-special-ports" => mach_special_ports(),
        #[cfg(target_os = "macos")]
        "exec-mach-special-ports" => {
            use std::os::unix::process::CommandExt;
            let error = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("mach-special-ports")
                .exec();
            eprintln!("self exec failed: {error}");
            std::process::exit(79);
        }
        "echo" => {
            for value in &args[2..] {
                println!("{}:{value}", value.len());
            }
        }
        "stdin" => attempt(|| {
            let mut data = Vec::new();
            io::stdin().read_to_end(&mut data)?;
            io::stdout().write_all(&data)
        }),
        "read" => attempt(|| io::stdout().write_all(&std::fs::read(&args[2])?)),
        "write" => attempt(|| std::fs::write(&args[2], &args[3])),
        "test-add" => attempt(|| {
            let source = std::fs::read_to_string(&args[2])?;
            let checks = std::fs::read_to_string(&args[3])?;
            let mut passed = 0;
            for line in checks.lines() {
                let numbers: Vec<i32> = line
                    .split(',')
                    .map(|value| value.parse().unwrap())
                    .collect();
                assert_eq!(numbers.len(), 3);
                let actual = match source.trim() {
                    "a+b" => numbers[0] + numbers[1],
                    "a-b" => numbers[0] - numbers[1],
                    _ => return Err(io::Error::other("invalid expression")),
                };
                assert_eq!(actual, numbers[2], "addition behavior for {line}");
                passed += 1;
            }
            assert!(passed > 0);
            println!("{passed} checks passed");
            Ok(())
        }),
        "tcp" => attempt(|| std::net::TcpStream::connect(&args[2]).map(|_| ())),
        "env" => {
            if let Some(value) = std::env::var_os(&args[2]) {
                println!("{}", value.to_string_lossy());
                std::process::exit(77);
            }
        }
        "sleep" => std::thread::sleep(Duration::from_millis(args[2].parse().unwrap())),
        "gated-write" => attempt(|| {
            println!("native job ready");
            io::stdout().flush()?;
            std::fs::write(&args[2], "ready").map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "ready marker {} in {:?}: {error}",
                        args[2],
                        std::env::current_dir()
                    ),
                )
            })?;
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !std::path::Path::new(&args[3]).exists() {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::other("test gate was not released"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            std::fs::write(&args[4], "finished").map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("completion marker {}: {error}", args[4]),
                )
            })?;
            println!("native job finished");
            Ok(())
        }),
        "late-write" => attempt(|| {
            std::fs::write(&args[2], std::process::id().to_string())?;
            std::thread::sleep(Duration::from_millis(args[4].parse().unwrap()));
            std::fs::write(&args[3], "late write")
        }),
        "cwd" => println!("{}", std::env::current_dir().unwrap().display()),
        "exit" => std::process::exit(args[2].parse().unwrap()),
        "spawn" | "spawn-wait" => attempt(|| {
            let status = std::process::Command::new(&args[2])
                .args(&args[3..])
                .status()?;
            if status.success() {
                Ok(())
            } else {
                Err(io::Error::other("child failed"))
            }
        }),
        #[cfg(windows)]
        "inherited-handle" => attempt(|| {
            use std::os::windows::io::FromRawHandle;
            let handle: usize = args[2].parse().unwrap();
            let mut input = unsafe { std::fs::File::from_raw_handle(handle as *mut _) };
            let mut data = [0_u8; 64];
            let result = input.read(&mut data);
            std::mem::forget(input);
            io::stdout().write_all(&data[..result?])
        }),
        #[cfg(unix)]
        "unix-socket" => attempt(|| std::os::unix::net::UnixStream::connect(&args[2]).map(|_| ())),
        #[cfg(unix)]
        "inherited-fd" => attempt(|| {
            use std::os::fd::FromRawFd;
            let descriptor: i32 = args[2].parse().unwrap();
            let mut input = unsafe { std::fs::File::from_raw_fd(descriptor) };
            let mut data = Vec::new();
            let result = input.read_to_end(&mut data);
            // File::drop intentionally rejects EBADF in debug builds; this
            // probe also tests an intentionally closed descriptor number.
            std::mem::forget(input);
            result?;
            io::stdout().write_all(&data)
        }),
        #[cfg(unix)]
        "exec-read" => {
            use std::os::unix::process::CommandExt;
            let error = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["read", &args[2]])
                .exec();
            eprintln!("self exec failed: {error}");
            // 77 belongs to the actual read denial in the replacement process.
            std::process::exit(79);
        }
        #[cfg(unix)]
        "fork-detach" => {
            unsafe extern "C" {
                fn fork() -> i32;
                fn setsid() -> i32;
                fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
                fn _exit(code: i32) -> !;
            }
            let child = unsafe { fork() };
            if child < 0 {
                eprintln!("fork denied: {}", io::Error::last_os_error());
                std::process::exit(77);
            }
            if child == 0 {
                if unsafe { setsid() } < 0 {
                    unsafe { _exit(78) }
                }
                let grandchild = unsafe { fork() };
                if grandchild < 0 {
                    unsafe { _exit(79) }
                }
                if grandchild != 0 {
                    unsafe { _exit(0) }
                }
                std::thread::sleep(Duration::from_millis(100));
                let ok = std::fs::write(&args[2], "detached").is_ok();
                unsafe { _exit(if ok { 0 } else { 80 }) }
            }
            unsafe {
                waitpid(child, std::ptr::null_mut(), 0);
            }
        }
        _ => {
            eprintln!("unknown probe mode: {mode}");
            std::process::exit(64);
        }
    }
}
