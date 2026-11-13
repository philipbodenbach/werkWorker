//! Authenticate the one retained userspace Mach endpoint before native exec.
//!
//! A live TASK_ACCESS_PORT is immutable, but it is not a kernel object and its
//! getter is not reliably denied by Seatbelt. Compare it with launchd's system
//! taskgated registration instead of trusting a live send right or a port name.
//!
//! Use the bounded legacy lookup wire protocol also used by libxpc's
//! launch_legacy_mig_look_up3. Unlike bootstrap_look_up2, this path needs no XPC
//! objects, allocation or locks after fork. Unknown protocol versions fail shut.
//! Authenticate the kernel audit trailer AND the response's recipient identity:
//! a forwarded reply from launchd must not authenticate a different recipient.

type Check = std::result::Result<(), (u8, i32)>;
const ACCESS: i32 = 9;
const BOOTSTRAP: i32 = 4;
const AUDIT_TOKEN: u32 = 15;
const LOOKUP_REQUEST: u32 = 439;
const LOOKUP_REPLY: u32 = LOOKUP_REQUEST + 100;
const REQUEST_SIZE: u32 = 188;
const REPLY_SIZE: u32 = 56;
const TIMEOUT_MS: u32 = 1000;
const MESSAGE_WORDS: usize = 128;
const COMPLEX: u32 = 0x8000_0000;
const RECEIVE_RIGHT: u32 = 1;

unsafe extern "C" {
    static mach_task_self_: u32;
    fn task_get_special_port(task: u32, which: i32, port: *mut u32) -> i32;
    fn task_info(task: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
    fn mach_port_allocate(task: u32, right: u32, port: *mut u32) -> i32;
    fn mach_port_mod_refs(task: u32, port: u32, right: u32, delta: i32) -> i32;
    fn mach_port_deallocate(task: u32, port: u32) -> i32;
    fn mach_msg(
        message: *mut u32,
        options: u32,
        send_size: u32,
        receive_size: u32,
        receive_port: u32,
        timeout: u32,
        notify: u32,
    ) -> i32;
    fn mach_msg_destroy(message: *mut u32);
}

fn live_name(port: u32) -> bool {
    port != 0 && port != u32::MAX
}

fn status(result: i32, stage: u8) -> Check {
    if result == 0 {
        Ok(())
    } else {
        Err((stage, result))
    }
}

// Fixed-layout launchd lookup reply: header, one send-right descriptor, NDR,
// recipient PID/version, then a kernel-supplied MACH_MSG_TRAILER_FORMAT_0 audit
// trailer. Never inspect a caller-supplied audit token in the message body.
fn authenticate_reply(
    reply: &[u32; MESSAGE_WORDS],
    receive: u32,
    candidate: u32,
    recipient: &[u32; 8],
) -> Check {
    if reply[0] != (COMPLEX | (18 << 8))
        || reply[1] != REPLY_SIZE
        || reply[2] != 0
        || reply[3] != receive
        || reply[4] != 0
        || reply[5] != LOOKUP_REPLY
        || reply[6] != 1
        || reply[9] != 17 << 16 // received MACH_MSG_TYPE_MOVE_SEND, PORT_DESCRIPTOR
        || reply[10..12] != [0, 1] // native NDR (little-endian ARM64)
        || !live_name(reply[7])
    {
        return Err((29, -1));
    }
    // The trailer starts at the validated, four-byte-aligned body size (56).
    if reply[14] != 0 || reply[15] < 52 || reply[15] > 512 - REPLY_SIZE {
        return Err((29, -2));
    }
    if reply[20] != 0 || reply[24] != 1 {
        // audit_token.val[1] = effective UID, val[5] = PID. Only root launchd.
        return Err((29, -3));
    }
    if reply[12] != recipient[5] || reply[13] != recipient[7] {
        return Err((29, -4));
    }
    if reply[7] != candidate {
        return Err((30, -1));
    }
    Ok(())
}

unsafe fn authenticate_candidate(task: u32, candidate: u32, bootstrap: u32) -> Check {
    if !live_name(candidate) || !live_name(bootstrap) || candidate == bootstrap {
        // Even the fixed lookup request must not be sent to the candidate.
        return Err((24, -1));
    }
    let mut identity = [0_u32; 8];
    let mut count = 8;
    status(
        unsafe { task_info(task, AUDIT_TOKEN, identity.as_mut_ptr().cast(), &mut count) },
        27,
    )?;
    if count != 8 || identity[5] != unsafe { libc::getpid() } as u32 || identity[7] == 0 {
        return Err((27, -1));
    }
    let mut receive = 0;
    status(
        unsafe { mach_port_allocate(task, RECEIVE_RIGHT, &mut receive) },
        26,
    )?;
    let mut message = [0_u32; MESSAGE_WORDS];
    message[0] = 19 | (21 << 8); // COPY_SEND destination, MAKE_SEND_ONCE reply
    message[1] = REQUEST_SIZE;
    message[2] = bootstrap;
    message[3] = receive;
    message[5] = LOOKUP_REQUEST;
    // NDR integer representation: little endian.
    message[7] = 1;
    // name[128] starts at byte 32; target_pid and instance UUID remain zero.
    let service = b"com.apple.taskgated\0";
    unsafe {
        std::ptr::copy_nonoverlapping(
            service.as_ptr(),
            message.as_mut_ptr().cast::<u8>().add(32),
            service.len(),
        );
    }
    // STRICT_LOOKUP | PRIVILEGED_SERVER: bypass user-domain shadows and resolve
    // only the system registration (uint64_t at byte 180).
    message[45] = 0x88;
    let result = unsafe {
        mach_msg(
            message.as_mut_ptr(),
            // Bound both phases and return interruptions instead of restarting
            // with a fresh timeout. Ask the kernel for the sender's audit token.
            1 | 2 | 0x10 | 0x100 | 0x40 | 0x400 | (3 << 24),
            REQUEST_SIZE,
            std::mem::size_of_val(&message) as u32,
            receive,
            TIMEOUT_MS,
            0,
        )
    };
    let checked = if result == 0 {
        let checked = authenticate_reply(&message, receive, candidate, &identity);
        // Even an unauthenticated, malformed reply may transfer rights or OOL
        // storage. Destroy all received resources, not just the expected port.
        unsafe { mach_msg_destroy(message.as_mut_ptr()) };
        checked
    } else {
        let mut failed = Err((28, result));
        let error = result & !0x3e00; // MACH_MSG_MASK: copyout resource-error bits.
        if (0x1000_0000..0x1000_4000).contains(&error) {
            // XNU ipc_kmsg_copyout_pseudo returns rights from an unsent message
            // in place: COPY_SEND becomes MOVE_SEND, MAKE_SEND_ONCE becomes a
            // separate MOVE_SEND_ONCE name. mach_msg_destroy skips local_port,
            // so consume it explicitly, following Apple's
            // libsyscall/mach/mach_msg.c:mach_msg_server_consume_unsent_message.
            // An early failure leaves the original dispositions intact.
            if matches!((message[0] >> 8) & 0xff, 17 | 18)
                && live_name(message[3])
                && let Err(error) = status(unsafe { mach_port_deallocate(task, message[3]) }, 31)
            {
                failed = Err(error);
            }
            unsafe { mach_msg_destroy(message.as_mut_ptr()) };
        }
        // A body copyout error can still deliver some rights/OOL allocations.
        // Other receive failures are cleaned by the kernel or leave no message.
        if error == 0x1000_400c {
            unsafe { mach_msg_destroy(message.as_mut_ptr()) };
        }
        failed
    };
    // Destroy our private receive right, including queued replies after timeout.
    status(
        unsafe { mach_port_mod_refs(task, receive, RECEIVE_RIGHT, -1) },
        31,
    )?;
    checked
}

/// Must run before the final child clears bootstrap/registered/exception slots.
/// All calls and storage below are allocation-free and usable after fork.
pub(super) unsafe fn verify_system_task_access() -> Check {
    let task = unsafe { mach_task_self_ };
    let mut access = 0;
    status(
        unsafe { task_get_special_port(task, ACCESS, &mut access) },
        24,
    )?;
    let mut bootstrap = 0;
    let queried = unsafe { task_get_special_port(task, BOOTSTRAP, &mut bootstrap) };
    let checked = if queried == 0 {
        unsafe { authenticate_candidate(task, access, bootstrap) }
    } else {
        Err((25, queried))
    };
    let mut released = Ok(());
    for port in [access, bootstrap] {
        if live_name(port) {
            let result = unsafe { mach_port_deallocate(task, port) };
            if result != 0 {
                released = Err((31, result));
            }
        }
    }
    released?;
    checked
}

/// Resolve lazy Mach stubs in the multithreaded parent, before the first fork.
pub(super) fn resolve_symbols() {
    let mut port = 0;
    let mut identity = [0_i32; 8];
    let mut count = 8;
    let mut empty = [0_u32; 6];
    unsafe {
        task_get_special_port(0, ACCESS, &mut port);
        task_info(0, AUDIT_TOKEN, identity.as_mut_ptr(), &mut count);
        mach_port_allocate(0, RECEIVE_RIGHT, &mut port);
        mach_port_mod_refs(0, 0, RECEIVE_RIGHT, 0);
        mach_port_deallocate(0, 0);
        mach_msg(empty.as_mut_ptr(), 0, 0, 0, 0, 0, 0);
        mach_msg_destroy(empty.as_mut_ptr());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" {
        fn mach_port_insert_right(task: u32, name: u32, right: u32, disposition: u32) -> i32;
        fn mach_port_set_attributes(
            task: u32,
            port: u32,
            flavor: u32,
            info: *const u32,
            count: u32,
        ) -> i32;
        fn mach_port_get_refs(task: u32, port: u32, right: u32, refs: *mut u32) -> i32;
        fn mach_port_names(
            task: u32,
            names: *mut *mut u32,
            name_count: *mut u32,
            types: *mut *mut u32,
            type_count: *mut u32,
        ) -> i32;
        fn vm_deallocate(task: u32, address: usize, size: usize) -> i32;
    }

    fn endpoint_with_send_right(task: u32) -> u32 {
        let mut endpoint = 0;
        assert_eq!(
            unsafe { mach_port_allocate(task, RECEIVE_RIGHT, &mut endpoint) },
            0
        );
        assert_eq!(
            unsafe { mach_port_insert_right(task, endpoint, endpoint, 20) },
            0
        );
        endpoint
    }

    fn destroy_endpoint(task: u32, endpoint: u32) {
        assert_eq!(unsafe { mach_port_deallocate(task, endpoint) }, 0);
        assert_eq!(
            unsafe { mach_port_mod_refs(task, endpoint, RECEIVE_RIGHT, -1) },
            0
        );
    }

    fn copied_special_port(task: u32, selector: i32) -> u32 {
        let mut port = 0;
        assert_eq!(
            unsafe { task_get_special_port(task, selector, &mut port) },
            0
        );
        assert!(live_name(port));
        port
    }

    #[test]
    fn live_system_task_access_matches_authenticated_launchd_registration() {
        resolve_symbols();
        assert_eq!(unsafe { verify_system_task_access() }, Ok(()));
    }

    #[test]
    fn arbitrary_live_endpoint_is_not_the_system_task_access_service() {
        let task = unsafe { mach_task_self_ };
        let endpoint = endpoint_with_send_right(task);
        let bootstrap = copied_special_port(task, BOOTSTRAP);
        let checked = unsafe { authenticate_candidate(task, endpoint, bootstrap) };
        assert_eq!(unsafe { mach_port_deallocate(task, bootstrap) }, 0);
        destroy_endpoint(task, endpoint);
        assert_eq!(checked, Err((30, -1)));
    }

    #[test]
    fn null_dead_and_kernel_task_ports_cannot_authenticate_as_taskgated() {
        let task = unsafe { mach_task_self_ };
        let bootstrap = copied_special_port(task, BOOTSTRAP);
        for candidate in [0, u32::MAX] {
            assert_eq!(
                unsafe { authenticate_candidate(task, candidate, bootstrap) },
                Err((24, -1))
            );
        }
        let checked = unsafe { authenticate_candidate(task, task, bootstrap) };
        assert_eq!(unsafe { mach_port_deallocate(task, bootstrap) }, 0);
        assert_eq!(checked, Err((30, -1)));
    }

    #[test]
    fn candidate_aliasing_bootstrap_is_rejected_without_sending_a_request() {
        let task = unsafe { mach_task_self_ };
        let candidate = endpoint_with_send_right(task);
        let checked = unsafe { authenticate_candidate(task, candidate, candidate) };
        let mut queued = [0_u32; MESSAGE_WORDS];
        let received = unsafe {
            mach_msg(
                queued.as_mut_ptr(),
                2 | 0x100,
                0,
                std::mem::size_of_val(&queued) as u32,
                candidate,
                0,
                0,
            )
        };
        if received == 0 {
            unsafe { mach_msg_destroy(queued.as_mut_ptr()) };
        }
        destroy_endpoint(task, candidate);
        assert_eq!(checked, Err((24, -1)));
        assert_eq!(received, 0x1000_4003, "candidate must receive no lookup");
    }

    #[test]
    fn full_bootstrap_queue_timeouts_release_all_returned_rights() {
        const CHILD: &str = "WERK_WORKER_TASK_ACCESS_FULL_QUEUE_TEST";
        if std::env::var_os(CHILD).is_none() {
            // Isolate namespace counts from other concurrently running tests.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "runtime::macos::task_access::tests::full_bootstrap_queue_timeouts_release_all_returned_rights", "--nocapture"])
                .env(CHILD, "1")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        fn namespace_counts(task: u32) -> (u32, usize) {
            let mut names = std::ptr::null_mut();
            let mut types = std::ptr::null_mut();
            let mut name_count = 0;
            let mut type_count = 0;
            assert_eq!(
                unsafe {
                    mach_port_names(
                        task,
                        &mut names,
                        &mut name_count,
                        &mut types,
                        &mut type_count,
                    )
                },
                0
            );
            let dead = unsafe { std::slice::from_raw_parts(types, type_count as usize) }
                .iter()
                .filter(|kind| **kind & (1 << 20) != 0)
                .count();
            assert_eq!(
                unsafe { vm_deallocate(task, names as usize, name_count as usize * 4) },
                0
            );
            assert_eq!(
                unsafe { vm_deallocate(task, types as usize, type_count as usize * 4) },
                0
            );
            (name_count, dead)
        }

        fn send_refs(task: u32, endpoint: u32) -> u32 {
            let mut refs = 0;
            assert_eq!(
                unsafe { mach_port_get_refs(task, endpoint, 0, &mut refs) },
                0
            );
            refs
        }

        resolve_symbols();
        let task = unsafe { mach_task_self_ };
        let endpoint = endpoint_with_send_right(task);
        let limit = 1;
        assert_eq!(
            unsafe { mach_port_set_attributes(task, endpoint, 1, &limit, 1) },
            0
        );
        // Fill exactly one slot: filling until timeout would itself return rights.
        let mut queued = [19, 24, endpoint, 0, 0, 1];
        assert_eq!(
            unsafe { mach_msg(queued.as_mut_ptr(), 1 | 0x10, 24, 0, 0, 0, 0) },
            0
        );
        let before = namespace_counts(task);
        let refs_before = send_refs(task, endpoint);
        let started = std::time::Instant::now();
        for _ in 0..3 {
            assert_eq!(
                unsafe { authenticate_candidate(task, task, endpoint) },
                Err((28, 0x1000_0004))
            );
            assert_eq!(
                send_refs(task, endpoint),
                refs_before,
                "destination SEND urefs leaked"
            );
            assert_eq!(
                namespace_counts(task),
                before,
                "reply SEND_ONCE/dead names leaked"
            );
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        destroy_endpoint(task, endpoint);
    }

    #[test]
    fn forged_bootstrap_reply_with_real_service_fails_kernel_sender_authentication() {
        let task = unsafe { mach_task_self_ };
        let access = copied_special_port(task, ACCESS);
        let fake_bootstrap = endpoint_with_send_right(task);
        let server = std::thread::spawn(move || {
            let mut request = [0_u32; MESSAGE_WORDS];
            let received = unsafe {
                mach_msg(
                    request.as_mut_ptr(),
                    2 | 0x100 | (3 << 24),
                    0,
                    std::mem::size_of_val(&request) as u32,
                    fake_bootstrap,
                    2000,
                    0,
                )
            };
            assert_eq!(received, 0);
            assert_eq!(request[1], REQUEST_SIZE);
            assert_eq!(request[5], LOOKUP_REQUEST);
            let mut reply = [0_u32; MESSAGE_WORDS];
            reply[0] = COMPLEX | 18; // Consume the request's SEND_ONCE reply right.
            reply[1] = REPLY_SIZE;
            reply[2] = request[2];
            reply[5] = LOOKUP_REPLY;
            reply[6] = 1;
            reply[7] = access;
            reply[9] = 19 << 16; // COPY_SEND: even the genuine endpoint cannot help.
            reply[11] = 1;
            // The request's kernel audit trailer follows its 188-byte body.
            reply[12] = request[57];
            reply[13] = request[59];
            let sent = unsafe { mach_msg(reply.as_mut_ptr(), 1 | 0x10, REPLY_SIZE, 0, 0, 1000, 0) };
            if sent != 0 {
                unsafe { mach_msg_destroy(reply.as_mut_ptr()) };
            }
            sent
        });
        let checked = unsafe { authenticate_candidate(task, access, fake_bootstrap) };
        let sent = server.join().unwrap();
        destroy_endpoint(task, fake_bootstrap);
        assert_eq!(unsafe { mach_port_deallocate(task, access) }, 0);
        assert_eq!(sent, 0);
        assert_eq!(checked, Err((29, -3)));
    }

    #[test]
    fn unresponsive_bootstrap_lookup_is_bounded_and_fails_closed() {
        let task = unsafe { mach_task_self_ };
        let access = copied_special_port(task, ACCESS);
        let fake_bootstrap = endpoint_with_send_right(task);
        let started = std::time::Instant::now();
        let checked = unsafe { authenticate_candidate(task, access, fake_bootstrap) };
        let elapsed = started.elapsed();
        destroy_endpoint(task, fake_bootstrap);
        assert_eq!(unsafe { mach_port_deallocate(task, access) }, 0);
        assert_eq!(checked, Err((28, 0x1000_4003)));
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "elapsed={elapsed:?}"
        );
    }

    #[test]
    fn inherited_system_task_access_authenticates_after_fork() {
        resolve_symbols();
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            let result = unsafe { verify_system_task_access() };
            unsafe { libc::_exit(if result.is_ok() { 0 } else { 1 }) };
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
    }

    #[test]
    fn reply_authentication_rejects_forgery_relay_and_other_endpoints() {
        let mut reply = [0_u32; MESSAGE_WORDS];
        reply[0] = COMPLEX | (18 << 8);
        reply[1] = REPLY_SIZE;
        reply[3] = 123;
        reply[5] = LOOKUP_REPLY;
        reply[6] = 1;
        reply[7] = 456;
        reply[9] = 17 << 16;
        reply[11] = 1;
        reply[12] = 789;
        reply[13] = 10;
        reply[15] = 52;
        reply[24] = 1;
        let identity = [0, 501, 20, 501, 20, 789, 0, 10];
        assert_eq!(authenticate_reply(&reply, 123, 456, &identity), Ok(()));
        for (offset, value) in [
            (0, 0),
            (1, 88),
            (2, 1),
            (3, 321),
            (4, 1),
            (5, 538),
            (6, 2),
            (7, 0),
            (9, 18 << 16),
            (11, 0),
            (14, 1),
            (15, 20),
            (20, 501),
            (24, 42),
            (12, 788),
            (13, 9),
            (7, 455),
        ] {
            let mut forged = reply;
            forged[offset] = value;
            assert!(
                authenticate_reply(&forged, 123, 456, &identity).is_err(),
                "offset={offset}"
            );
        }
    }
}
