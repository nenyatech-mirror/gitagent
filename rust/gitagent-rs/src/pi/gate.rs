//! Tool gate — the `beforeToolCall` seam from pi-agent-core. A gate runs before
//! a tool executes and can allow it, rewrite its arguments, or deny it (the deny
//! message becomes the tool result, so the model sees why). Gates compose: the
//! SDK layers a permission gate and a script-hook gate on top of the engine.

use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum GateDecision {
    /// Proceed with the (possibly already-modified) arguments.
    Allow,
    /// Proceed, but with these arguments instead.
    Modify(Value),
    /// Block execution; the message is returned to the model as an error result.
    Deny(String),
}

#[async_trait]
pub trait ToolGate: Send + Sync {
    async fn check(&self, tool_name: &str, args: &Value) -> GateDecision;
}
