//! AgentMessage model — the transcript type the loop works in (converted to
//! LLM wire messages only at the provider boundary). Faithful to pi-agent-core.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub total: u64,
    pub cost_usd: f64,
}

/// A block of assistant content.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    Thinking(String),
    ToolCall { id: String, name: String, arguments: Value },
}

#[derive(Debug, Clone)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
    pub usage: Usage,
}

impl AssistantMessage {
    pub fn text(&self) -> String {
        let mut s = String::new();
        for b in &self.content {
            if let ContentBlock::Text(t) = b {
                s.push_str(t);
            }
        }
        s
    }

    /// The tool-call blocks, in order.
    pub fn tool_calls(&self) -> Vec<(String, String, Value)> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall { id, name, arguments } => {
                    Some((id.clone(), name.clone(), arguments.clone()))
                }
                _ => None,
            })
            .collect()
    }

    pub fn failure(stop_reason: StopReason, error: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text(String::new())],
            stop_reason,
            error_message: Some(error.into()),
            usage: Usage::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
}

/// The transcript message union (user / assistant / tool result).
#[derive(Debug, Clone)]
pub enum AgentMessage {
    User(String),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

/// The value a tool returns.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    /// If every tool call in a batch returns terminate=true, the loop stops.
    pub terminate: bool,
}

impl ToolResult {
    pub fn text(content: impl Into<String>) -> Self {
        Self { content: content.into(), terminate: false }
    }
}
