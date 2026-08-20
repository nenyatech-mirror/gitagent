//! Context-window compaction — wired into the loop via the `transformContext`
//! seam pi-agent-core exposes and the TS gitagent never wires up (its compact.ts
//! is exported but has zero in-loop callers).
//!
//! Two-stage strategy, matching current best practice (Anthropic context editing
//! `compact` + `clear_tool_uses`):
//!   1. SUMMARIZE the oldest turns with the model, keeping recent turns verbatim
//!      (recency), and continue from the summary. A running summary means only
//!      newly-aged messages are summarized each time — cache-friendly.
//!   2. Fallback: TRUNCATE old tool-result / assistant-text content in place.
//!
//! Invariant: never DROP a message — that can orphan an assistant `tool_use`
//! from its `tool_result` (→ provider 400). We only shrink or fold-into-summary,
//! and the summary replaces a contiguous head so no pair is split.

use crate::pi::message::{AgentMessage, ContentBlock};
use crate::pi::provider::{stream_assistant, GenParams, ModelSpec};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct RunningSummary {
    /// Number of leading messages already folded into `text`.
    folded: usize,
    text: String,
}

pub struct Compactor {
    /// Token budget for the transcript sent to the model.
    pub max_tokens: usize,
    /// Target size of the recent tail kept verbatim.
    recent_tokens: usize,
    /// Whether to LLM-summarize the head (else truncation-only).
    summarize: bool,
    state: Arc<Mutex<RunningSummary>>,
}

impl Compactor {
    pub fn new(context_window: usize) -> Self {
        Self {
            max_tokens: (context_window as f64 * 0.75) as usize,
            recent_tokens: (context_window as f64 * 0.4) as usize,
            summarize: true,
            state: Arc::new(Mutex::new(RunningSummary::default())),
        }
    }

    /// Disable LLM summarization (truncation-only) — for deterministic tests.
    pub fn without_summarize(mut self) -> Self {
        self.summarize = false;
        self
    }

    fn msg_chars(m: &AgentMessage) -> usize {
        match m {
            AgentMessage::User(s) => s.len(),
            AgentMessage::ToolResult(t) => t.content.len() + 32,
            AgentMessage::Assistant(a) => a
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text(t) | ContentBlock::Thinking(t) => t.len(),
                    ContentBlock::ToolCall { arguments, .. } => arguments.to_string().len() + 48,
                })
                .sum(),
        }
    }

    /// ~3.5 chars/token — closer than 4 for code/JSON-heavy transcripts.
    pub fn estimate_tokens(messages: &[AgentMessage]) -> usize {
        (messages.iter().map(Self::msg_chars).sum::<usize>() as f64 / 3.5).ceil() as usize
    }

    fn truncate_middle(s: &str, keep: usize) -> String {
        let n = s.chars().count();
        if n <= keep * 2 + 40 {
            return s.to_string();
        }
        let head: String = s.chars().take(keep).collect();
        let tail: String = s.chars().rev().take(keep).collect::<Vec<_>>().into_iter().rev().collect();
        format!("{head}\n… [{} chars omitted to fit the context window] …\n{tail}", n - keep * 2)
    }

    /// Synchronous, truncation-only compaction — the safe fallback (and what the
    /// loop uses when no summarizer model is available).
    pub fn apply(&self, messages: &[AgentMessage]) -> Vec<AgentMessage> {
        let mut out = messages.to_vec();
        if Self::estimate_tokens(&out) <= self.max_tokens {
            return out;
        }
        // Pass 1: truncate tool-result content, oldest-first, until under budget.
        for i in 0..out.len() {
            if Self::estimate_tokens(&out) <= self.max_tokens {
                break;
            }
            if let AgentMessage::ToolResult(t) = &mut out[i] {
                if t.content.chars().count() > 840 {
                    t.content = Self::truncate_middle(&t.content, 400);
                }
            }
        }
        // Pass 2: if still over, compress assistant text too.
        for i in 0..out.len() {
            if Self::estimate_tokens(&out) <= self.max_tokens {
                break;
            }
            if let AgentMessage::Assistant(a) = &mut out[i] {
                for b in a.content.iter_mut() {
                    if let ContentBlock::Text(txt) = b {
                        if txt.chars().count() > 640 {
                            *txt = Self::truncate_middle(txt, 300);
                        }
                    }
                }
            }
        }
        out
    }

    /// Full compaction: summarize the aged head, keep the recent tail verbatim,
    /// then truncation-clamp if still over. Falls back to `apply` when there is
    /// no summarizer model. Returns messages that fit the budget.
    pub async fn compact(
        &self,
        client: &reqwest::Client,
        spec: Option<&ModelSpec>,
        messages: &[AgentMessage],
        cancel: &CancellationToken,
    ) -> Vec<AgentMessage> {
        if Self::estimate_tokens(messages) <= self.max_tokens {
            return messages.to_vec(); // under budget — untouched, cache-friendly
        }
        let Some(spec) = spec.filter(|_| self.summarize) else {
            return self.apply(messages);
        };

        // Choose the recent tail (verbatim), walking from the end by token budget.
        let mut tail_start = messages.len();
        let mut acc = 0usize;
        while tail_start > 0 {
            let cost = (Self::msg_chars(&messages[tail_start - 1]) as f64 / 3.5).ceil() as usize;
            if acc + cost > self.recent_tokens {
                break;
            }
            acc += cost;
            tail_start -= 1;
        }
        // Never split a tool_use/tool_result pair: don't let the tail begin on a
        // ToolResult (its tool_use would be stranded in the head).
        while tail_start < messages.len() && matches!(messages[tail_start], AgentMessage::ToolResult(_)) {
            tail_start += 1;
        }
        tail_start = tail_start.min(messages.len().saturating_sub(1));
        if tail_start == 0 {
            return self.apply(messages); // nothing to summarize → truncate
        }

        // Fold the newly-aged head messages into a running summary (only the part
        // not yet summarized — keeps summarization incremental / cheap).
        let head = &messages[..tail_start];
        let (prior, folded) = {
            let s = self.state.lock().unwrap();
            (s.text.clone(), s.folded.min(head.len()))
        };
        let new_slice = &head[folded..];
        let summary = if new_slice.is_empty() && !prior.is_empty() {
            prior
        } else {
            let text = self.summarize_head(client, spec, &prior, new_slice, cancel).await;
            let mut s = self.state.lock().unwrap();
            s.text = text.clone();
            s.folded = head.len();
            text
        };

        let mut out = Vec::with_capacity(messages.len() - tail_start + 1);
        out.push(AgentMessage::User(format!(
            "[Summary of the earlier conversation, compacted to fit the context window]\n{summary}"
        )));
        out.extend_from_slice(&messages[tail_start..]);

        // Still over (e.g. a huge recent tool result)? Truncation-clamp it.
        if Self::estimate_tokens(&out) > self.max_tokens {
            out = self.apply(&out);
        }
        out
    }

    async fn summarize_head(
        &self,
        client: &reqwest::Client,
        spec: &ModelSpec,
        prior: &str,
        new_slice: &[AgentMessage],
        cancel: &CancellationToken,
    ) -> String {
        let mut convo = String::new();
        for m in new_slice {
            match m {
                AgentMessage::User(s) => convo.push_str(&format!("User: {}\n", s)),
                AgentMessage::Assistant(a) => {
                    let t = a.text();
                    if !t.is_empty() {
                        convo.push_str(&format!("Assistant: {t}\n"));
                    }
                    for (_, name, args) in a.tool_calls() {
                        convo.push_str(&format!("Tool call: {name}({})\n", Self::truncate_middle(&args.to_string(), 100)));
                    }
                }
                AgentMessage::ToolResult(t) => {
                    convo.push_str(&format!("Tool result [{}]: {}\n", t.tool_name, Self::truncate_middle(&t.content, 250)));
                }
            }
        }
        let convo = Self::truncate_middle(&convo, 4000);
        let existing = if prior.is_empty() { String::new() } else { format!("EXISTING SUMMARY:\n{prior}\n\n") };
        let prompt = format!(
            "Update the running summary with the new exchange below. Preserve key \
             decisions, file paths, code changes, commands run, errors, and outcomes; \
             omit routine tool-call detail unless it failed. Output ONLY the updated summary.\n\n\
             {existing}NEW EXCHANGE:\n{convo}"
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let msg = stream_assistant(
            client,
            spec,
            "You are a precise conversation summarizer for an autonomous agent.",
            &[AgentMessage::User(prompt)],
            &[],
            &GenParams { temperature: Some(0.0), max_tokens: Some(2048), top_p: None },
            cancel,
            &tx,
        )
        .await;
        drop(tx);
        let _ = drain.await;

        let text = msg.text();
        if text.trim().is_empty() {
            // Summarizer failed — keep the prior summary rather than losing history.
            if prior.is_empty() { "(summary unavailable)".to_string() } else { prior.to_string() }
        } else {
            text
        }
    }
}
