use anyhow::Result;

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
            "command": "ferrus",
            "args": ["serve", "--role", role, "--agent-name", agent_name, "--agent-index", index.to_string()]
        }),
    );
    println!("Registered {key} in .mcp.json (agent_id will be \"{role}:{agent_name}:{index}\")");

    let content = serde_json::to_string_pretty(&root)?;
    tokio::fs::write(path, content).await?;
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
    Ok(())
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
