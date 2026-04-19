use std::process::Command as StdCommand;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShutdownSignal {
    Terminate,
    Kill,
}

pub(crate) fn set_serve_process_name() {
    #[cfg(target_os = "linux")]
    unsafe {
        let name = b"ferrus-mcp\0";
        let _ = libc::prctl(libc::PR_SET_NAME, name.as_ptr() as libc::c_ulong, 0, 0, 0);
    }
}

pub(crate) fn install_serve_parent_lifecycle_hooks() {
    #[cfg(target_os = "linux")]
    {
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
}

pub(crate) fn configure_headless_command(command: &mut StdCommand) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // SAFETY: these libc calls are async-signal-safe and operate only on the
        // child process between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }

                #[cfg(target_os = "linux")]
                {
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
            });
        }
    }
}

pub(crate) fn signal_process(pid: u32, signal: ShutdownSignal) {
    #[cfg(unix)]
    unsafe {
        let signal = match signal {
            ShutdownSignal::Terminate => libc::SIGTERM,
            ShutdownSignal::Kill => libc::SIGKILL,
        };
        libc::kill(pid as libc::pid_t, signal);
    }

    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
    }
}

pub(crate) fn signal_process_group(pid: u32, signal: ShutdownSignal) {
    #[cfg(unix)]
    unsafe {
        let signal = match signal {
            ShutdownSignal::Terminate => libc::SIGTERM,
            ShutdownSignal::Kill => libc::SIGKILL,
        };
        libc::kill(-(pid as libc::pid_t), signal);
    }

    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
    }
}

pub(crate) fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let ret = unsafe { libc::kill(pid as i32, 0) };
        if ret == 0 {
            return true;
        }

        let err = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or_default();
        err == libc::EPERM
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}
