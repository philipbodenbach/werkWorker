//! Fresh-process thread-identity probes with independent synthetic listeners.
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, SetLastError},
    Networking::WinSock::{WSACleanup, WSADATA, WSAGetLastError, WSAStartup},
    Security::{ImpersonateAnonymousToken, ImpersonateSelf, RevertToSelf, SecurityAnonymous},
    System::Threading::GetCurrentThread,
};

pub struct Witness {
    tcp: [SocketAddr; 2],
    udp: [SocketAddr; 2],
    seen: Arc<Mutex<Vec<String>>>,
    accepted: Arc<[AtomicUsize; 2]>,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}
const KINDS: [&str; 4] = ["tcp-v4", "tcp-v6", "udp-v4", "udp-v6"];
fn message(nonce: &str, kind: &str) -> String {
    format!("{nonce}/{kind}")
}
impl Witness {
    pub fn new() -> Self {
        let tcp = [
            TcpListener::bind("127.0.0.1:0").unwrap(),
            TcpListener::bind("[::1]:0").unwrap(),
        ];
        let udp = [
            UdpSocket::bind("127.0.0.1:0").unwrap(),
            UdpSocket::bind("[::1]:0").unwrap(),
        ];
        let tcp_addresses = tcp.each_ref().map(|s| s.local_addr().unwrap());
        let udp_addresses = udp.each_ref().map(|s| s.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for (index, socket) in tcp.into_iter().enumerate() {
            let accepted = accepted.clone();
            socket.set_nonblocking(true).unwrap();
            let seen = seen.clone();
            let stop = stop.clone();
            threads.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match socket.accept() {
                        Ok((stream, _)) => {
                            accepted[index].fetch_add(1, Ordering::SeqCst);
                            // Winsock accept inherits the listener's nonblocking
                            // state. Read the accepted stream with a bounded wait
                            // so a not-yet-arrived nonce is not discarded.
                            stream.set_nonblocking(false).unwrap();
                            stream
                                .set_read_timeout(Some(Duration::from_millis(750)))
                                .unwrap();
                            let mut bytes = Vec::new();
                            let result = stream.take(256).read_to_end(&mut bytes);
                            if let Err(error) = &result {
                                println!("NATIVE_TREE_TCP_WITNESS_READ_ERROR {}", json!({
                                    "family_index":index,"received_bytes":bytes.len(),
                                    "error":error.raw_os_error(),"kind":format!("{:?}",error.kind()),
                                }));
                            }
                            if !bytes.is_empty() {
                                let text = String::from_utf8(bytes).unwrap();
                                if text.starts_with("ww-tree-net-") {
                                    seen.lock().unwrap().push(text);
                                }
                            }
                            // A hostile/failed writer may close or time out. Any bytes
                            // already received still constitute independent evidence.
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("Synthetic TCP witness failed: {error}"),
                    }
                }
            }));
        }
        for socket in udp {
            socket
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            let seen = seen.clone();
            let stop = stop.clone();
            threads.push(std::thread::spawn(move || {
                let mut bytes = [0u8; 256];
                while !stop.load(Ordering::Acquire) {
                    match socket.recv_from(&mut bytes) {
                        Ok((length, _)) => {
                            let text = String::from_utf8(bytes[..length].to_vec()).unwrap();
                            if text.starts_with("ww-tree-net-") {
                                seen.lock().unwrap().push(text);
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(error) => panic!("Synthetic UDP witness failed: {error}"),
                    }
                }
            }));
        }
        Self {
            tcp: tcp_addresses,
            udp: udp_addresses,
            seen,
            accepted,
            stop,
            threads,
        }
    }
    pub fn connections(&self) -> [usize; 2] {
        self.accepted
            .each_ref()
            .map(|count| count.load(Ordering::SeqCst))
    }
    pub fn configuration(&self) -> Value {
        json!({"tcp":self.tcp,"udp":self.udp})
    }
    pub fn arrivals(&self, nonce: &str) -> [usize; 4] {
        let seen = self.seen.lock().unwrap();
        KINDS.map(|kind| {
            seen.iter()
                .filter(|entry| **entry == message(nonce, kind))
                .count()
        })
    }
    pub fn positive_control(&self) {
        let nonce = format!("ww-tree-net-control-{}", uuid::Uuid::new_v4());
        let result = sockets(&self.configuration(), &nonce);
        for operation in result.as_array().unwrap() {
            assert_eq!(
                operation["sent"], true,
                "Host socket witness control failed: {result}"
            );
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.arrivals(&nonce).contains(&0) {
            assert!(
                Instant::now() < deadline,
                "Host socket control did not reach all witnesses: {:?}; accepted TCP: {:?}; API results: {result}",
                self.arrivals(&nonce),
                self.connections()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Witness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let joined = thread.join();
            if !std::thread::panicking() {
                joined.expect("Network witness thread failed");
            }
        }
    }
}
fn sockets(configuration: &Value, nonce: &str) -> Value {
    let mut results = Vec::new();
    for (index, address) in configuration["tcp"].as_array().unwrap().iter().enumerate() {
        let address: SocketAddr = serde_json::from_value(address.clone()).unwrap();
        let kind = KINDS[index];
        let result = match TcpStream::connect_timeout(&address, Duration::from_millis(750)) {
            Ok(mut stream) => {
                let sent = stream
                    .set_write_timeout(Some(Duration::from_millis(750)))
                    .and_then(|_| stream.write_all(message(nonce, kind).as_bytes()));
                json!({"kind":kind,"connected":true,"sent":sent.is_ok(),"error":sent.err().and_then(|error|error.raw_os_error())})
            }
            Err(error) => {
                json!({"kind":kind,"connected":false,"sent":false,"error":error.raw_os_error()})
            }
        };
        results.push(result);
    }
    for (index, address) in configuration["udp"].as_array().unwrap().iter().enumerate() {
        let address: SocketAddr = serde_json::from_value(address.clone()).unwrap();
        let kind = KINDS[index + 2];
        let bind = if address.is_ipv4() {
            "127.0.0.1:0"
        } else {
            "[::1]:0"
        };
        let result = match UdpSocket::bind(bind) {
            Ok(socket) => {
                let bytes = message(nonce, kind);
                let sent = socket.send_to(bytes.as_bytes(), address);
                json!({"kind":kind,"bound":true,"sent":sent.as_ref().is_ok_and(|n|*n==bytes.len()),"error":sent.err().and_then(|error|error.raw_os_error())})
            }
            Err(error) => {
                json!({"kind":kind,"bound":false,"sent":false,"error":error.raw_os_error()})
            }
        };
        results.push(result);
    }
    json!(results)
}
struct RestoreThread(bool);
impl RestoreThread {
    fn restore(&mut self) {
        if self.0 {
            if unsafe { RevertToSelf() } == 0 {
                // Never continue fixture I/O under an unexpected thread identity.
                std::process::exit(126);
            }
            self.0 = false;
        }
    }
}
impl Drop for RestoreThread {
    fn drop(&mut self) {
        self.restore();
    }
}

struct WinsockLibrary(bool);
impl WinsockLibrary {
    fn close(&mut self) -> Value {
        let status = if self.0 {
            self.0 = false;
            unsafe { WSACleanup() }
        } else {
            0
        };
        json!({"status":status,"error":if status == 0 { None } else { Some(unsafe { WSAGetLastError() }) }})
    }
}
impl Drop for WinsockLibrary {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
pub fn attempt(plan: &Value) -> Value {
    let variant = plan["impersonation"].as_str().unwrap();
    // Initialize only the library under the original identity: no socket, DNS
    // call or connection. This models a caller that initializes Winsock before
    // changing its thread identity; every tested socket is created afterward.
    let mut data: WSADATA = unsafe { std::mem::zeroed() };
    let startup = unsafe { WSAStartup(0x0202, &mut data) };
    let mut library = WinsockLibrary(startup == 0);
    let mut winsock = json!({"startup_status":startup,"requested_version":0x0202,
        "version":data.wVersion,"high_version":data.wHighVersion});
    if startup != 0 {
        return json!({"variant":variant,"winsock":winsock,"network_attempted":false});
    }
    unsafe { SetLastError(0) };
    let changed = match variant {
        "baseline" => 1,
        "self-anonymous" => unsafe { ImpersonateSelf(SecurityAnonymous) },
        "anonymous-token" => unsafe { ImpersonateAnonymousToken(GetCurrentThread()) },
        _ => panic!("Unknown thread identity variant"),
    };
    if changed == 0 {
        let error = unsafe { GetLastError() };
        winsock["cleanup"] = library.close();
        return json!({"variant":variant,"winsock":winsock,"applied":false,"impersonation_error":error,"network_attempted":false});
    }
    let mut restore = RestoreThread(variant != "baseline");
    // This is this fresh process's first DNS operation. No token reset occurs
    // between this call and creation/use of all four fresh network sockets.
    let (status, record) = super::tree_dns::query(
        serde_json::from_value(plan["dns_server"].clone()).unwrap(),
        plan["dns_name"].as_str().unwrap(),
    );
    let transport = sockets(&plan["network"], plan["network_nonce"].as_str().unwrap());
    restore.restore();
    // The thread identity is restored before Winsock cleanup, including unwind
    // paths (RestoreThread is constructed after WinsockLibrary).
    winsock["cleanup"] = library.close();
    json!({"variant":variant,"winsock":winsock,"applied":true,"network_attempted":true,
        "dns":{"status":status,"record":record},"transport":transport,
        "revert":if variant == "baseline" { "not-needed" } else { "succeeded" }})
}
