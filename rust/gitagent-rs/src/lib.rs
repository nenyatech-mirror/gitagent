//! Ira — a git-native AI agent (Rust).
//!
//! - `pi`  — a faithful port of the pi-agent-core engine: the agent loop,
//!   streaming, async tool execution (parallel by default; any Sequential tool
//!   serializes the batch), steering / follow-up queues, and abort.
//! - `sdk` — the agent SDK (gitagent-compatible): loading, builtin tools,
//!   `query()`, multi-turn `Session`, goal loop, eval harness.

pub mod pi;
pub mod sdk;
pub mod ui;
