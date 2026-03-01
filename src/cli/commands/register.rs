use anyhow::Result;

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    #[value(name = "claude-code")]
    ClaudeCode,
    Codex,
}

pub async fn run(supervisor: Agent, executor: Agent) -> Result<()> {
    let mut claude_roles: Vec<&str> = Vec::new();
    let mut codex_roles: Vec<&str> = Vec::new();

    match &supervisor {
        Agent::ClaudeCode => claude_roles.push("supervisor"),
        Agent::Codex => codex_roles.push("supervisor"),
    }
    match &executor {
        Agent::ClaudeCode => claude_roles.push("executor"),
        Agent::Codex => codex_roles.push("executor"),
    }

    if !claude_roles.is_empty() {
        write_claude_code(&claude_roles).await?;
    }
    if !codex_roles.is_empty() {
        write_codex(&codex_roles).await?;
    }

    Ok(())
}

async fn write_claude_code(roles: &[&str]) -> Result<()> {
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

    for role in roles {
        servers_obj.insert(
            format!("ferrus-{role}"),
            serde_json::json!({
                "command": "ferrus",
                "args": ["serve", "--role", role]
            }),
        );
        println!("Registered ferrus-{role} in .mcp.json");
    }

    let content = serde_json::to_string_pretty(&root)?;
    tokio::fs::write(path, content).await?;

    Ok(())
}

async fn write_codex(roles: &[&str]) -> Result<()> {
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

    for role in roles {
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
            ]),
        );
        mcp_servers.insert(format!("ferrus-{role}"), toml::Value::Table(entry));
        println!("Registered ferrus-{role} in .codex/config.toml");
    }

    let content = toml::to_string_pretty(&table)?;
    tokio::fs::write(&path, content).await?;

    Ok(())
}
