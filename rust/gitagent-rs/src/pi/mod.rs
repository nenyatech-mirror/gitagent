//! `pi` — a faithful Rust port of the pi-agent-core engine (the agent loop,
//! streaming, tool execution, steering/follow-up, abort). The provider layer
//! targets the OpenAI-compatible API in place of pi-ai's provider zoo.

pub mod agent;
pub mod agent_loop;
pub mod compact;
pub mod event;
pub mod gate;
pub mod message;
pub mod provider;
pub mod tool;
