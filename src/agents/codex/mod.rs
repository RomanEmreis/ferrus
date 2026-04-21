//! Codex-backed supervisor and executor adapters.
//!
//! These wrappers isolate the CLI details needed to launch Codex in the shapes
//! Ferrus expects for interactive and headless sessions.

use super::{AgentRunMode, ExecutorAgent, SupervisorAgent, normalized_model};
#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use std::path::{Path, PathBuf};
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(crate) const NAME: &str = "codex";

/// Interactive and headless supervisor launcher for the Codex CLI.
#[derive(Debug, Clone)]
pub struct Supervisor {
    model: Option<String>,
}

/// Interactive and headless executor launcher for the Codex CLI.
#[derive(Debug, Clone)]
pub struct Executor {
    model: Option<String>,
}

impl Supervisor {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            model: normalized_model(model),
        }
    }
}

impl Executor {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            model: normalized_model(model),
        }
    }
}

impl SupervisorAgent for Supervisor {
    /// Returns the Ferrus-visible identifier for the Codex supervisor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Codex command used by Ferrus HQ or an interactive user.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Command {
        codex_command(mode, self.model())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the Codex executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Codex command used by Ferrus HQ or an interactive user.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Command {
        codex_command(mode, self.model())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

#[inline(always)]
fn codex_command(mode: AgentRunMode<'_>, model: Option<&str>) -> Command {
    let mut cmd = Command::new(codex_executable());
    match mode {
        AgentRunMode::Interactive { prompt } => {
            if let Some(model) = model {
                cmd.arg("--model").arg(model);
            }
            if let Some(prompt) = prompt {
                cmd.arg(prompt);
            }
        }
        AgentRunMode::Headless { prompt } => {
            // `codex exec` is the non-interactive entrypoint that runs a single prompt
            // and exits, which matches Ferrus executor and supervisor automation.
            cmd.arg("exec");
            if let Some(model) = model {
                cmd.arg("--model").arg(model);
            }
            cmd.arg(prompt);
        }
    }
    cmd
}

#[cfg(not(windows))]
fn codex_executable() -> &'static str {
    NAME
}

#[cfg(windows)]
fn codex_executable() -> OsString {
    resolve_codex_on_path().unwrap_or_else(|| OsString::from(NAME))
}

#[cfg(windows)]
fn resolve_codex_on_path() -> Option<OsString> {
    let path_var = std::env::var_os("PATH")?;
    let pathext_var =
        std::env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
    let pathext = pathext_var
        .to_string_lossy()
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .collect::<Vec<_>>();

    for dir in std::env::split_paths(&path_var) {
        let direct = dir.join(NAME);
        if direct.is_file() {
            return Some(direct.into_os_string());
        }
        for ext in &pathext {
            let candidate: PathBuf = dir.join(format!("{NAME}{ext}"));
            if Path::new(&candidate).is_file() {
                return Some(candidate.into_os_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(windows))]
    use crate::agents::tests::assert_program_and_args;

    #[test]
    fn codex_supervisor_builds_interactive_command() {
        let agent = Supervisor::new(None);
        let command = agent.spawn(AgentRunMode::Interactive {
            prompt: Some("plan"),
        });
        assert_codex_program(command.get_program());
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["plan".to_string()]
        );
    }

    #[test]
    fn codex_executor_builds_headless_command() {
        let agent = Executor::new(None);
        let command = agent.spawn(AgentRunMode::Headless { prompt: "run" });
        assert_codex_program(command.get_program());
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["exec".to_string(), "run".to_string()]
        );
    }

    #[test]
    fn codex_model_override_is_part_of_spawned_command() {
        let agent = Executor::new(Some("gpt-5.4"));
        let command = agent.spawn(AgentRunMode::Headless { prompt: "run" });
        assert_codex_program(command.get_program());
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec![
                "exec".to_string(),
                "--model".to_string(),
                "gpt-5.4".to_string(),
                "run".to_string(),
            ]
        );
    }

    #[test]
    fn codex_supervisor_config_entry_uses_expected_args() {
        let entry = Supervisor::new(Some("gpt-5.4"))
            .mcp_config_entry("supervisor", 1)
            .unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec![
                "serve",
                "--role",
                "supervisor",
                "--agent-name",
                "codex",
                "--agent-index",
                "1",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        assert_eq!(entry.model.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn codex_config_entry_uses_expected_args() {
        let entry = Executor::new(None).mcp_config_entry("executor", 3).unwrap();
        assert!(!entry.command.is_empty());
        assert_eq!(
            entry.args,
            vec![
                "serve",
                "--role",
                "executor",
                "--agent-name",
                "codex",
                "--agent-index",
                "3",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        assert_eq!(entry.model, None);
    }

    #[cfg(not(windows))]
    fn assert_codex_program(program: &std::ffi::OsStr) {
        assert_eq!(program.to_string_lossy(), "codex");
    }

    #[cfg(windows)]
    fn assert_codex_program(program: &std::ffi::OsStr) {
        let path = std::path::Path::new(program);
        let stem = path
            .file_stem()
            .expect("codex program should have filename")
            .to_string_lossy()
            .to_ascii_lowercase();
        assert_eq!(stem, "codex");
    }
}
