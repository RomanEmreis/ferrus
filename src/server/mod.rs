use anyhow::Result;
use neva::App;
use neva::types::ToolSchema;

mod tools;

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Role {
    Supervisor,
    Executor,
}

pub async fn start(role: Option<Role>) -> Result<()> {
    let mut app = App::new().with_options(|opt| {
        opt.with_stdio()
            .with_name("ferrus")
            .with_version(env!("CARGO_PKG_VERSION"))
    });

    let sup = role.as_ref().is_none_or(|r| *r == Role::Supervisor);
    let exe = role.as_ref().is_none_or(|r| *r == Role::Executor);

    if sup {
        app.map_tool("create_task", tools::create_task::handler)
            .with_description(tools::create_task::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::create_task::INPUT_SCHEMA));
        app.map_tool("review_pending", tools::review_pending::handler)
            .with_description(tools::review_pending::DESCRIPTION);
        app.map_tool("approve", tools::approve::handler)
            .with_description(tools::approve::DESCRIPTION);
        app.map_tool("reject", tools::reject::handler)
            .with_description(tools::reject::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::reject::INPUT_SCHEMA));
    }

    if exe {
        app.map_tool("next_task", tools::next_task::handler)
            .with_description(tools::next_task::DESCRIPTION);
        app.map_tool("check", tools::check::handler)
            .with_description(tools::check::DESCRIPTION);
        app.map_tool("submit", tools::submit::handler)
            .with_description(tools::submit::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::submit::INPUT_SCHEMA));
    }

    app.map_tool("status", tools::status::handler)
        .with_description(tools::status::DESCRIPTION);
    app.map_tool("reset", tools::reset::handler)
        .with_description(tools::reset::DESCRIPTION);

    app.run().await;
    Ok(())
}
