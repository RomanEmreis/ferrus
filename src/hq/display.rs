use crate::state::{
    agents::{AgentStatus, AgentsRegistry},
    machine::{StateData, TaskState},
};

pub fn print_status(state: &StateData, agents: &AgentsRegistry) {
    println!();
    println!("  state      : {:?}", state.state);
    if let Some(by) = &state.claimed_by {
        println!("  claimed_by : {by}");
    }
    if state.check_retries > 0 {
        println!("  retries    : {}", state.check_retries);
    }
    if state.review_cycles > 0 {
        println!("  cycles     : {}", state.review_cycles);
    }
    if agents.agents.is_empty() {
        println!("  agents     : none");
    } else {
        for a in &agents.agents {
            let s = match a.status {
                AgentStatus::Idle => "idle",
                AgentStatus::Running => "running",
                AgentStatus::Suspended => "suspended",
            };
            let pid = a.pid.map(|p| format!(" pid={p}")).unwrap_or_default();
            println!("  [{:<10}] {:<10}{}", a.role, s, pid);
        }
    }
    println!();
}

pub fn print_transition(from: &TaskState, to: &TaskState) {
    println!("\n  ── {:?} → {:?} ──\n", from, to);
}

pub fn print_info(msg: &str) {
    println!("  {msg}");
}
pub fn print_error(msg: &str) {
    println!("  error: {msg}");
}
