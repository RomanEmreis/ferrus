pub(super) fn set_serve_process_name() {
    unsafe {
        let name = b"ferrus-mcp\0";
        let _ = libc::prctl(libc::PR_SET_NAME, name.as_ptr() as libc::c_ulong, 0, 0, 0);
    }
}

pub(super) fn install_serve_parent_lifecycle_hooks() {
    unsafe {
        let _ = libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGTERM as libc::c_ulong,
            0,
            0,
            0,
        );
    }

    let parent_pid = unsafe { libc::getppid() };
    if parent_pid > 1 {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                let current_ppid = unsafe { libc::getppid() };
                if current_ppid <= 1 || current_ppid != parent_pid {
                    std::process::exit(0);
                }
            }
        });
    }
}

pub(super) fn install_headless_child_lifecycle_hook() -> std::io::Result<()> {
    unsafe {
        if libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGTERM as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(())
}
