use std::process::Command as StdCommand;

use super::ShutdownSignal;
use anyhow::{Context, Result};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    TerminateProcess, WaitForSingleObject,
};

const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;

pub(crate) fn set_serve_process_name() {}

pub(crate) fn install_serve_parent_lifecycle_hooks() {}

pub(crate) fn configure_headless_command(_command: &mut StdCommand) {}

pub(crate) struct HeadlessProcessGuard(HANDLE);

// SAFETY: the guard owns a Windows HANDLE and only closes it on drop.
// Ownership can move across threads without violating handle semantics.
unsafe impl Send for HeadlessProcessGuard {}

impl Drop for HeadlessProcessGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub(crate) fn attach_headless_process(pid: u32) -> Result<HeadlessProcessGuard> {
    let job = create_kill_on_close_job()?;

    let assigned = with_process_handle(
        pid,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA | PROCESS_TERMINATE,
        |handle| unsafe { AssignProcessToJobObject(job, handle) != 0 },
    )
    .unwrap_or(false);

    if assigned {
        Ok(HeadlessProcessGuard(job))
    } else {
        unsafe {
            let _ = CloseHandle(job);
        }
        anyhow::bail!("failed to assign process {pid} to Windows job object");
    }
}

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
    with_process_handle(
        pid,
        PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_ACCESS,
        |handle| unsafe {
            match WaitForSingleObject(handle, 0) {
                WAIT_TIMEOUT => true,
                WAIT_OBJECT_0 | WAIT_FAILED => false,
                _ => false,
            }
        },
    )
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

fn create_kill_on_close_job() -> Result<HANDLE> {
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        anyhow::bail!("failed to create Windows job object");
    }

    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) != 0
    };

    if ok {
        Ok(job)
    } else {
        unsafe {
            let _ = CloseHandle(job);
        }
        Err(anyhow::anyhow!("failed to configure Windows job object"))
            .context("job object setup for headless process")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn shell_command_uses_cmd_c() {
        assert_program_and_args(
            shell_command("echo ferrus").into_std(),
            "cmd",
            &["/C", "echo ferrus"],
        );
    }

    #[test]
    fn current_pid_is_alive() {
        assert!(pid_is_alive(std::process::id()));
    }

    #[test]
    fn obviously_invalid_pid_is_not_alive() {
        assert!(!pid_is_alive(u32::MAX));
    }

    fn assert_program_and_args(command: Command, program: &str, args: &[&str]) {
        assert_eq!(command.get_program().to_string_lossy(), program);
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            args.iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        );
    }
}
