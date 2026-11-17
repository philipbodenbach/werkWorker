//! Provisioned trusted bootstrap: host launcher or dedicated-account runner.
//! Credentials stay local to the host launcher; neither mode inherits a Job handle.
// CreateProcessWithLogonW enables a new console by default, even with the
// CREATE_NO_WINDOW flag on the native acceptance host. Both trusted modes use
// authenticated pipes exclusively; a console-subsystem image adds an unwanted
// live conhost.exe to the job before the payload starts.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    std::process::exit(werk_worker::runtime::windows_process_tree_runner());
    #[cfg(not(windows))]
    {
        eprintln!("The provisioned process-tree runner requires native Windows");
        std::process::exit(2);
    }
}
