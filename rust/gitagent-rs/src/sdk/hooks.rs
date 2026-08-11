//! Script hooks — `hooks.yaml` `pre_tool_use` commands run as a `ToolGate`.
//!
//! Each hook command receives `{"tool": <name>, "args": <value>}` as JSON on
//! stdin and may print `{"action": "deny", "message": "..."}` on stdout to block
//! the call (any other output = allow). A hook that fails to spawn or parse is
//! treated as allow (fail-open) so a broken hook never wedges a run — the crash
//! that took down the TS harness (G17) can't happen here.

use crate::pi::gate::{GateDecision, ToolGate};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Deserialize)]
struct HooksFile {
    #[serde(default)]
    pre_tool_use: Vec<HookEntry>,
}

#[derive(Deserialize)]
struct HookEntry {
    command: String,
}

pub struct HookGate {
    commands: Vec<String>,
    cwd: PathBuf,
}

/// Load `hooks.yaml`; returns None when there are no pre_tool_use hooks.
pub fn load_hook_gate(dir: &Path) -> Option<HookGate> {
    let raw = std::fs::read_to_string(dir.join("hooks.yaml")).ok()?;
    let parsed: HooksFile = serde_yaml::from_str(&raw).ok()?;
    let commands: Vec<String> = parsed.pre_tool_use.into_iter().map(|h| h.command).collect();
    if commands.is_empty() {
        return None;
    }
    Some(HookGate { commands, cwd: dir.to_path_buf() })
}

async fn run_hook(cmd: &str, cwd: &Path, payload: &str) -> Option<Value> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes()).await;
        // Drop stdin so the child sees EOF.
    }
    let out = child.wait_with_output().await.ok()?;
    serde_json::from_slice::<Value>(&out.stdout).ok()
}

#[async_trait]
impl ToolGate for HookGate {
    async fn check(&self, name: &str, args: &Value) -> GateDecision {
        let payload = json!({ "tool": name, "args": args }).to_string();
        for cmd in &self.commands {
            if let Some(v) = run_hook(cmd, &self.cwd, &payload).await {
                if v.get("action").and_then(Value::as_str) == Some("deny") {
                    let msg = v.get("message").and_then(Value::as_str).unwrap_or("blocked by hook");
                    return GateDecision::Deny(msg.to_string());
                }
            }
        }
        GateDecision::Allow
    }
}
