use std::process::Command as StdCommand;

use super::ShutdownSignal;

pub(crate) fn set_serve_process_name() {}

pub(crate) fn install_serve_parent_lifecycle_hooks() {}

pub(crate) fn configure_headless_command(_command: &mut StdCommand) {}

pub(crate) fn signal_process(_pid: u32, _signal: ShutdownSignal) {}

pub(crate) fn signal_process_group(_pid: u32, _signal: ShutdownSignal) {}

pub(crate) fn pid_is_alive(_pid: u32) -> bool {
    false
}

pub(crate) fn shell_command(cmd: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("cmd");
    command.arg("/C").arg(cmd);
    command
}
