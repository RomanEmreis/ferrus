//! Codex-backed supervisor and executor adapters.
//!
//! These wrappers isolate the CLI details needed to launch Codex in the shapes
//! Ferrus expects for interactive and headless sessions.

use super::{
    AgentRunMode, ExecutorAgent, HeadlessPromptTransport, SupervisorAgent, normalized_model,
};
use crate::agent_id::{ROLE_EXECUTOR, ROLE_SUPERVISOR};
use anyhow::{Result, anyhow};
#[cfg(windows)]
use std::path::PathBuf;
use std::process::Command;

/// Stable agent identifier used in Ferrus configuration and error messages.
pub(crate) const NAME: &str = "codex";
/// Actual CLI executable name used to launch Codex.
#[cfg(not(windows))]
const EXECUTABLE: &str = "codex";
#[cfg(windows)]
const WINDOWS_CMD_EXECUTABLE: &str = "codex.cmd";
#[cfg(windows)]
const WINDOWS_POWERSHELL_EXECUTABLE: &str = "codex.ps1";

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
    fn spawn(&self, mode: AgentRunMode<'_>) -> Result<Command> {
        codex_command(mode, self.model())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn headless_prompt_transport(&self) -> HeadlessPromptTransport {
        codex_headless_prompt_transport()
    }
}

impl ExecutorAgent for Executor {
    /// Returns the Ferrus-visible identifier for the Codex executor backend.
    fn name(&self) -> &'static str {
        NAME
    }

    /// Builds the Codex command used by Ferrus HQ or an interactive user.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Result<Command> {
        codex_command(mode, self.model())
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn headless_prompt_transport(&self) -> HeadlessPromptTransport {
        codex_headless_prompt_transport()
    }
}

#[inline(always)]
fn codex_command(mode: AgentRunMode<'_>, model: Option<&str>) -> Result<Command> {
    #[cfg(windows)]
    let mut cmd = windows_codex_command()?;
    #[cfg(not(windows))]
    let mut cmd = Command::new(EXECUTABLE);
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
            #[cfg(windows)]
            {
                let _ = prompt;
                cmd.arg("-");
            }
            #[cfg(not(windows))]
            cmd.arg(prompt);
        }
    }
    Ok(cmd)
}

fn codex_headless_prompt_transport() -> HeadlessPromptTransport {
    #[cfg(windows)]
    {
        HeadlessPromptTransport::Stdin
    }
    #[cfg(not(windows))]
    {
        HeadlessPromptTransport::Argv
    }
}

#[cfg(windows)]
fn resolve_windows_npm_shim_path() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .flat_map(|path| {
                [
                    path.join(WINDOWS_CMD_EXECUTABLE),
                    path.join(WINDOWS_POWERSHELL_EXECUTABLE),
                ]
            })
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(windows)]
fn windows_codex_invocation() -> Result<(PathBuf, PathBuf)> {
    let shim = resolve_windows_npm_shim_path().ok_or_else(|| {
        anyhow!(
            "Failed to locate codex.cmd or codex.ps1 in PATH; cannot resolve npm base directory \
             for direct Node launcher."
        )
    })?;
    let base_dir = shim.parent().ok_or_else(|| {
        anyhow!(
            "Failed to resolve parent directory for shim path: {}",
            shim.display()
        )
    })?;
    let codex_js = base_dir
        .join("node_modules")
        .join("@openai")
        .join("codex")
        .join("bin")
        .join("codex.js");
    if !codex_js.is_file() {
        return Err(anyhow!(
            "Failed to resolve direct Codex launcher script at {}",
            codex_js.display()
        ));
    }
    let local_node = base_dir.join("node.exe");
    let node = if local_node.is_file() {
        local_node
    } else {
        PathBuf::from("node.exe")
    };
    Ok((node, codex_js))
}

#[cfg(windows)]
fn windows_codex_command() -> Result<Command> {
    let (node, codex_js) = windows_codex_invocation()
        .map_err(|error| anyhow!("Failed to resolve Codex Windows launcher: {error}"))?;
    let mut cmd = Command::new(node);
    cmd.arg(codex_js);
    Ok(cmd)
}

pub(crate) fn apply_tool_approval_overrides(role: &str, entry: &mut toml::Table) {
    let tools = entry
        .entry("tools")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .expect("tools must be a TOML table");

    for tool in auto_approved_tools(role) {
        let mut tool_config = toml::Table::new();
        tool_config.insert(
            "approval_mode".to_string(),
            toml::Value::String("approve".to_string()),
        );
        tools.insert(tool.to_string(), toml::Value::Table(tool_config));
    }
}

fn auto_approved_tools(role: &str) -> &'static [&'static str] {
    match role {
        ROLE_EXECUTOR => &[
            "wait_for_task",
            "check",
            "consult",
            "submit",
            "wait_for_consult",
            "wait_for_answer",
            "ask_human",
            "answer",
            "status",
            "reset",
            "heartbeat",
        ],
        ROLE_SUPERVISOR => &[
            "create_task",
            "create_spec",
            "wait_for_review",
            "review_pending",
            "approve",
            "reject",
            "respond_consult",
            "ask_human",
            "answer",
            "status",
            "reset",
            "heartbeat",
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::tests::assert_program_and_args;
    #[cfg(windows)]
    use std::ffi::OsString;
    #[cfg(windows)]
    use std::sync::Mutex;
    #[cfg(windows)]
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(windows)]
    struct PathGuard {
        original: Option<OsString>,
    }

    #[cfg(windows)]
    impl PathGuard {
        fn set(path: &std::path::Path) -> Self {
            let original = std::env::var_os("PATH");
            std::env::set_var("PATH", path.as_os_str());
            Self { original }
        }
    }

    #[cfg(windows)]
    impl Drop for PathGuard {
        fn drop(&mut self) {
            if let Some(original) = self.original.take() {
                std::env::set_var("PATH", original);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[cfg(windows)]
    fn assert_windows_program_and_args(command: Result<Command>, tail_args: &[&str]) {
        let Ok(command) = command else {
            let error = command.unwrap_err().to_string();
            assert!(
                error.contains("Failed to resolve Codex Windows launcher"),
                "expected structured launcher resolution error, got: {error}"
            );
            return;
        };
        let program = command.get_program().to_string_lossy().into_owned();
        let actual = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if program.ends_with("node.exe") || program == "node.exe" {
            assert!(
                !actual.is_empty(),
                "expected codex.js arg + launcher args, got: {actual:?}"
            );
            assert!(
                actual[0].ends_with("node_modules\\@openai\\codex\\bin\\codex.js"),
                "expected codex.js path, got {}",
                actual[0]
            );
            let expected_tail = tail_args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            assert_eq!(actual[1..], expected_tail);
            return;
        }
        panic!("unexpected launcher program: {program} with args {actual:?}");
    }

    #[cfg(windows)]
    fn assert_windows_version_command_shape(command: Result<Command>, expected_node: &str) {
        let command = command.expect("version command should resolve");
        let program = command.get_program().to_string_lossy().into_owned();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            program.ends_with(expected_node),
            "expected launcher program ending with {expected_node}, got {program}"
        );
        assert_eq!(args.len(), 2, "expected codex.js + --version args");
        assert!(
            args[0].ends_with("node_modules\\@openai\\codex\\bin\\codex.js"),
            "expected first arg to be codex.js path, got {}",
            args[0]
        );
        assert_eq!(args[1], "--version");
    }

    #[test]
    fn codex_supervisor_builds_interactive_command() {
        let agent = Supervisor::new(None);
        #[cfg(not(windows))]
        let expected_program = EXECUTABLE;
        #[cfg(not(windows))]
        let expected_args: &[&str] = &["plan"];
        #[cfg(windows)]
        assert_windows_program_and_args(
            agent.spawn(AgentRunMode::Interactive {
                prompt: Some("plan"),
            }),
            &["plan"],
        );
        #[cfg(not(windows))]
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Interactive {
                    prompt: Some("plan"),
                })
                .unwrap(),
            expected_program,
            expected_args,
        );
    }

    #[test]
    fn codex_executor_builds_headless_command() {
        let agent = Executor::new(None);
        #[cfg(not(windows))]
        let expected_program = EXECUTABLE;
        #[cfg(not(windows))]
        let expected: &[&str] = &["exec", "run"];
        #[cfg(windows)]
        assert_windows_program_and_args(
            agent.spawn(AgentRunMode::Headless { prompt: "run" }),
            &["exec", "-"],
        );
        #[cfg(not(windows))]
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Headless { prompt: "run" })
                .unwrap(),
            expected_program,
            expected,
        );
    }

    #[test]
    fn codex_model_override_is_part_of_spawned_command() {
        let agent = Executor::new(Some("gpt-5.4"));
        #[cfg(not(windows))]
        let expected_program = EXECUTABLE;
        #[cfg(not(windows))]
        let expected: &[&str] = &["exec", "--model", "gpt-5.4", "run"];
        #[cfg(windows)]
        assert_windows_program_and_args(
            agent.spawn(AgentRunMode::Headless { prompt: "run" }),
            &["exec", "--model", "gpt-5.4", "-"],
        );
        #[cfg(not(windows))]
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Headless { prompt: "run" })
                .unwrap(),
            expected_program,
            expected,
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

    #[test]
    fn codex_approves_executor_tools_by_role() {
        let mut entry = toml::Table::new();
        apply_tool_approval_overrides("executor", &mut entry);
        let tools = entry.get("tools").and_then(toml::Value::as_table).unwrap();
        assert!(tools.contains_key("wait_for_task"));
        assert!(tools.contains_key("submit"));
        assert!(!tools.contains_key("approve"));
    }

    #[test]
    fn codex_approves_supervisor_tools_by_role() {
        let mut entry = toml::Table::new();
        apply_tool_approval_overrides("supervisor", &mut entry);
        let tools = entry.get("tools").and_then(toml::Value::as_table).unwrap();
        assert!(tools.contains_key("create_task"));
        assert!(tools.contains_key("create_spec"));
        assert!(!tools.contains_key("submit"));
    }

    #[test]
    fn codex_headless_prompt_preserves_newlines() {
        let agent = Executor::new(None);
        #[cfg(not(windows))]
        let expected_program = EXECUTABLE;
        #[cfg(not(windows))]
        let expected: &[&str] = &["exec", "line one\n\nline two"];
        #[cfg(windows)]
        assert_windows_program_and_args(
            agent.spawn(AgentRunMode::Headless {
                prompt: "line one\n\nline two",
            }),
            &["exec", "-"],
        );
        #[cfg(not(windows))]
        assert_program_and_args(
            agent
                .spawn(AgentRunMode::Headless {
                    prompt: "line one\n\nline two",
                })
                .unwrap(),
            expected_program,
            expected,
        );
    }

    #[test]
    fn codex_uses_expected_headless_prompt_transport() {
        #[cfg(windows)]
        let expected = HeadlessPromptTransport::Stdin;
        #[cfg(not(windows))]
        let expected = HeadlessPromptTransport::Argv;
        assert_eq!(Executor::new(None).headless_prompt_transport(), expected);
        assert_eq!(Supervisor::new(None).headless_prompt_transport(), expected);
    }

    #[test]
    fn codex_version_command_uses_expected_shape() {
        let agent = Supervisor::new(None);
        #[cfg(not(windows))]
        assert_program_and_args(agent.version_command().unwrap(), EXECUTABLE, &["--version"]);

        #[cfg(windows)]
        {
            let _lock = ENV_LOCK.lock().expect("env lock poisoned");
            let temp = tempfile::TempDir::new().expect("tempdir");
            let bin_dir = temp.path().join("npm");
            std::fs::create_dir_all(&bin_dir).expect("create shim dir");
            std::fs::write(bin_dir.join(WINDOWS_CMD_EXECUTABLE), "@echo off\n").expect("shim");
            let codex_js = bin_dir
                .join("node_modules")
                .join("@openai")
                .join("codex")
                .join("bin")
                .join("codex.js");
            std::fs::create_dir_all(codex_js.parent().expect("codex.js parent"))
                .expect("create codex.js parent");
            std::fs::write(&codex_js, "console.log('codex');").expect("codex.js");
            std::fs::write(bin_dir.join("node.exe"), "").expect("node.exe");

            let _guard = PathGuard::set(&bin_dir);
            assert_windows_version_command_shape(agent.version_command(), "node.exe");
        }
    }
}
