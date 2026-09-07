//! Provider boundary. Adapters assemble complete responses; partial arguments are display-only.

use super::tools::{ToolCall, ToolDescriptor, ToolOutcome};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,

    /// False is a host estimate, never provider-reported billing data.
    pub reported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FinishReason {
    Stop,
    ToolCalls,
    Length,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelResponse {
    pub finish: FinishReason,
    pub text: String,
    pub calls: Vec<ToolCall>,

    /// Adapter-owned continuation data (including signed blocks), preserved without interpretation.
    pub continuation: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub(crate) enum Message {
    User {
        text: String,
    },
    Assistant {
        response: ModelResponse,
    },
    Tool {
        provider_call_id: String,
        outcome: ToolOutcome,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDescriptor>,
    pub max_output_tokens: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum ProviderEvent {
    TextDelta(String),
    ArgumentsDelta(String),
    Completed {
        response: ModelResponse,
        usage: Option<Usage>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderError {
    pub retryable: bool,
}

pub(crate) trait Provider {
    /// Both start and next_event must be safe to drop on cancellation/deadline.
    async fn start(&mut self, request: ModelRequest) -> Result<(), ProviderError>;

    async fn next_event(&mut self) -> Result<Option<ProviderEvent>, ProviderError>;
}
