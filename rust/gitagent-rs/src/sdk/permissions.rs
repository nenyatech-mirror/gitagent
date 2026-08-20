//! Permission gate — Claude-Code-style modes + allow/deny rules, applied to
//! every tool call via the engine's `ToolGate` seam.
//!
//! Rule syntax: `tool` or `tool(substring)` (a trailing `*` is ignored), e.g.
//! `cli(git )` matches any `cli` call whose command contains "git ". `*` matches
//! any tool. Precedence: bypass short-circuits → deny rules → allow rules →
//! mode default. `plan` mode blocks mutating tools unless explicitly allowed.

use crate::pi::gate::{GateDecision, ToolGate};
use crate::sdk::manifest::PermissionConfig;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Default,
    Plan,
    AcceptEdits,
    Bypass,
}

impl Mode {
    fn parse(s: &str) -> Mode {
        match s.trim().to_lowercase().as_str() {
            "plan" => Mode::Plan,
            "acceptedits" | "accept_edits" => Mode::AcceptEdits,
            "bypass" | "bypasspermissions" => Mode::Bypass,
            _ => Mode::Default,
        }
    }
}

#[derive(Debug, Clone)]
struct Rule {
    tool: String,
    contains: Option<String>,
}

impl Rule {
    fn parse(s: &str) -> Rule {
        let s = s.trim();
        if let Some((tool, rest)) = s.split_once('(') {
            let pat = rest.trim_end_matches(')').trim_end_matches('*').trim();
            Rule {
                tool: tool.trim().to_string(),
                contains: (!pat.is_empty()).then(|| pat.to_string()),
            }
        } else {
            Rule { tool: s.to_string(), contains: None }
        }
    }

    fn matches(&self, name: &str, args: &Value) -> bool {
        if self.tool != "*" && self.tool != name {
            return false;
        }
        match &self.contains {
            None => true,
            Some(p) => args_haystack(args).contains(p),
        }
    }
}

/// Best string to match a rule pattern against: the `command`/`path` field if
/// present, else the whole JSON.
fn args_haystack(args: &Value) -> String {
    for k in ["command", "path", "prompt"] {
        if let Some(s) = args.get(k).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    args.to_string()
}

pub struct PermissionGate {
    mode: Mode,
    allow: Vec<Rule>,
    deny: Vec<Rule>,
    /// Tool names treated as mutating (blocked in `plan` mode).
    mutating: HashSet<String>,
}

impl PermissionGate {
    /// Build from manifest config + the set of mutating tool names, with an
    /// optional CLI mode override (takes precedence over the manifest's mode).
    pub fn new(
        cfg: &PermissionConfig,
        mutating: HashSet<String>,
        mode_override: Option<&str>,
    ) -> Self {
        let mode_str = mode_override.filter(|s| !s.is_empty()).unwrap_or(&cfg.mode);
        PermissionGate {
            mode: Mode::parse(mode_str),
            allow: cfg.allow.iter().map(|s| Rule::parse(s)).collect(),
            deny: cfg.deny.iter().map(|s| Rule::parse(s)).collect(),
            mutating,
        }
    }

    /// True when this gate would never block anything (avoids a redundant gate).
    pub fn is_noop(&self) -> bool {
        self.mode == Mode::Default && self.allow.is_empty() && self.deny.is_empty()
    }
}

#[async_trait]
impl ToolGate for PermissionGate {
    async fn check(&self, name: &str, args: &Value) -> GateDecision {
        if self.mode == Mode::Bypass {
            return GateDecision::Allow;
        }
        if self.deny.iter().any(|r| r.matches(name, args)) {
            return GateDecision::Deny(format!("'{name}' is denied by a permission rule"));
        }
        if self.allow.iter().any(|r| r.matches(name, args)) {
            return GateDecision::Allow;
        }
        if self.mode == Mode::Plan && self.mutating.contains(name) {
            return GateDecision::Deny(format!(
                "plan mode is read-only; '{name}' would mutate state. Present a plan instead."
            ));
        }
        GateDecision::Allow
    }
}
