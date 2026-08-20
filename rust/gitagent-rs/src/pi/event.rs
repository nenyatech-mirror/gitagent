//! Lifecycle events emitted by the agent loop (faithful to pi-agent-core's
//! AgentEvent, with the streaming `message_update` reduced to a text/thinking
//! delta since that's all a consumer needs).

use crate::pi::message::AssistantMessage;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Thinking,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentStart,
    TurnStart,
    /// A user/steering message was injected into the transcript.
    UserMessage(String),
    /// Streaming chunk of the current assistant message.
    MessageDelta { kind: DeltaKind, text: String },
    /// The assistant message finished (carries usage + stop reason).
    MessageEnd(AssistantMessage),
    ToolExecutionStart { tool_call_id: String, tool_name: String, args: Value },
    ToolExecutionEnd { tool_call_id: String, tool_name: String, content: String, is_error: bool },
    TurnEnd,
    AgentEnd,
}
