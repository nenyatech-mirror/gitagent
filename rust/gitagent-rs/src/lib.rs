//! gitagent — Rust port.
//!
//! - `pi`  — a faithful port of the pi-agent-core engine: the agent loop,
//!   streaming, async tool execution (parallel by default; any Sequential tool
//!   serializes the batch), steering / follow-up queues, and abort.
//! - `sdk` — the gitagent SDK: agent loading, the builtin tools, and `query()`.

pub mod pi;
pub mod sdk;
