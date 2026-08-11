//! gitagent CLI — a thin front-end over the SDK. All agent behavior lives in the
//! library (`gitagent::sdk` / `::pi`); this binary only parses flags and hands
//! off to the `cli` renderer.

mod cli;

use clap::Parser;
use gitagent::sdk::env::load_env;
use gitagent::sdk::goal::Goal;
use gitagent::sdk::query::QueryOptions;
use gitagent::sdk::session::{repo_name, RepoOptions};
use std::path::PathBuf;

/// Git-native AI agent (Rust core — faithful pi-core engine).
#[derive(Parser)]
#[command(name = "gitagent", version, about)]
struct Args {
    /// Agent directory (contains agent.yaml). With --repo, the clone target.
    #[arg(short, long, default_value = ".")]
    dir: String,
    /// Override model, e.g. "openai:gpt-4o-mini@http://localhost:8090/v1".
    #[arg(short, long)]
    model: Option<String>,
    /// Clone a GitHub repo and run the agent from it on a session branch.
    #[arg(short, long)]
    repo: Option<String>,
    /// GitHub PAT (or set GITHUB_TOKEN / GIT_TOKEN).
    #[arg(long)]
    pat: Option<String>,
    /// Resume an existing session branch (e.g. gitagent/session-abc12345).
    #[arg(long)]
    session: Option<String>,
    /// Permission mode: plan | acceptEdits | bypass | default.
    #[arg(long)]
    permission_mode: Option<String>,
    /// Test-driven goal mode: a shell command that must exit 0 for success.
    /// The agent retries (self-correcting) until it passes or attempts run out.
    #[arg(long)]
    verify: Option<String>,
    /// Max self-correcting attempts in goal mode (default 3).
    #[arg(long)]
    max_attempts: Option<u32>,
    /// The prompt to run. Omit for an interactive REPL.
    prompt: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Load ~/.gitagent/.env then <dir>/.env (API keys, GITAGENT_MODEL_BASE_URL).
    load_env(&PathBuf::from(&args.dir));

    let code = match (args.verify.clone(), args.prompt.clone()) {
        // Test-driven goal loop (needs a prompt + a verify command).
        (Some(verify), Some(prompt)) => {
            cli::run_goal_cli(
                Goal {
                    prompt,
                    verify: Some(verify),
                    max_attempts: args.max_attempts.unwrap_or(3),
                    model: args.model.clone(),
                    reflect: true,
                },
                PathBuf::from(&args.dir),
            )
            .await
        }
        // One-shot streaming run.
        (None, Some(prompt)) => {
            let repo = build_repo(&args);
            cli::run_stream(QueryOptions {
                dir: PathBuf::from(&args.dir),
                model: args.model,
                prompt,
                repo,
                permission_mode: args.permission_mode,
            })
            .await
        }
        // No prompt → interactive REPL.
        (_, None) => {
            let repo = build_repo(&args);
            cli::run_interactive(PathBuf::from(&args.dir), args.model, repo, args.permission_mode).await
        }
    };

    std::process::exit(code);
}

/// Build repo options when --repo is given.
fn build_repo(args: &Args) -> Option<RepoOptions> {
    let url = args.repo.as_ref()?;
    let token = args
        .pat
        .clone()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
        .or_else(|| std::env::var("GIT_TOKEN").ok())
        .unwrap_or_default();
    // Clone into --dir if given, else ./<repo-name>.
    let clone_dir = if args.dir == "." { PathBuf::from(repo_name(url)) } else { PathBuf::from(&args.dir) };
    Some(RepoOptions { url: url.clone(), token, dir: clone_dir, session: args.session.clone() })
}
