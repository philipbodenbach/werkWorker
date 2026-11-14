//! Provisioned trusted bootstrap: host launcher or dedicated-account runner.
//! Credentials stay local to the host launcher; neither mode inherits a Job handle.
fn main() {
    #[cfg(windows)]
    std::process::exit(werk_worker::runtime::windows_process_tree_runner());
    #[cfg(not(windows))]
    {
        eprintln!("The provisioned process-tree runner requires native Windows");
        std::process::exit(2);
    }
}
