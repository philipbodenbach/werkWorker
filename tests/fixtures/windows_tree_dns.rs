//! Synthetic DnsQueryEx witness: no host DNS configuration and no public query.
//! Explicit per-query servers and synchronous lifetime follow:
//! https://learn.microsoft.com/en-us/windows/win32/api/windns/nf-windns-dnsqueryex
//! https://learn.microsoft.com/en-us/windows/win32/api/windns/ns-windns-dns_query_request
use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::ERROR_SUCCESS, NetworkManagement::Dns::*, Networking::WinSock::AF_INET,
};

pub struct Server {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    pub fn new() -> Self {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 53)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let address = socket.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let seen_thread = seen.clone();
        let stop_thread = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut bytes = [0u8; 4096];
            while !stop_thread.load(Ordering::Acquire) {
                let (length, peer) = match socket.recv_from(&mut bytes) {
                    Ok(value) => value,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => panic!("Synthetic DNS receive failed: {error}"),
                };
                let packet = &bytes[..length];
                let Some((name, question_end)) = question(packet) else {
                    continue;
                };
                // Only this test's nonce names are answered. A bind to an
                // ephemeral loopback port cannot become a public DNS service.
                if !name.ends_with(".ww-tree.test") {
                    continue;
                }
                seen_thread.lock().unwrap().push(name);
                let mut response = Vec::from(&packet[..question_end]);
                response[2] = 0x85; // response, authoritative, recursion requested
                response[3] = 0x80; // recursion available; successful synthetic A
                response[4..12].copy_from_slice(&[0, 1, 0, 1, 0, 0, 0, 0]);
                response.extend_from_slice(&[
                    0xc0, 0x0c, // owner points to the original question
                    0, 1, 0, 1, // A, IN
                    0, 0, 0, 0, // TTL zero: no reusable cache entry
                    0, 4, 192, 0, 2, 123,
                ]);
                socket.send_to(&response, peer).unwrap();
            }
        });
        Self {
            address,
            seen,
            stop,
            thread: Some(thread),
        }
    }
    pub fn address(&self) -> SocketAddr {
        self.address
    }
    pub fn arrivals(&self, name: &str) -> usize {
        let expected = name.trim_end_matches('.').to_ascii_lowercase();
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| *seen == &expected)
            .count()
    }
    pub fn positive_control(&self, name: &str) {
        let (status, has_record) = query(self.address, name);
        assert_eq!(
            status, ERROR_SUCCESS as i32,
            "DnsQueryEx host control did not use the explicit synthetic DNS server at {}",
            self.address
        );
        assert!(has_record, "Host DNS control returned no A record");
        println!(
            "SYNTHETIC_DNS_HOST_CONTROL server={} name={} arrivals={}",
            self.address,
            name,
            self.arrivals(name)
        );
        assert!(
            self.arrivals(name) > 0,
            "Successful status without the nonce reaching the controlled DNS server is not a positive control"
        );
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}

fn question(packet: &[u8]) -> Option<(String, usize)> {
    if packet.len() < 12 || packet[2] & 0x80 != 0 || packet[4..6] != [0, 1] {
        return None;
    }
    let mut offset = 12;
    let mut labels = Vec::new();
    loop {
        let count = *packet.get(offset)? as usize;
        offset += 1;
        if count == 0 {
            break;
        }
        if count > 63 {
            return None;
        }
        let end = offset.checked_add(count)?;
        labels.push(
            std::str::from_utf8(packet.get(offset..end)?)
                .ok()?
                .to_ascii_lowercase(),
        );
        offset = end;
    }
    if packet.get(offset..offset + 4)? != [0, 1, 0, 1] {
        return None;
    }
    Some((labels.join("."), offset + 4))
}

pub fn query(server: SocketAddr, name: &str) -> (i32, bool) {
    let SocketAddr::V4(server) = server else {
        panic!("Synthetic DNS server must be IPv4 loopback");
    };
    assert_eq!(*server.ip(), Ipv4Addr::LOCALHOST);
    assert!(name.ends_with(".ww-tree.test."));
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut servers = DNS_ADDR_ARRAY {
        MaxCount: 1,
        AddrCount: 1,
        ..Default::default()
    };
    let mut sockaddr = [0u8; 32];
    sockaddr[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    // DnsQueryEx v1 rejects a nonzero sockaddr port with ERROR_INVALID_PARAMETER.
    // The explicit loopback address uses standard DNS port 53; no OS DNS config changes.
    assert_eq!(server.port(), 53);

    sockaddr[4..8].copy_from_slice(&server.ip().octets());
    servers.AddrArray[0].MaxSa = sockaddr.map(|byte| byte as i8);
    let request = DNS_QUERY_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1,
        QueryName: name.as_ptr(),
        QueryType: DNS_TYPE_A,
        QueryOptions: (DNS_QUERY_BYPASS_CACHE
            | DNS_QUERY_WIRE_ONLY
            | DNS_QUERY_NO_HOSTS_FILE
            | DNS_QUERY_NO_LOCAL_NAME
            | DNS_QUERY_NO_NETBT
            | DNS_QUERY_NO_MULTICAST
            | DNS_QUERY_TREAT_AS_FQDN
            | DNS_QUERY_ACCEPT_TRUNCATED_RESPONSE) as u64,
        pDnsServerList: &mut servers,
        // No completion callback: request/result storage stays alive until this
        // synchronous API returns. It never consults an OS-configured server.
        ..Default::default()
    };
    let mut result = DNS_QUERY_RESULT {
        Version: DNS_QUERY_RESULTS_VERSION1,
        ..Default::default()
    };
    let mut cancel = DNS_QUERY_CANCEL::default();
    let status = unsafe { DnsQueryEx(&request, &mut result, &mut cancel) };
    let has_record = !result.pQueryRecords.is_null();
    if has_record {
        unsafe {
            DnsFree(result.pQueryRecords.cast(), DnsFreeRecordList);
        }
    }
    (status, has_record)
}
