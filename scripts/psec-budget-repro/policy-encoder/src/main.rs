//! Standalone diagnostic encoder. Never creates PSEC or launches a payload.
//! Uses the exact pinned SDK's unstable compatibility schema exports, not v1.
use mxc_sdk::process_security_environment_spec::process_security_environment_layout as psec;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

const USAGE: &str = "usage: werkworker-psec-policy-encoder --readonly PATH ... --readwrite PATH ... --deny PATH ... --out NEW_SPEC_FILE";

fn main() {
    if let Err(error) = run(std::env::args_os().skip(1).collect()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run(args: Vec<OsString>) -> Result<(), Box<dyn std::error::Error>> {
    let mut readonly = Vec::new();
    let mut writable = Vec::new();
    let mut denied = Vec::new();
    let mut output = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(USAGE)?;
        match flag.to_str() {
            Some("--out") if output.is_none() => output = Some(PathBuf::from(value)),
            Some("--readonly" | "--readwrite" | "--deny") => {
                let value = value
                    .into_string()
                    .map_err(|_| "policy path must be valid Unicode")?;
                // Diagnostic policy accepts only explicit absolute drive paths.
                let bytes = value.as_bytes();
                if bytes.len() < 3
                    || !bytes[0].is_ascii_alphabetic()
                    || bytes[1] != b':'
                    || bytes[2] != b'\\'
                    || value.contains('\0')
                    || value.split('\\').any(|part| part == "." || part == "..")
                {
                    return Err("policy path must be a drive-absolute Windows path without dot segments or NUL".into());
                }
                match flag.to_str() {
                    Some("--readonly") => readonly.push(value),
                    Some("--readwrite") => writable.push(value),
                    _ => denied.push(value),
                }
            }
            _ => return Err(USAGE.into()),
        }
    }
    let output = output.ok_or(USAGE)?;
    if readonly.is_empty() || writable.is_empty() || denied.is_empty() {
        return Err("all three policy lists must be supplied explicitly".into());
    }
    let bytes = encode(readonly, writable, denied);
    // Never replace an existing policy artifact.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    println!("wrote {} PSEC 1.1 bytes", bytes.len());
    Ok(())
}

fn encode(readonly: Vec<String>, writable: Vec<String>, denied: Vec<String>) -> Vec<u8> {
    let mut egress = psec::EndpointPolicyT::default();
    egress.default_action = psec::FilterAction::deny;
    egress.allow = None;
    egress.deny = None;
    let mut ingress = psec::IngressPolicyT::default();
    ingress.default_action = psec::FilterAction::deny;
    ingress.host_loopback = psec::FilterAction::deny;
    let mut network = psec::NetworkPolicyT::default();
    network.egress = Some(Box::new(egress));
    network.ingress = Some(Box::new(ingress));
    network.proxy = None;
    network.allowed_appcontainer_peer = None;
    let mut spec = psec::ProcessSecurityEnvironmentT::default();
    spec.version = psec::SchemaVersionT { major: 1, minor: 1 };
    spec.capabilities = None;
    spec.disallow_win32k_system_calls = true;
    spec.ui_restrictions = 0x3ff;
    spec.fs_read_only = Some(readonly);
    spec.fs_read_write = Some(writable);
    spec.fs_deny = Some(denied);
    spec.fs_enumerate = None;
    spec.network_policy = Some(Box::new(network));
    let mut builder = flatbuffers::FlatBufferBuilder::new();
    let root = spec.pack(&mut builder);
    psec::finish_process_security_environment_buffer(&mut builder, root);
    builder.finished_data().to_vec()
}

#[test]
fn strict_psec_policy_roundtrip_has_explicit_network_and_root_deny() {
    let bytes = encode(
        vec![r"C:\Windows\System32".into()],
        vec![r"C:\synthetic\work".into()],
        vec![r"C:\".into()],
    );
    assert!(psec::process_security_environment_buffer_has_identifier(
        &bytes
    ));
    let spec = psec::root_as_process_security_environment(&bytes).unwrap();
    assert_eq!((spec.version().major(), spec.version().minor()), (1, 1));
    assert_eq!(spec.capabilities(), None);
    assert!(spec.disallow_win32k_system_calls());
    assert_eq!(spec.ui_restrictions(), 0x3ff);
    assert_eq!(spec.fs_deny().unwrap().get(0), r"C:\");
    assert_eq!(spec.fs_read_only().unwrap().get(0), r"C:\Windows\System32");
    assert_eq!(spec.fs_read_write().unwrap().get(0), r"C:\synthetic\work");
    let network = spec.network_policy().unwrap();
    assert!(network.proxy().is_none());
    assert!(network.allowed_appcontainer_peer().is_none());
    assert_eq!(
        network.egress().unwrap().default_action(),
        psec::FilterAction::deny
    );
    assert_eq!(
        network.ingress().unwrap().default_action(),
        psec::FilterAction::deny
    );
    assert_eq!(
        network.ingress().unwrap().host_loopback(),
        psec::FilterAction::deny
    );
}
