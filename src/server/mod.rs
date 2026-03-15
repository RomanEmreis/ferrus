use std::sync::Arc;

use anyhow::Result;
use neva::types::ToolSchema;
use neva::App;

mod prompts;
mod resources;
mod tools;

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Role {
    Supervisor,
    Executor,
}

pub async fn start(role: Option<Role>, agent_name: String, agent_index: u32) -> Result<()> {
    let role_str = match &role {
        Some(Role::Supervisor) => "supervisor",
        Some(Role::Executor) => "executor",
        None => "agent",
    };
    let agent_id = Arc::new(format!("{role_str}:{agent_name}:{agent_index}"));

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
        {
            let id = agent_id.clone();
            app.map_tool("wait_for_review", move || {
                let id = id.clone();
                async move { tools::wait_for_review::handler(&id).await }
            })
            .with_description(tools::wait_for_review::DESCRIPTION);
        }
        app.map_tool("review_pending", tools::review_pending::handler)
            .with_description(tools::review_pending::DESCRIPTION);
        app.map_tool("approve", tools::approve::handler)
            .with_description(tools::approve::DESCRIPTION);
        app.map_tool("reject", tools::reject::handler)
            .with_description(tools::reject::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::reject::INPUT_SCHEMA));
    }

    if exe {
        {
            let id = agent_id.clone();
            app.map_tool("wait_for_task", move || {
                let id = id.clone();
                async move { tools::wait_for_task::handler(&id).await }
            })
            .with_description(tools::wait_for_task::DESCRIPTION);
        }
        app.map_tool("next_task", tools::next_task::handler)
            .with_description(tools::next_task::DESCRIPTION);
        app.map_tool("check", tools::check::handler)
            .with_description(tools::check::DESCRIPTION);
        app.map_tool("submit", tools::submit::handler)
            .with_description(tools::submit::DESCRIPTION)
            .with_input_schema(|_| ToolSchema::from_json_str(tools::submit::INPUT_SCHEMA));
    }

    // Resources
    app.add_resource("ferrus://task", "Task");
    app.add_resource("ferrus://feedback", "Feedback");
    app.add_resource("ferrus://review", "Review Notes");
    app.add_resource("ferrus://submission", "Submission");
    app.add_resource("ferrus://question", "Question");
    app.add_resource("ferrus://state", "State");
    app.map_resource("ferrus://{file}", "ferrus-file", resources::read);

    // Prompts
    app.map_prompt("executor-context", prompts::executor_context)
        .with_description("Executor task context: state, task, feedback, and review notes");
    app.map_prompt("supervisor-review", prompts::supervisor_review)
        .with_description("Supervisor review context: state, task, and submission notes");

    // Shared tools (always registered regardless of role)
    app.map_tool("ask_human", tools::ask_human::handler)
        .with_description(tools::ask_human::DESCRIPTION)
        .with_input_schema(|_| ToolSchema::from_json_str(tools::ask_human::INPUT_SCHEMA));
    app.map_tool("answer", tools::answer::handler)
        .with_description(tools::answer::DESCRIPTION)
        .with_input_schema(|_| ToolSchema::from_json_str(tools::answer::INPUT_SCHEMA));
    app.map_tool("status", tools::status::handler)
        .with_description(tools::status::DESCRIPTION);
    app.map_tool("reset", tools::reset::handler)
        .with_description(tools::reset::DESCRIPTION);
    {
        let id = agent_id.clone();
        app.map_tool("heartbeat", move |agent_id: String| {
            let id = id.clone();
            async move { tools::heartbeat::handler(&id, agent_id).await }
        })
        .with_description(tools::heartbeat::DESCRIPTION)
        .with_input_schema(|_| ToolSchema::from_json_str(tools::heartbeat::INPUT_SCHEMA));
    }

    app.run().await;
    Ok(())
}
