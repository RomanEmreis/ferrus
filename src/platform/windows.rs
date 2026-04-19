use std::process::Command as StdCommand;

use super::ShutdownSignal;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    STILL_ACTIVE, TerminateProcess,
};

pub(crate) fn set_serve_process_name() {}

pub(crate) fn install_serve_parent_lifecycle_hooks() {}

pub(crate) fn configure_headless_command(_command: &mut StdCommand) {}

pub(crate) fn signal_process(pid: u32, _signal: ShutdownSignal) {
    with_process_handle(pid, PROCESS_TERMINATE, |handle| unsafe {
        let _ = TerminateProcess(handle, 1);
    });
}

pub(crate) fn signal_process_group(pid: u32, signal: ShutdownSignal) {
    // Phase 1: Windows has no direct equivalent to Unix process groups here yet.
    // Fall back to terminating the recorded root process only.
    signal_process(pid, signal);
}

pub(crate) fn pid_is_alive(pid: u32) -> bool {
    with_process_handle(pid, PROCESS_QUERY_LIMITED_INFORMATION, |handle| unsafe {
        let mut exit_code = 0;
        if GetExitCodeProcess(handle, &mut exit_code) == 0 {
            return false;
        }

        exit_code == STILL_ACTIVE
    })
    .unwrap_or(false)
}

pub(crate) fn shell_command(cmd: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("cmd");
    command.arg("/C").arg(cmd);
    command
}

pub(crate) fn flush_stdin_input_buffer() {}

fn with_process_handle<T>(pid: u32, access: u32, f: impl FnOnce(HANDLE) -> T) -> Option<T> {
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return None;
    }

    let result = f(handle);
    unsafe {
        let _ = CloseHandle(handle);
    }
    Some(result)
}
