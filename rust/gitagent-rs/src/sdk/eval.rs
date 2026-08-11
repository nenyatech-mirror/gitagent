//! Eval harness — measures agent accuracy on a fixed task set. Each task is a
//! prompt + a verify command; `run_eval` runs the goal loop per task in an
//! isolated dir and reports the pass rate. Run the same tasks single-shot
//! (max_attempts=1, no reflection) vs. self-correcting to prove the loop's lift.

use crate::sdk::goal::{run_goal, Goal};
use std::path::Path;

#[derive(Clone)]
pub struct EvalTask {
    pub name: String,
    pub prompt: String,
    /// Shell command; exit 0 = pass.
    pub verify: String,
    /// Optional shell command to seed files before the attempt (e.g. a bug).
    pub setup: Option<String>,
}

pub struct TaskResult {
    pub name: String,
    pub passed: bool,
    pub attempts: u32,
}

pub struct EvalReport {
    pub results: Vec<TaskResult>,
}

impl EvalReport {
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.passed).count()
    }
    pub fn total(&self) -> usize {
        self.results.len()
    }
    pub fn pass_rate(&self) -> f64 {
        if self.results.is_empty() {
            0.0
        } else {
            self.passed() as f64 / self.total() as f64
        }
    }
}

/// Run every task in its own isolated directory under `base`.
pub async fn run_eval(
    base: &Path,
    tasks: &[EvalTask],
    model: Option<String>,
    max_attempts: u32,
    reflect: bool,
) -> anyhow::Result<EvalReport> {
    let mut results = Vec::new();
    let model_line = model.clone().unwrap_or_else(|| "openai:gpt-4o-mini".to_string());

    for task in tasks {
        let dir = base.join(&task.name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("agent.yaml"),
            // temperature 0 → first-attempt failures are reproducible, so any
            // recovery is attributable to the loop, not a lucky re-roll.
            // max_tokens set so providers that require it (Anthropic) accept it.
            format!("name: eval\nmodel:\n  preferred: \"{model_line}\"\n  constraints:\n    max_tokens: 4096\n    temperature: 0\n"),
        )?;
        if let Some(setup) = &task.setup {
            let _ = tokio::process::Command::new("sh").arg("-c").arg(setup).current_dir(&dir).output().await;
        }

        let outcome = run_goal(
            &dir,
            Goal {
                prompt: task.prompt.clone(),
                verify: Some(task.verify.clone()),
                max_attempts,
                model: model.clone(),
                reflect,
            },
        )
        .await?;

        results.push(TaskResult { name: task.name.clone(), passed: outcome.passed, attempts: outcome.attempts });
    }

    Ok(EvalReport { results })
}
