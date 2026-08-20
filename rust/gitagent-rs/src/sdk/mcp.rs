//! MCP client (stdio transport) — a self-contained JSON-RPC 2.0 implementation
//! of the Model Context Protocol handshake (`initialize` → `notifications/
//! initialized` → `tools/list`), surfacing each server's tools as namespaced
//! `AgentTool`s (`mcp__<server>__<tool>`). Tool calls go through `tools/call`.
//!
//! Servers are declared in `mcp.yaml`:
//! ```yaml
//! servers:
//!   - name: fs
//!     command: "npx -y @modelcontextprotocol/server-filesystem ."
//! ```
//! The connection is a long-lived child process reused across calls. Blocking
//! stdio runs inside `spawn_blocking`, so it never stalls the async runtime.
//! A server that fails to start is skipped (its tools simply don't appear).

use crate::pi::message::ToolResult;
use crate::pi::tool::{AgentTool, ExecutionMode};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
struct McpFile {
    #[serde(default)]
    servers: Vec<ServerSpec>,
}

#[derive(Deserialize)]
struct ServerSpec {
    name: String,
    /// Shell command that launches the server (spawned via `sh -c`).
    command: String,
}

/// A live stdio connection to one MCP server.
pub struct McpConn {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
}

impl Drop for McpConn {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl McpConn {
    fn spawn(command: &str, cwd: &Path) -> Result<Child> {
        Ok(Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?)
    }

    /// Connect + handshake + list tools. Returns the connection and its tools.
    fn connect(command: &str, cwd: &Path) -> Result<(McpConn, Vec<ToolDef>)> {
        let mut child = Self::spawn(command, cwd)?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let mut conn = McpConn { child, stdin, reader: BufReader::new(stdout), next_id: 1 };

        conn.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "gitagent", "version": "0.1" }
            }),
        )?;
        conn.notify("notifications/initialized", json!({}))?;

        let listed = conn.request("tools/list", json!({}))?;
        let tools = listed
            .get("tools")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        Some(ToolDef {
                            name: t.get("name")?.as_str()?.to_string(),
                            description: t.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                            schema: t
                                .get("inputSchema")
                                .cloned()
                                .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((conn, tools))
    }

    fn write_message(&mut self, msg: &Value) -> Result<()> {
        self.stdin.write_all(msg.to_string().as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_message(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Send a request and return its `result` (skipping unrelated notifications).
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;

        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                return Err(anyhow!("MCP server closed the connection"));
            }
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
            // Skip notifications / responses to other ids.
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                return Err(anyhow!("MCP error: {err}"));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Result<String> {
        let res = self.request("tools/call", json!({ "name": name, "arguments": args }))?;
        // result.content is an array of typed parts; concatenate the text parts.
        let text = res
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Ok(if text.is_empty() { res.to_string() } else { text })
    }
}

struct ToolDef {
    name: String,
    description: String,
    schema: Value,
}

pub struct McpTool {
    /// Namespaced tool name exposed to the model: `mcp__<server>__<tool>`.
    display_name: String,
    /// The raw tool name the server expects in `tools/call`.
    raw_name: String,
    description: String,
    schema: Value,
    conn: Arc<Mutex<McpConn>>,
}

#[async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.display_name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        self.schema.clone()
    }
    // Network + side effects → serialize (never race in a parallel batch).
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }
    async fn execute(&self, _id: &str, args: Value, cancel: &CancellationToken) -> Result<ToolResult> {
        let conn = self.conn.clone();
        let raw = self.raw_name.clone();
        // Blocking stdio JSON-RPC runs off the async runtime.
        let call = tokio::task::spawn_blocking(move || conn.lock().unwrap().call_tool(&raw, args));
        tokio::pin!(call);
        let joined = tokio::select! {
            _ = cancel.cancelled() => return Ok(ToolResult::text("(aborted)")),
            r = &mut call => r,
        };
        match joined {
            Ok(Ok(text)) => Ok(ToolResult::text(text)),
            Ok(Err(e)) => Ok(ToolResult::text(format!("Error: {e}"))),
            Err(e) => Ok(ToolResult::text(format!("Error: {e}"))),
        }
    }
}

/// Connect every server in `mcp.yaml` and return their tools. Servers that fail
/// to start are skipped with a stderr note (they just don't contribute tools).
pub fn load_mcp_tools(dir: &Path) -> Vec<Arc<dyn AgentTool>> {
    let Ok(raw) = std::fs::read_to_string(dir.join("mcp.yaml")) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_yaml::from_str::<McpFile>(&raw) else {
        return Vec::new();
    };

    let mut out: Vec<Arc<dyn AgentTool>> = Vec::new();
    for server in parsed.servers {
        match McpConn::connect(&server.command, dir) {
            Ok((conn, defs)) => {
                let shared = Arc::new(Mutex::new(conn));
                for d in defs {
                    out.push(Arc::new(McpTool {
                        display_name: format!("mcp__{}__{}", server.name, d.name),
                        raw_name: d.name,
                        description: d.description,
                        schema: d.schema,
                        conn: shared.clone(),
                    }));
                }
            }
            Err(e) => eprintln!("\x1b[33mmcp: server '{}' failed to start: {e}\x1b[0m", server.name),
        }
    }
    out
}
