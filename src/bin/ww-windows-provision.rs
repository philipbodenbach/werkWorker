//! Explicit administrator helper; never elevates itself or starts a workload.
#[cfg(windows)]
fn execute() -> werk_worker::Result<()> {
    use werk_worker::runtime::windows_provision::{
        ProvisionRequest, provision, require_administrator,
    };
    require_administrator()?;
    let mut arguments = std::env::args_os().skip(1);
    let request = arguments
        .next()
        .filter(|v| v == "--request")
        .and_then(|_| arguments.next())
        .filter(|_| arguments.next().is_none())
        .ok_or_else(|| {
            werk_worker::WorkerError::new(
                "invalid_arguments",
                "Usage: ww-windows-provision --request REQUEST.json",
            )
        })?;
    let bytes = std::fs::read(request)?;
    let request: ProvisionRequest = serde_json::from_slice(&bytes)
        .map_err(|e| werk_worker::WorkerError::new("invalid_arguments", e.to_string()))?;
    let path = provision(request)?;
    println!("Provisioned: {}", path.display());
    println!("Native qualification remains required before enabling the process-tree profile.");
    Ok(())
}
fn main() {
    #[cfg(windows)]
    if let Err(error) = execute() {
        eprintln!("{error}");
        std::process::exit(2);
    }
    #[cfg(not(windows))]
    {
        eprintln!("Native Windows administrator setup is required; no changes made");
        std::process::exit(2);
    }
}
