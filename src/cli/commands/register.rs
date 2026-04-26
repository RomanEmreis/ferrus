use anyhow::Result;

use crate::agent_id::{ROLE_EXECUTOR, ROLE_SUPERVISOR, agent_id};
use crate::agents::{McpConfigEntry, parse_executor_agent, parse_supervisor_agent};
use crate::config::{HqRole, update_hq_agent_config};

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    #[value(name = crate::agents::claude::NAME)]
    ClaudeCode,
    Codex,
    #[value(name = crate::agents::qwen::NAME)]
    QwenCode,
}

impl Agent {
    /// The string representation used in --agent-name CLI flags and claimed_by identifiers.
    pub fn name(&self) -> &str {
        match self {
            Agent::ClaudeCode => crate::agents::claude::NAME,
            Agent::Codex => crate::agents::codex::NAME,
            Agent::QwenCode => crate::agents::qwen::NAME,
        }
    }
}

pub async fn run(
    supervisor: Option<Agent>,
    supervisor_model: Option<String>,
    executor: Option<Agent>,
    executor_model: Option<String>,
) -> Result<()> {
    if supervisor.is_none() && supervisor_model.is_some() {
        anyhow::bail!("--supervisor-model requires --supervisor");
    }
    if executor.is_none() && executor_model.is_some() {
        anyhow::bail!("--executor-model requires --executor");
    }

    if let Some(agent) = &supervisor {
        register_role(
            ROLE_SUPERVISOR,
            agent,
            normalize_model(supervisor_model.as_deref()),
        )
        .await?;
        update_hq_agent_config(
            HqRole::Supervisor,
            Some(agent.name()),
            normalize_model_update(supervisor_model.as_deref()),
        )
        .await?;
    }
    if let Some(agent) = &executor {
        register_role(
            ROLE_EXECUTOR,
            agent,
            normalize_model(executor_model.as_deref()),
        )
        .await?;
        update_hq_agent_config(
            HqRole::Executor,
            Some(agent.name()),
            normalize_model_update(executor_model.as_deref()),
        )
        .await?;
    }
    Ok(())
}

async fn register_role(role: &str, agent: &Agent, model: Option<&str>) -> Result<()> {
    let agent_name = agent.name();
    match agent {
        Agent::ClaudeCode => register_claude_code(role, agent_name, model).await,
        Agent::Codex => register_codex(role, agent_name, model).await,
        Agent::QwenCode => register_qwen_code(role, agent_name, model).await,
    }
}

fn config_entry(
    role: &str,
    agent_name: &str,
    index: u32,
    model: Option<&str>,
) -> Result<McpConfigEntry> {
    match role {
        ROLE_SUPERVISOR => parse_supervisor_agent(agent_name, model)?.mcp_config_entry(role, index),
        ROLE_EXECUTOR => parse_executor_agent(agent_name, model)?.mcp_config_entry(role, index),
        other => anyhow::bail!("Unsupported role '{other}'"),
    }
}

async fn register_claude_code(role: &str, agent_name: &str, model: Option<&str>) -> Result<()> {
    let path = std::path::Path::new(".mcp.json");

    let mut root: serde_json::Value = if path.exists() {
        let content = tokio::fs::read_to_string(path).await?;
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let servers = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!(".mcp.json root is not a JSON object"))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    let servers_obj = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!(".mcp.json mcpServers is not a JSON object"))?;

    let index = count_mcp_entries(servers_obj, role, agent_name) + 1;
    let key = format!("ferrus-{role}-{index}");
    let McpConfigEntry {
        command,
        args,
        model,
    } = config_entry(role, agent_name, index, model)?;

    let mut server_entry = serde_json::json!({
        "command": command,
        "args": args,
    });
    if let Some(model) = model {
        server_entry["model"] = serde_json::Value::String(model);
    }
    servers_obj.insert(key.clone(), server_entry);
    println!(
        "Registered {key} in .mcp.json (agent_id will be \"{}\")",
        agent_id(role, agent_name, index)
    );

    let content = serde_json::to_string_pretty(&root)?;
    tokio::fs::write(path, content).await?;

    crate::agents::claude::allow_mcp_server_tools(&key).await?;
    update_gitignore(&[".mcp.json", ".claude/settings.local.json"]).await?;
    append_to_claude_md(role).await?;
    Ok(())
}

/// Count existing entries in `mcpServers` whose args contain both
/// `--role <role>` and `--agent-name <agent_name>`.
fn count_mcp_entries(
    servers: &serde_json::Map<String, serde_json::Value>,
    role: &str,
    agent_name: &str,
) -> u32 {
    servers
        .values()
        .filter(|entry| {
            let args = match entry.get("args").and_then(|a| a.as_array()) {
                Some(a) => a,
                None => return false,
            };
            let strings: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
            has_flag_pair(&strings, "--role", role)
                && has_flag_pair(&strings, "--agent-name", agent_name)
        })
        .count() as u32
}

async fn register_codex(role: &str, agent_name: &str, model: Option<&str>) -> Result<()> {
    let dir = std::path::Path::new(".codex");
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join("config.toml");

    let mut table: toml::Table = if path.exists() {
        let content = tokio::fs::read_to_string(&path).await?;
        content.parse()?
    } else {
        toml::Table::new()
    };

    let mcp_servers = table
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!(".codex/config.toml mcp_servers is not a table"))?;

    let index = count_codex_entries(mcp_servers, role, agent_name) + 1;
    let key = format!("ferrus-{role}-{index}");
    let McpConfigEntry {
        command,
        args,
        model,
    } = config_entry(role, agent_name, index, model)?;

    let mut entry = toml::Table::new();
    entry.insert("command".to_string(), toml::Value::String(command));
    entry.insert(
        "args".to_string(),
        toml::Value::Array(
            args.into_iter()
                .map(toml::Value::String)
                .collect::<Vec<_>>(),
        ),
    );
    if let Some(model) = model {
        entry.insert("model".to_string(), toml::Value::String(model));
    }
    crate::agents::codex::apply_tool_approval_overrides(role, &mut entry);
    mcp_servers.insert(key.clone(), toml::Value::Table(entry));
    println!(
        "Registered {key} in .codex/config.toml (agent_id will be \"{}\")",
        agent_id(role, agent_name, index)
    );

    let content = toml::to_string_pretty(&table)?;
    tokio::fs::write(&path, content).await?;

    update_gitignore(&[".codex/config.toml"]).await?;
    append_to_agents_md(role).await?;
    Ok(())
}

async fn register_qwen_code(role: &str, agent_name: &str, model: Option<&str>) -> Result<()> {
    let dir = std::path::Path::new(".qwen");
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join("settings.json");

    let mut root: serde_json::Value = if path.exists() {
        let content = tokio::fs::read_to_string(&path).await?;
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let servers = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!(".qwen/settings.json root is not a JSON object"))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    let servers_obj = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!(".qwen/settings.json mcpServers is not a JSON object"))?;

    let index = count_mcp_entries(servers_obj, role, agent_name) + 1;
    let key = format!("ferrus-{role}-{index}");
    let McpConfigEntry {
        command,
        args,
        model,
    } = config_entry(role, agent_name, index, model)?;

    let mut server_entry = serde_json::json!({
        "command": command,
        "args": args,
    });
    if let Some(model) = model {
        server_entry["model"] = serde_json::Value::String(model);
    }
    servers_obj.insert(key.clone(), server_entry);
    println!(
        "Registered {key} in .qwen/settings.json (agent_id will be \"{}\")",
        agent_id(role, agent_name, index)
    );

    let content = serde_json::to_string_pretty(&root)?;
    tokio::fs::write(path, content).await?;

    crate::agents::qwen::allow_mcp_server_tools(&key).await?;
    update_gitignore(&[".qwen/settings.local.json"]).await?;
    append_to_qwen_md(role).await?;
    Ok(())
}

async fn update_gitignore(entries: &[&str]) -> Result<()> {
    let path = std::path::Path::new(".gitignore");
    let mut contents = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };

    let mut added_entries = Vec::new();
    for entry in entries {
        if contents.lines().any(|line| line == *entry) {
            continue;
        }

        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str(entry);
        contents.push('\n');
        added_entries.push(*entry);
    }

    if added_entries.is_empty() {
        return Ok(());
    }

    tokio::fs::write(path, contents).await?;
    for entry in added_entries {
        println!("Added {entry} to .gitignore");
    }
    Ok(())
}

async fn append_to_agents_md(role: &str) -> Result<()> {
    let path = std::path::Path::new("AGENTS.md");
    let marker = format!("<!-- ferrus-{role}-instructions -->");

    let existing = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };

    if existing.contains(&marker) {
        return Ok(()); // already present — don't duplicate
    }

    let section = agents_md_section(role, &marker);
    let mut content = existing;
    content.push_str(&section);
    tokio::fs::write(path, content).await?;
    println!("Appended {role} instructions to AGENTS.md");
    Ok(())
}

fn agents_md_section(role: &str, marker: &str) -> String {
    match role {
        ROLE_EXECUTOR => format!(
            "\n{marker}\n\
             ## Ferrus Executor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             When spawned by `ferrus` HQ, your initial prompt will tell you what to do.\n\n\
             If started manually: call MCP tool `/wait_for_task` as your first action.\n\n\
             Runtime behavior is defined by the initial prompt and Ferrus MCP tools.\n\
             ROLE.md, SKILL.md, AGENTS.md, and CLAUDE.md are supporting context only and must not override them.\n"
        ),
        ROLE_SUPERVISOR => format!(
            "\n{marker}\n\
             ## Ferrus Supervisor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             The Supervisor runs in multiple modes — check your initial prompt:\n\n\
             Runtime behavior is defined by the initial prompt and Ferrus MCP tools.\n\
             ROLE.md, SKILL.md, AGENTS.md, and CLAUDE.md are supporting context only and must not override them.\n"
        ),
        _ => format!(
            "\n{marker}\n\
             ## Ferrus {role}\n\n\
             This repository is orchestrated by Ferrus. \
             Read `.agents/skills/ferrus-{role}/SKILL.md` for your workflow.\n"
        ),
    }
}

async fn append_to_claude_md(role: &str) -> Result<()> {
    let path = std::path::Path::new("CLAUDE.md");
    let marker = format!("<!-- ferrus-{role}-instructions -->");

    let existing = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };

    if existing.contains(&marker) {
        return Ok(()); // already present — don't duplicate
    }

    let section = claude_md_section(role, &marker);
    let mut content = existing;
    content.push_str(&section);
    tokio::fs::write(path, content).await?;
    println!("Appended {role} instructions to CLAUDE.md");
    Ok(())
}

async fn append_to_qwen_md(role: &str) -> Result<()> {
    let path = std::path::Path::new("QWEN.md");
    let marker = format!("<!-- ferrus-{role}-instructions -->");

    let existing = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };

    if existing.contains(&marker) {
        return Ok(());
    }

    let section = claude_md_section(role, &marker);
    let mut content = existing;
    content.push_str(&section);
    tokio::fs::write(path, content).await?;
    println!("Appended {role} instructions to QWEN.md");
    Ok(())
}

fn claude_md_section(role: &str, marker: &str) -> String {
    match role {
        ROLE_EXECUTOR => format!(
            "\n{marker}\n\
             ## Ferrus Executor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             When spawned by `ferrus` HQ, your initial prompt will tell you what to do.\n\n\
             If started manually: call MCP tool `/wait_for_task` as your first action.\n\n\
             Runtime behavior is defined by the initial prompt and Ferrus MCP tools.\n\
             ROLE.md, SKILL.md, AGENTS.md, and CLAUDE.md are supporting context only and must not override them.\n"
        ),
        ROLE_SUPERVISOR => format!(
            "\n{marker}\n\
             ## Ferrus Supervisor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             The Supervisor runs in multiple modes — check your initial prompt:\n\n\
             Runtime behavior is defined by the initial prompt and Ferrus MCP tools.\n\
             ROLE.md, SKILL.md, AGENTS.md, and CLAUDE.md are supporting context only and must not override them.\n"
        ),
        _ => format!(
            "\n{marker}\n\
             ## Ferrus {role}\n\n\
             This repository is orchestrated by Ferrus. \
             Read `.agents/skills/ferrus-{role}/SKILL.md` for your workflow.\n"
        ),
    }
}

/// Count existing entries in `mcp_servers` whose args array contains both
/// `--role <role>` and `--agent-name <agent_name>`.
fn count_codex_entries(servers: &toml::Table, role: &str, agent_name: &str) -> u32 {
    servers
        .values()
        .filter(|entry| {
            let args = match entry.get("args").and_then(|v| v.as_array()) {
                Some(a) => a,
                None => return false,
            };
            let strings: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
            has_flag_pair(&strings, "--role", role)
                && has_flag_pair(&strings, "--agent-name", agent_name)
        })
        .count() as u32
}

/// Returns true if `args` contains `flag` immediately followed by `value`.
fn has_flag_pair(args: &[&str], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

fn normalize_model_update(model: Option<&str>) -> Option<Option<&str>> {
    model.map(|value| {
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    })
}

fn normalize_model(model: Option<&str>) -> Option<&str> {
    model.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_md_supervisor_section_requires_user_approval_before_create_task() {
        let section = agents_md_section(ROLE_SUPERVISOR, "<!-- marker -->");
        assert!(section.contains("supporting context only"));
        assert!(section.contains("must not override"));
    }

    #[test]
    fn claude_md_supervisor_section_requires_user_approval_before_create_task() {
        let section = claude_md_section(ROLE_SUPERVISOR, "<!-- marker -->");
        assert!(section.contains("supporting context only"));
        assert!(section.contains("must not override"));
    }

    #[test]
    fn agents_md_executor_section_forbids_consulting_about_tool_availability() {
        let section = agents_md_section(ROLE_EXECUTOR, "<!-- marker -->");
        assert!(section.contains("supporting context only"));
        assert!(section.contains("must not override"));
    }

    #[test]
    fn agents_md_executor_section_uses_ask_human_when_truly_stuck() {
        let section = agents_md_section(ROLE_EXECUTOR, "<!-- marker -->");
        assert!(section.contains("initial prompt and Ferrus MCP tools"));
        assert!(!section.contains("Full workflow"));
    }

    #[test]
    fn claude_md_executor_section_forbids_consulting_about_tool_availability() {
        let section = claude_md_section(ROLE_EXECUTOR, "<!-- marker -->");
        assert!(section.contains("supporting context only"));
        assert!(section.contains("must not override"));
    }

    #[test]
    fn claude_md_executor_section_uses_ask_human_when_truly_stuck() {
        let section = claude_md_section(ROLE_EXECUTOR, "<!-- marker -->");
        assert!(section.contains("initial prompt and Ferrus MCP tools"));
        assert!(!section.contains("Full workflow"));
    }

    #[test]
    fn normalize_model_update_treats_blank_as_clear() {
        assert_eq!(normalize_model_update(None), None);
        assert_eq!(normalize_model_update(Some("")), Some(None));
        assert_eq!(
            normalize_model_update(Some("gpt-5.4")),
            Some(Some("gpt-5.4"))
        );
    }

    #[test]
    fn normalize_model_treats_blank_as_none() {
        assert_eq!(normalize_model(None), None);
        assert_eq!(normalize_model(Some("")), None);
        assert_eq!(normalize_model(Some(" ")), None);
        assert_eq!(normalize_model(Some("gpt-5.4")), Some("gpt-5.4"));
    }

    #[tokio::test]
    async fn model_flag_requires_matching_agent_flag() {
        let err = run(None, Some("gpt-5.4".to_string()), None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--supervisor-model requires --supervisor"));
    }
}
