//! Provisioned trusted bootstrap: host launcher or dedicated-account runner.
//! Credentials stay local to the host launcher; neither mode inherits a Job handle.
// CreateProcessWithLogonW enables a new console by default, even with the
// CREATE_NO_WINDOW flag on the native acceptance host. Both trusted modes use
// authenticated pipes exclusively; a console-subsystem image adds an unwanted
// live conhost.exe to the job before the payload starts.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.first().is_some_and(|a| a == "--setup-package") {
            let code = match args.as_slice() {
                [_, user, name, diagnostic] => match (user.to_str(), name.to_str()) {
                    (Some(user), Some(name)) => {
                        werk_worker::runtime::windows_provision::setup_account_package(
                            user,
                            name,
                            std::path::Path::new(diagnostic),
                        )
                        .map_err(|error| eprintln!("{error}"))
                        .map_or(125, |_| 0)
                    }
                    _ => 82,
                },
                _ => 83,
            };
            std::process::exit(code);
        }
        std::process::exit(werk_worker::runtime::windows_process_tree_runner());
    }
    #[cfg(not(windows))]
    {
        eprintln!("The provisioned process-tree runner requires native Windows");
        std::process::exit(2);
    }
}
