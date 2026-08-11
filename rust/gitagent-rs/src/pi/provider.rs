//! Streaming provider (OpenAI-compatible). Replaces pi-ai's `streamSimple` for
//! the OpenAI/gateway API. Emits text deltas as they arrive and returns the
//! assembled AssistantMessage. Like pi, provider/network errors are encoded as
//! `stop_reason = Error` (not thrown), so the loop stops cleanly.

use crate::pi::event::{AgentEvent, DeltaKind};
use crate::pi::message::{AssistantMessage, ContentBlock, StopReason, Usage};
use crate::pi::message::AgentMessage;
use crate::pi::tool::AgentTool;
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ModelSpec {
    pub model_id: String,
    pub base_url: String,
    pub api_key: String,
}

/// Sampling parameters forwarded to the provider (only serialized when set).
#[derive(Clone, Default)]
pub struct GenParams {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub top_p: Option<f64>,
}

/// Best-effort context window (tokens) for a model id — sizes compaction to the
/// real model instead of a flat guess. Unknown → 128k (a safe common default).
pub fn model_context_window(model_id: &str) -> usize {
    let m = model_id.to_lowercase();
    let has = |n: &str| m.contains(n);
    if has("gemini") {
        1_000_000
    } else if has("claude") || has("sonnet") || has("opus") || has("haiku") {
        200_000
    } else if has("o1") || has("o3") || has("o4") || has("gpt-5") {
        200_000
    } else if has("gpt-4.1") {
        1_000_000
    } else {
        128_000
    }
}

/// Best-effort (input, output) USD per million tokens. Unknown models → (0, 0).
/// Lets us compute cost from token counts even when a gateway reports $0 (G24).
pub fn model_cost_per_mtok(model_id: &str) -> (f64, f64) {
    let m = model_id.to_lowercase();
    let has = |n: &str| m.contains(n);
    if has("gpt-4o-mini") {
        (0.15, 0.60)
    } else if has("gpt-4o") {
        (2.50, 10.0)
    } else if has("gpt-4.1-mini") {
        (0.40, 1.60)
    } else if has("gpt-4.1") {
        (2.0, 8.0)
    } else if has("o3-mini") || has("o4-mini") {
        (1.10, 4.40)
    } else if has("haiku") {
        (0.80, 4.0)
    } else if has("sonnet") {
        (3.0, 15.0)
    } else if has("opus") {
        (15.0, 75.0)
    } else {
        (0.0, 0.0)
    }
}

pub fn resolve_model(spec: &str) -> ModelSpec {
    let rest = spec.split_once(':').map(|(_, r)| r).unwrap_or(spec);
    let (model_id, base_from_at) = match rest.split_once('@') {
        Some((m, b)) => (m.to_string(), Some(b.to_string())),
        None => (rest.to_string(), None),
    };
    let base_url = base_from_at
        .or_else(|| std::env::var("GITAGENT_MODEL_BASE_URL").ok())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .unwrap_or_default();
    ModelSpec { model_id, base_url, api_key }
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallWire>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct ToolCallWire {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: FunctionWire,
}

#[derive(Serialize)]
struct FunctionWire {
    name: String,
    arguments: String,
}

/// convert_to_llm: AgentMessage[] → OpenAI chat messages.
fn to_chat_messages(system: &str, messages: &[AgentMessage]) -> Vec<ChatMessage> {
    let mut out = vec![ChatMessage {
        role: "system".into(),
        content: Some(system.to_string()),
        tool_calls: None,
        tool_call_id: None,
    }];
    for m in messages {
        match m {
            AgentMessage::User(s) => out.push(ChatMessage {
                role: "user".into(),
                content: Some(s.clone()),
                tool_calls: None,
                tool_call_id: None,
            }),
            AgentMessage::Assistant(a) => {
                let text = a.text();
                let tcs: Vec<ToolCallWire> = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall { id, name, arguments } => Some(ToolCallWire {
                            id: id.clone(),
                            kind: "function".into(),
                            function: FunctionWire { name: name.clone(), arguments: arguments.to_string() },
                        }),
                        _ => None,
                    })
                    .collect();
                out.push(ChatMessage {
                    role: "assistant".into(),
                    content: (!text.is_empty()).then_some(text),
                    tool_calls: (!tcs.is_empty()).then_some(tcs),
                    tool_call_id: None,
                });
            }
            AgentMessage::ToolResult(t) => out.push(ChatMessage {
                role: "tool".into(),
                content: Some(t.content.clone()),
                tool_calls: None,
                tool_call_id: Some(t.tool_call_id.clone()),
            }),
        }
    }
    out
}

/// Whether a provider error looks transient (worth retrying / falling back).
pub fn is_transient_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    [
        "429", "500", "502", "503", "504", "overloaded", "rate limit", "timeout", "timed out",
        "connection", "connect", "reset", "temporarily", "unavailable", "dns", "broken pipe",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

/// `stream_assistant` with provider fallback + bounded retry/backoff. Tries each
/// spec in order; within a spec, retries transient failures with exponential
/// backoff. Returns the first non-error result, or the last error. Transient
/// errors almost always occur at request time (before any delta is streamed), so
/// a retry does not duplicate visible output.
#[allow(clippy::too_many_arguments)]
pub async fn stream_assistant_resilient(
    client: &reqwest::Client,
    specs: &[ModelSpec],
    system: &str,
    messages: &[AgentMessage],
    tools: &[Arc<dyn AgentTool>],
    params: &GenParams,
    max_retries: u32,
    cancel: &CancellationToken,
    tx: &Sender<AgentEvent>,
) -> AssistantMessage {
    let mut last = AssistantMessage::failure(StopReason::Error, "no model configured");
    for spec in specs {
        let mut attempt = 0u32;
        loop {
            if cancel.is_cancelled() {
                return AssistantMessage::failure(StopReason::Aborted, "aborted");
            }
            let msg = stream_assistant(client, spec, system, messages, tools, params, cancel, tx).await;
            if msg.stop_reason != StopReason::Error {
                return msg; // success / tool-use / stop / aborted
            }
            let transient = msg.error_message.as_deref().map(is_transient_error).unwrap_or(false);
            last = msg;
            if transient && attempt < max_retries {
                let backoff = Duration::from_millis(400u64 * (1u64 << attempt.min(5)));
                tokio::select! {
                    _ = cancel.cancelled() => return AssistantMessage::failure(StopReason::Aborted, "aborted"),
                    _ = tokio::time::sleep(backoff) => {}
                }
                attempt += 1;
                continue; // retry the same spec
            }
            break; // non-transient or retries exhausted → next fallback spec
        }
    }
    last
}

#[allow(clippy::too_many_arguments)]
pub async fn stream_assistant(
    client: &reqwest::Client,
    spec: &ModelSpec,
    system: &str,
    messages: &[AgentMessage],
    tools: &[Arc<dyn AgentTool>],
    params: &GenParams,
    cancel: &CancellationToken,
    tx: &Sender<AgentEvent>,
) -> AssistantMessage {
    let chat = to_chat_messages(system, messages);
    let tool_schemas: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({ "type": "function", "function": {
                "name": t.name(), "description": t.description(), "parameters": t.parameters()
            }})
        })
        .collect();

    match stream_inner(client, spec, &chat, &tool_schemas, params, cancel, tx).await {
        Ok(msg) => msg,
        Err(e) => AssistantMessage::failure(StopReason::Error, e.to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_inner(
    client: &reqwest::Client,
    spec: &ModelSpec,
    chat: &[ChatMessage],
    tools: &[Value],
    params: &GenParams,
    cancel: &CancellationToken,
    tx: &Sender<AgentEvent>,
) -> Result<AssistantMessage> {
    let mut body = json!({ "model": spec.model_id, "messages": chat, "stream": true });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
    }
    if let Some(t) = params.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(mt) = params.max_tokens {
        body["max_tokens"] = json!(mt);
    }
    if let Some(tp) = params.top_p {
        body["top_p"] = json!(tp);
    }

    let url = format!("{}/chat/completions", spec.base_url.trim_end_matches('/'));
    let resp = client.post(&url).bearer_auth(&spec.api_key).json(&body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("provider error {status}: {text}"));
    }

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut text = String::new();
    let mut thinking = String::new();
    let mut acc: Vec<(String, String, String)> = Vec::new(); // (id, name, args)
    let mut usage = Usage::default();
    let mut had_tool_calls = false;
    let mut finish_reason: Option<String> = None;

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                return Ok(AssistantMessage::failure(StopReason::Aborted, "aborted"));
            }
            c = stream.next() => match c {
                Some(Ok(bytes)) => bytes,
                Some(Err(e)) => return Err(e.into()),
                None => break,
            }
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            let Ok(json) = serde_json::from_str::<Value>(data) else { continue };

            if let Some(u) = json.get("usage").filter(|u| !u.is_null()) {
                usage.input = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(usage.input);
                usage.output = u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(usage.output);
                usage.total = u.get("total_tokens").and_then(Value::as_u64).unwrap_or(usage.total);
            }

            if let Some(fr) = json.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
                finish_reason = Some(fr.to_string());
            }

            let Some(delta) = json.pointer("/choices/0/delta") else { continue };
            // Reasoning models stream their chain-of-thought separately
            // (`reasoning_content` on DeepSeek/vLLM, `reasoning` on some gateways).
            if let Some(r) = delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .or_else(|| delta.get("reasoning").and_then(Value::as_str))
            {
                if !r.is_empty() {
                    thinking.push_str(r);
                    let _ = tx.send(AgentEvent::MessageDelta { kind: DeltaKind::Thinking, text: r.to_string() }).await;
                }
            }
            if let Some(c) = delta.get("content").and_then(Value::as_str) {
                if !c.is_empty() {
                    text.push_str(c);
                    let _ = tx.send(AgentEvent::MessageDelta { kind: DeltaKind::Text, text: c.to_string() }).await;
                }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
                had_tool_calls = true;
                for tc in tcs {
                    let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    while acc.len() <= idx {
                        acc.push((String::new(), String::new(), String::new()));
                    }
                    let e = &mut acc[idx];
                    if let Some(id) = tc.get("id").and_then(Value::as_str) {
                        e.0 = id.to_string();
                    }
                    if let Some(f) = tc.get("function") {
                        if let Some(n) = f.get("name").and_then(Value::as_str) {
                            e.1 = n.to_string();
                        }
                        if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                            e.2.push_str(a);
                        }
                    }
                }
            }
        }
    }

    let mut content: Vec<ContentBlock> = Vec::new();
    // Thinking precedes the answer, mirroring how it was produced.
    if !thinking.is_empty() {
        content.push(ContentBlock::Thinking(thinking));
    }
    if !text.is_empty() {
        content.push(ContentBlock::Text(text));
    }
    for (id, name, args) in acc.into_iter().filter(|(_, n, _)| !n.is_empty()) {
        let arguments: Value = serde_json::from_str(&args).unwrap_or_else(|_| json!({}));
        content.push(ContentBlock::ToolCall {
            id: if id.is_empty() { format!("call_{name}") } else { id },
            name,
            arguments,
        });
    }

    // Derive cost from token counts (works even when the gateway reports none).
    if usage.cost_usd == 0.0 {
        let (ci, co) = model_cost_per_mtok(&spec.model_id);
        usage.cost_usd = (usage.input as f64 / 1e6) * ci + (usage.output as f64 / 1e6) * co;
    }

    let stop_reason = if had_tool_calls {
        StopReason::ToolUse
    } else if finish_reason.as_deref() == Some("length") {
        StopReason::Length // response was truncated by max_tokens
    } else {
        StopReason::Stop
    };
    Ok(AssistantMessage { content, stop_reason, error_message: None, usage })
}
