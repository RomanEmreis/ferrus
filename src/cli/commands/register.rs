use anyhow::{Context, Result};

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    #[value(name = "claude-code")]
    ClaudeCode,
    Codex,
}

impl Agent {
    /// The string representation used in --agent-name CLI flags and claimed_by identifiers.
    pub fn name(&self) -> &str {
        match self {
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
        }
    }
}

pub async fn run(supervisor: Option<Agent>, executor: Option<Agent>) -> Result<()> {
    if let Some(agent) = &supervisor {
        register_role("supervisor", agent).await?;
    }
    if let Some(agent) = &executor {
        register_role("executor", agent).await?;
    }
    Ok(())
}

async fn register_role(role: &str, agent: &Agent) -> Result<()> {
    let agent_name = agent.name();
    match agent {
        Agent::ClaudeCode => register_claude_code(role, agent_name).await,
        Agent::Codex => register_codex(role, agent_name).await,
    }
}

async fn register_claude_code(role: &str, agent_name: &str) -> Result<()> {
    let path = std::path::Path::new(".mcp.json");

    // Use the absolute path of the running binary so Claude Code can find it
    // regardless of its PATH environment (which may not include ~/.cargo/bin).
    let command = std::env::current_exe()
        .context("Failed to resolve current executable path")?
        .to_string_lossy()
        .into_owned();

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

    servers_obj.insert(
        key.clone(),
        serde_json::json!({
            "command": command,
            "args": ["serve", "--role", role, "--agent-name", agent_name, "--agent-index", index.to_string()]
        }),
    );
    println!("Registered {key} in .mcp.json (agent_id will be \"{role}:{agent_name}:{index}\")");

    let content = serde_json::to_string_pretty(&root)?;
    tokio::fs::write(path, content).await?;

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

async fn register_codex(role: &str, agent_name: &str) -> Result<()> {
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

    let mut entry = toml::Table::new();
    entry.insert(
        "command".to_string(),
        toml::Value::String("ferrus".to_string()),
    );
    entry.insert(
        "args".to_string(),
        toml::Value::Array(vec![
            toml::Value::String("serve".to_string()),
            toml::Value::String("--role".to_string()),
            toml::Value::String(role.to_string()),
            toml::Value::String("--agent-name".to_string()),
            toml::Value::String(agent_name.to_string()),
            toml::Value::String("--agent-index".to_string()),
            toml::Value::String(index.to_string()),
        ]),
    );
    mcp_servers.insert(key.clone(), toml::Value::Table(entry));
    println!(
        "Registered {key} in .codex/config.toml (agent_id will be \"{role}:{agent_name}:{index}\")"
    );

    let content = toml::to_string_pretty(&table)?;
    tokio::fs::write(&path, content).await?;

    append_to_agents_md(role).await?;
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
        "executor" => format!(
            "\n{marker}\n\
             ## Ferrus Executor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             When spawned by `ferrus` HQ, your initial prompt will tell you what to do.\n\n\
             If started manually: call MCP tool `/wait_for_task` as your first action.\n\n\
             Full workflow: `.agents/skills/ferrus-executor/SKILL.md`\n"
        ),
        "supervisor" => format!(
            "\n{marker}\n\
             ## Ferrus Supervisor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             When spawned via `/plan`: collaborate with the user to define the task, then call `/create_task`.\n\n\
             When spawned for review: your initial prompt will direct you — read TASK.md + SUBMISSION.md,\n\
             then call `/review_pending`, `/approve` or `/reject`.\n\n\
             If started manually: call `/status` first, then follow \
             `.agents/skills/ferrus-supervisor/SKILL.md`.\n"
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

fn claude_md_section(role: &str, marker: &str) -> String {
    match role {
        "executor" => format!(
            "\n{marker}\n\
             ## Ferrus Executor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             When spawned by `ferrus` HQ, your initial prompt will tell you what to do.\n\n\
             If started manually: call MCP tool `/wait_for_task` as your first action.\n\n\
             Full workflow: `.agents/skills/ferrus-executor/SKILL.md`\n"
        ),
        "supervisor" => format!(
            "\n{marker}\n\
             ## Ferrus Supervisor\n\n\
             This repository is orchestrated by Ferrus HQ.\n\n\
             When spawned via `/plan`: collaborate with the user to define the task, then call `/create_task`.\n\n\
             When spawned for review: your initial prompt will direct you — read TASK.md + SUBMISSION.md,\n\
             then call `/review_pending`, `/approve` or `/reject`.\n\n\
             If started manually: call `/status` first, then follow \
             `.agents/skills/ferrus-supervisor/SKILL.md`.\n"
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
