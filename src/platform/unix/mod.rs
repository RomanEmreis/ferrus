use std::process::Command as StdCommand;

use super::ShutdownSignal;

#[cfg(target_os = "linux")]
mod linux;

pub(crate) fn set_serve_process_name() {
    #[cfg(target_os = "linux")]
    linux::set_serve_process_name();
}

pub(crate) fn install_serve_parent_lifecycle_hooks() {
    #[cfg(target_os = "linux")]
    linux::install_serve_parent_lifecycle_hooks();
}

pub(crate) fn configure_headless_command(command: &mut StdCommand) {
    use std::os::unix::process::CommandExt;

    // SAFETY: these libc calls are async-signal-safe and operate only on the
    // child process between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }

            #[cfg(target_os = "linux")]
            linux::install_headless_child_lifecycle_hook()?;

            Ok(())
        });
    }
}

pub(crate) fn signal_process(pid: u32, signal: ShutdownSignal) {
    unsafe {
        let signal = match signal {
            ShutdownSignal::Terminate => libc::SIGTERM,
            ShutdownSignal::Kill => libc::SIGKILL,
        };
        libc::kill(pid as libc::pid_t, signal);
    }
}

pub(crate) fn signal_process_group(pid: u32, signal: ShutdownSignal) {
    unsafe {
        let signal = match signal {
            ShutdownSignal::Terminate => libc::SIGTERM,
            ShutdownSignal::Kill => libc::SIGKILL,
        };
        libc::kill(-(pid as libc::pid_t), signal);
    }
}

pub(crate) fn pid_is_alive(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as i32, 0) };
    if ret == 0 {
        return true;
    }

    let err = std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or_default();
    err == libc::EPERM
}

pub(crate) fn shell_command(cmd: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg(cmd);
    command
}

pub(crate) fn flush_stdin_input_buffer() {
    // SAFETY: tcflush discards bytes queued on stdin. Errors are ignored because
    // some environments do not expose a flushable TTY.
    unsafe {
        let _ = libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}
