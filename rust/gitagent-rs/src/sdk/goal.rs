//! Trajectory-driven, self-correcting, test-driven goal loop.
//!
//! The agent attempts a goal, a verification command decides success, and on
//! failure the loop feeds the trajectory + test output back as a correction and
//! retries (Reflexion). Every attempt's trajectory is persisted under
//! `.gitagent/trajectories/`, and failure lessons accumulate in
//! `.gitagent/lessons.md` — so the agent learns within a run and across runs
//! (past lessons are injected into the very first attempt of later runs).

use crate::pi::message::AgentMessage;
use crate::pi::provider::{resolve_model, stream_assistant, GenParams};
use crate::sdk::query::{query, Event, QueryOptions};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

pub struct Goal {
    pub prompt: String,
    /// Shell command deciding success (exit 0 = pass). None = single attempt.
    pub verify: Option<String>,
    pub max_attempts: u32,
    pub model: Option<String>,
    /// Use an explicit LLM reflection step to diagnose failures (raises accuracy
    /// on small models). When false, the raw verify output is the correction.
    pub reflect: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalStep {
    pub tool: String,
    pub args: Value,
    pub result: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Attempt {
    pub n: u32,
    pub steps: Vec<GoalStep>,
    pub final_text: String,
    pub verify_output: String,
    pub passed: bool,
}

#[derive(Debug, Clone)]
pub struct GoalOutcome {
    pub passed: bool,
    pub attempts: u32,
    pub trajectory: Vec<Attempt>,
    pub lessons: Vec<String>,
}

fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    s.chars().skip(n - max).collect()
}

/// Build the attempt prompt: the goal, plus accumulated corrections on retries.
fn build_prompt(goal: &str, lessons: &[String]) -> String {
    if lessons.is_empty() {
        return goal.to_string();
    }
    let mut s = String::from(goal);
    s.push_str("\n\n## Previous attempts failed verification — correct your approach\n");
    for (i, l) in lessons.iter().enumerate() {
        s.push_str(&format!("### Correction {}\n{}\n\n", i + 1, l));
    }
    s.push_str("Diagnose why the checks failed above, then fix it this time.");
    s
}

/// Load lessons persisted from earlier runs (cross-run learning).
fn prior_lessons(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join(".gitagent").join("lessons.md"))
        .ok()
        .map(|s| s.split("\n---\n").map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default()
}

fn append_lesson(dir: &Path, lesson: &str) {
    let path = dir.join(".gitagent").join("lessons.md");
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
    existing.push_str(lesson);
    existing.push_str("\n---\n");
    let _ = std::fs::write(&path, existing);
}

fn persist_trajectory(dir: &Path, attempt: &Attempt) {
    let traj_dir = dir.join(".gitagent").join("trajectories");
    let _ = std::fs::create_dir_all(&traj_dir);
    if let Ok(json) = serde_json::to_string_pretty(attempt) {
        let _ = std::fs::write(traj_dir.join(format!("attempt-{}.json", attempt.n)), json);
    }
}

/// A filesystem-safe slug from a goal description.
fn slug(goal: &str) -> String {
    let s: String = goal
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s: String = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    s.chars().take(48).collect::<String>().trim_matches('-').to_string()
}

/// Learn from a WINNING trajectory: write (or reinforce, +confidence) a skill
/// that captures the approach that verified. This is the self-learning half —
/// success feeds forward, not just failure.
fn learn_from_success(dir: &Path, goal: &str, attempt: &Attempt) {
    let name = slug(goal);
    if name.is_empty() {
        return;
    }
    let path = dir.join("skills").join(&name).join("SKILL.md");
    // Bump confidence if we've solved this before, else start at 0.6.
    let confidence = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("confidence:").map(|v| v.trim().parse::<f64>().unwrap_or(0.6)))
        })
        .map(|c| (c + 0.1).min(1.0))
        .unwrap_or(0.6);

    let mut body = format!("Verified approach for: {goal}\n\nWinning steps:\n");
    for s in &attempt.steps {
        body.push_str(&format!("- {}({}) → {}\n", s.tool, s.args, tail(&s.result, 120)));
    }
    if !attempt.final_text.trim().is_empty() {
        body.push_str(&format!("\nOutcome: {}\n", tail(attempt.final_text.trim(), 300)));
    }
    let out = format!("---\nname: {name}\ndescription: {goal}\nconfidence: {confidence}\n---\n{body}");
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::write(&path, out);
}

/// Run one attempt, collecting the agent's trajectory (tool calls + final text).
async fn run_attempt(dir: &Path, model: Option<String>, prompt: String) -> anyhow::Result<(Vec<GoalStep>, String)> {
    let mut stream = query(QueryOptions {
        dir: dir.to_path_buf(),
        model,
        prompt,
        repo: None,
        permission_mode: None,
    })?;

    let mut steps: Vec<GoalStep> = Vec::new();
    let mut final_text = String::new();
    let mut pending: Vec<(String, Value)> = Vec::new(); // tool calls awaiting results
    while let Some(ev) = stream.next().await {
        match ev {
            Event::Delta(t) => final_text.push_str(&t),
            Event::ToolCall { name, args } => pending.push((name, args)),
            Event::ToolResult { name, content, is_error } => {
                // Pair with the earliest pending call of the same tool.
                let args = pending
                    .iter()
                    .position(|(n, _)| n == &name)
                    .map(|i| pending.remove(i).1)
                    .unwrap_or(Value::Null);
                steps.push(GoalStep { tool: name, args, result: content, is_error });
            }
            _ => {}
        }
    }
    Ok((steps, final_text))
}

/// Reflexion: ask the model to diagnose its own failed trajectory and state a
/// concrete correction. Returns the correction text (falls back to raw output).
async fn reflect(model: &Option<String>, goal: &str, attempt: &Attempt) -> String {
    let spec = resolve_model(model.as_deref().unwrap_or("openai:gpt-4o-mini"));
    let steps = if attempt.steps.is_empty() {
        "(used no tools)".to_string()
    } else {
        attempt
            .steps
            .iter()
            .map(|s| format!("- {}({}) → {}{}", s.tool, s.args, if s.is_error { "ERROR " } else { "" }, tail(&s.result, 150)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let prompt = format!(
        "Your attempt to achieve a goal FAILED its verification check.\n\n\
         GOAL: {goal}\n\n\
         WHAT YOU DID:\n{steps}\n\n\
         YOUR FINAL MESSAGE: {}\n\n\
         VERIFICATION OUTPUT (this is why it failed):\n{}\n\n\
         In 2-3 sentences: diagnose the ROOT CAUSE and state exactly what you will do \
         differently next attempt. Be specific and actionable.",
        tail(attempt.final_text.trim(), 300),
        tail(&attempt.verify_output, 800),
    );

    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let msg = stream_assistant(
        &client,
        &spec,
        "You are a precise debugging assistant. Diagnose failures crisply.",
        &[AgentMessage::User(prompt)],
        &[],
        // temperature 0 for a deterministic diagnosis; max_tokens for Anthropic.
        &GenParams { temperature: Some(0.0), max_tokens: Some(1024), top_p: None },
        &cancel,
        &tx,
    )
    .await;
    drop(tx);
    let _ = drain.await;

    let text = msg.text();
    if text.trim().is_empty() {
        format!("Verification failed:\n{}", tail(&attempt.verify_output, 400))
    } else {
        text
    }
}

/// Run the verification command; returns (passed, combined output).
async fn run_verify(cmd: &str, dir: &Path) -> (bool, String) {
    match tokio::process::Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output().await {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            if !o.status.success() {
                s.push_str(&format!("\n[exit code {}]", o.status.code().unwrap_or(-1)));
            }
            (o.status.success(), s)
        }
        Err(e) => (false, format!("verify command failed to run: {e}")),
    }
}

/// The self-correcting, test-driven goal loop.
pub async fn run_goal(dir: &Path, goal: Goal) -> anyhow::Result<GoalOutcome> {
    let dir = PathBuf::from(dir);
    // Seed with lessons learned in earlier runs (cross-run self-learning).
    let mut lessons = prior_lessons(&dir);
    let mut trajectory: Vec<Attempt> = Vec::new();

    for n in 1..=goal.max_attempts.max(1) {
        let prompt = build_prompt(&goal.prompt, &lessons);
        let (steps, final_text) = run_attempt(&dir, goal.model.clone(), prompt).await?;

        let (passed, verify_output) = match &goal.verify {
            Some(cmd) => run_verify(cmd, &dir).await,
            None => (true, String::new()),
        };

        let attempt = Attempt { n, steps, final_text, verify_output: verify_output.clone(), passed };
        persist_trajectory(&dir, &attempt);
        trajectory.push(attempt);

        if passed {
            // Self-learning: capture the winning trajectory as a reinforced skill.
            if let Some(a) = trajectory.last() {
                learn_from_success(&dir, &goal.prompt, a);
            }
            return Ok(GoalOutcome { passed: true, attempts: n, trajectory, lessons });
        }

        // Self-correct: an LLM reflection (better on small models) or the raw
        // verify output distilled with the last action taken.
        let lesson = if goal.reflect {
            reflect(&goal.model, &goal.prompt, trajectory.last().unwrap()).await
        } else {
            let last_action = trajectory
                .last()
                .and_then(|a| a.steps.last())
                .map(|s| format!("last action: {}({}) → {}", s.tool, s.args, tail(&s.result, 200)))
                .unwrap_or_else(|| "no tools were used".into());
            format!("Attempt {n} failed the check.\nVerification output:\n{}\n{}", tail(&verify_output, 800), last_action)
        };
        append_lesson(&dir, &lesson);
        lessons.push(lesson);
    }

    Ok(GoalOutcome { passed: false, attempts: goal.max_attempts.max(1), trajectory, lessons })
}

/// Test-driven goal *move*: run a sequence of goals, advancing to the next only
/// when the current one verifies. Stops at the first goal that can't be verified
/// (a later goal usually depends on an earlier one succeeding). Returns the
/// outcome of every goal attempted.
pub async fn run_goals(dir: &Path, goals: Vec<Goal>) -> anyhow::Result<Vec<GoalOutcome>> {
    let mut outcomes = Vec::new();
    for goal in goals {
        let outcome = run_goal(dir, goal).await?;
        let passed = outcome.passed;
        outcomes.push(outcome);
        if !passed {
            break; // don't advance past an unmet goal
        }
    }
    Ok(outcomes)
}
