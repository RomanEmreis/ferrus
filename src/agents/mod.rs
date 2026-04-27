//! Agent adapters for the supported supervisor and executor backends.
//!
//! This module centralizes how Ferrus names agent implementations, spawns them
//! interactively or headlessly, and derives the MCP configuration used by HQ.

pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod qwen;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

/// Describes one MCP server entry for a spawned Ferrus agent.
///
/// Ferrus writes these values into client-facing configuration so external
/// tools can reconnect to the correct `ferrus serve` process for a given role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigEntry {
    /// Executable that should be launched for the MCP server.
    pub command: String,
    /// Arguments passed to [`Self::command`] when the MCP server starts.
    pub args: Vec<String>,
    /// Optional model override understood by the target client.
    pub model: Option<String>,
}

/// Describes how Ferrus intends to run an agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRunMode<'a> {
    Interactive { prompt: Option<&'a str> },
    Headless { prompt: &'a str },
}

/// Declares how a startup prompt should be transported to the child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTransport {
    /// Pass prompt as a regular CLI argument.
    Argv,
    /// Pass prompt via stdin and close stdin after writing.
    Stdin,
}

/// Behavior required from a supervisor-capable agent implementation.
///
/// Supervisor agents support both interactive sessions for humans and
/// headless sessions for HQ-managed workflows.
pub trait SupervisorAgent: Send + Sync {
    /// Returns the stable configuration name for this agent backend.
    fn name(&self) -> &'static str;

    /// Builds the command used when a human or HQ drives the supervisor.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Command;

    /// Builds the MCP configuration entry for this supervisor instance.
    ///
    /// The default implementation points the client back at the current
    /// `ferrus` executable so HQ can serve tools through the repository's own
    /// binary rather than relying on an external wrapper.
    ///
    /// # Errors
    ///
    /// Returns an error when Ferrus cannot resolve the path to the current
    /// executable.
    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
            model: self.model().map(ToOwned::to_owned),
        })
    }

    /// Returns the optional model override used by this backend.
    fn model(&self) -> Option<&str>;

    /// Describes how startup prompt text should be delivered.
    fn prompt_transport(&self) -> PromptTransport {
        PromptTransport::Argv
    }
}

/// Behavior required from an executor-capable agent implementation.
///
/// Executors mirror the supervisor API because HQ may start them in interactive
/// or headless modes depending on the orchestration context.
pub trait ExecutorAgent: Send + Sync {
    /// Returns the stable configuration name for this agent backend.
    fn name(&self) -> &'static str;

    /// Builds the command used when a human or HQ drives the executor.
    fn spawn(&self, mode: AgentRunMode<'_>) -> Command;

    /// Builds the MCP configuration entry for this executor instance.
    ///
    /// # Errors
    ///
    /// Returns an error when Ferrus cannot resolve the path to the current
    /// executable.
    fn mcp_config_entry(&self, role: &str, index: u32) -> Result<McpConfigEntry> {
        Ok(McpConfigEntry {
            command: current_exe_string()?,
            args: serve_args(role, self.name(), index),
            model: self.model().map(ToOwned::to_owned),
        })
    }

    /// Returns the optional model override used by this backend.
    fn model(&self) -> Option<&str>;

    /// Describes how startup prompt text should be delivered.
    fn prompt_transport(&self) -> PromptTransport {
        PromptTransport::Argv
    }
}

/// Parses a configured supervisor agent name into its concrete implementation.
///
/// # Errors
///
/// Returns an error that lists the supported agent names when `agent_type` does
/// not match a registered supervisor backend.
pub fn parse_supervisor_agent(
    agent_type: &str,
    model: Option<&str>,
) -> Result<Arc<dyn SupervisorAgent>> {
    match agent_type {
        claude::NAME => Ok(Arc::new(claude::Supervisor::new(model))),
        codex::NAME => Ok(Arc::new(codex::Supervisor::new(model))),
        qwen::NAME => Ok(Arc::new(qwen::Supervisor::new(model))),
        other => bail!(
            "Unknown supervisor agent '{other}'. Supported values: \"claude-code\", \"codex\", \"qwen-code\"."
        ),
    }
}

/// Parses a configured executor agent name into its concrete implementation.
///
/// # Errors
///
/// Returns an error that lists the supported agent names when `agent_type` does
/// not match a registered executor backend.
pub fn parse_executor_agent(
    agent_type: &str,
    model: Option<&str>,
) -> Result<Arc<dyn ExecutorAgent>> {
    match agent_type {
        claude::NAME => Ok(Arc::new(claude::Executor::new(model))),
        codex::NAME => Ok(Arc::new(codex::Executor::new(model))),
        qwen::NAME => Ok(Arc::new(qwen::Executor::new(model))),
        other => bail!(
            "Unknown executor agent '{other}'. Supported values: \"claude-code\", \"codex\", \"qwen-code\"."
        ),
    }
}

fn current_exe_string() -> Result<String> {
    // Persist the exact executable path so generated MCP configs keep working
    // even when Ferrus is launched outside of PATH-based resolution.
    Ok(std::env::current_exe()
        .context("Failed to resolve current executable path")?
        .to_string_lossy()
        .into_owned())
}

fn serve_args(role: &str, agent_name: &str, index: u32) -> Vec<String> {
    // Ferrus reconnects to agents through `ferrus serve`, so every backend uses
    // the same argument shape with role and agent identity baked in.
    vec![
        "serve".to_string(),
        "--role".to_string(),
        role.to_string(),
        "--agent-name".to_string(),
        agent_name.to_string(),
        "--agent-index".to_string(),
        index.to_string(),
    ]
}

pub(crate) fn normalized_model(model: Option<&str>) -> Option<String> {
    model.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub(crate) async fn allow_mcp_server_tools_in_json_settings(
    path: &Path,
    server_key: &str,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut root: Value = if path.exists() {
        let content = tokio::fs::read_to_string(path).await?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?
    } else {
        serde_json::json!({})
    };

    let permission = mcp_server_tools_permission(server_key);
    let added = add_json_allow_permission(&mut root, &permission, path)?;
    let content = serde_json::to_string_pretty(&root)?;
    tokio::fs::write(path, content).await?;
    if added {
        println!("Allowed {permission} in {}", path.display());
    }
    Ok(())
}

fn mcp_server_tools_permission(server_key: &str) -> String {
    format!("mcp__{server_key}__*")
}

fn add_json_allow_permission(root: &mut Value, permission: &str, path: &Path) -> Result<bool> {
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} root is not a JSON object", path.display()))?;

    let permissions = root_obj
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    let permissions_obj = permissions
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} permissions is not an object", path.display()))?;

    let allow = permissions_obj
        .entry("allow")
        .or_insert_with(|| serde_json::json!([]));
    let allow_array = allow
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("{} permissions.allow is not an array", path.display()))?;

    if allow_array
        .iter()
        .any(|value| value.as_str() == Some(permission))
    {
        return Ok(false);
    }
    if allow_array.iter().any(|value| !value.is_string()) {
        bail!(
            "{} permissions.allow must contain only strings",
            path.display()
        );
    }

    allow_array.push(Value::String(permission.to_string()));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn assert_program_and_args(command: Command, program: &str, args: &[&str]) {
        assert_eq!(command.get_program().to_string_lossy(), program);
        let actual = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let expected = args
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn unknown_supervisor_agent_is_actionable() {
        let err = match parse_supervisor_agent("unknown", None) {
            Ok(_) => panic!("expected unknown supervisor agent to fail"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("Unknown supervisor agent 'unknown'"));
        assert!(err.contains("claude-code"));
        assert!(err.contains("codex"));
        assert!(err.contains("qwen-code"));
    }

    #[test]
    fn unknown_executor_agent_is_actionable() {
        let err = match parse_executor_agent("unknown", None) {
            Ok(_) => panic!("expected unknown executor agent to fail"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("Unknown executor agent 'unknown'"));
        assert!(err.contains("claude-code"));
        assert!(err.contains("codex"));
        assert!(err.contains("qwen-code"));
    }

    #[test]
    fn blank_model_is_normalized_to_none() {
        assert_eq!(normalized_model(None), None);
        assert_eq!(normalized_model(Some("")), None);
        assert_eq!(normalized_model(Some("  ")), None);
        assert_eq!(
            normalized_model(Some("gpt-5.4")),
            Some("gpt-5.4".to_string())
        );
    }

    #[test]
    fn mcp_permission_uses_mcp_server_wildcard() {
        assert_eq!(
            mcp_server_tools_permission("ferrus-supervisor-1"),
            "mcp__ferrus-supervisor-1__*"
        );
    }

    #[test]
    fn add_json_allow_permission_preserves_existing_entries() {
        let mut root = serde_json::json!({
            "permissions": {
                "allow": ["Bash(cargo test)"]
            }
        });

        let added = add_json_allow_permission(
            &mut root,
            "mcp__ferrus-executor-1__*",
            Path::new(".claude/settings.local.json"),
        )
        .unwrap();
        assert!(added);
        assert_eq!(
            root["permissions"]["allow"],
            serde_json::json!(["Bash(cargo test)", "mcp__ferrus-executor-1__*"])
        );
    }

    #[test]
    fn add_json_allow_permission_is_idempotent() {
        let mut root = serde_json::json!({
            "permissions": {
                "allow": ["mcp__ferrus-supervisor-1__*"]
            }
        });

        let added = add_json_allow_permission(
            &mut root,
            "mcp__ferrus-supervisor-1__*",
            Path::new(".qwen/settings.json"),
        )
        .unwrap();
        assert!(!added);
        assert_eq!(
            root["permissions"]["allow"],
            serde_json::json!(["mcp__ferrus-supervisor-1__*"])
        );
    }
}
