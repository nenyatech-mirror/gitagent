//! Terminal front-end for the gitagent SDK.
//!
//! This module is a *thin presentation layer*: it contains no agent logic and
//! touches only the public SDK surface (`gitagent::sdk::query` / `::goal`). It
//! renders the SDK's `Event` stream — one-shot, interactive, or goal mode.

use futures_util::{Stream, StreamExt};
use gitagent::sdk::goal::{run_goal, Goal};
use gitagent::sdk::query::{open_session, query, Event, QueryOptions};
use gitagent::sdk::session::RepoOptions;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};

// ── styling ───────────────────────────────────────────────────────────────

/// Colors are on only for an interactive terminal without `NO_COLOR` — so piped
/// output stays clean and greppable.
struct Style {
    on: bool,
}

impl Style {
    fn new() -> Self {
        Self { on: std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() }
    }
    fn paint(&self, code: &str, s: &str) -> String {
        if self.on { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn dim(&self, s: &str) -> String { self.paint("2", s) }
    fn cyan(&self, s: &str) -> String { self.paint("36", s) }
    fn green(&self, s: &str) -> String { self.paint("32", s) }
    fn yellow(&self, s: &str) -> String { self.paint("33", s) }
    fn red(&self, s: &str) -> String { self.paint("31", s) }
}

fn preview(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        format!("{}…", one_line.chars().take(max).collect::<String>())
    }
}

// ── shared event renderer ───────────────────────────────────────────────────

#[derive(PartialEq)]
enum Mode {
    Idle,
    Answer,
    Thinking,
}

/// Render an Event stream to the terminal (streaming tokens). Returns the exit
/// code for the turn. Generic so one-shot `QueryStream` and interactive
/// `TurnStream` share it.
async fn render_events<S>(stream: &mut S, st: &Style) -> i32
where
    S: Stream<Item = Event> + Unpin,
{
    let out = std::io::stdout();
    let mut exit = 0;
    let mut mode = Mode::Idle;
    let mut col0 = true; // are we at the start of a line?
    let br = |col0: bool| {
        if !col0 {
            println!();
        }
    };

    while let Some(ev) = stream.next().await {
        match ev {
            Event::Thinking(t) => {
                if mode != Mode::Thinking {
                    br(col0);
                    print!("{} ", st.dim("💭"));
                    mode = Mode::Thinking;
                }
                print!("{}", st.dim(&t));
                let _ = out.lock().flush();
                col0 = t.ends_with('\n');
            }
            Event::Delta(t) => {
                if mode == Mode::Thinking {
                    br(col0);
                }
                mode = Mode::Answer;
                print!("{t}");
                let _ = out.lock().flush();
                col0 = t.ends_with('\n');
            }
            Event::ToolCall { name, args } => {
                br(col0);
                println!("{}", st.cyan(&format!("⚙ {name}({})", preview(&args.to_string(), 72))));
                mode = Mode::Idle;
                col0 = true;
            }
            Event::ToolResult { name, content, is_error } => {
                let mark = if is_error { st.red("✗") } else { st.dim("→") };
                println!("{mark} {}", st.dim(&format!("{name}: {}", preview(&content, 100))));
                mode = Mode::Idle;
                col0 = true;
            }
            Event::Done => {
                br(col0);
                col0 = true;
            }
            Event::Error(e) => {
                br(col0);
                eprintln!("{}", st.red(&format!("error: {e}")));
                exit = 1;
                col0 = true;
            }
        }
    }
    exit
}

fn usage_line(st: &Style, u: gitagent::sdk::query::RunUsage, prefix: &str) {
    if u.input_tokens + u.output_tokens > 0 {
        eprintln!(
            "{}",
            st.dim(&format!("{prefix}{} in · {} out · ${:.4}", u.input_tokens, u.output_tokens, u.cost_usd))
        );
    }
}

// ── one-shot streaming mode ─────────────────────────────────────────────────

/// Run a single prompt, streaming tokens. Returns the exit code.
pub async fn run_stream(opts: QueryOptions) -> i32 {
    let st = Style::new();
    let mut stream = match query(opts) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", st.red(&format!("error: {e}")));
            return 1;
        }
    };
    let exit = render_events(&mut stream, &st).await;
    usage_line(&st, stream.usage(), "");
    exit
}

// ── interactive (REPL) mode ─────────────────────────────────────────────────

/// Multi-turn REPL: conversation context persists across messages. Ctrl-D or
/// `/exit` quits.
pub async fn run_interactive(
    dir: PathBuf,
    model: Option<String>,
    repo: Option<RepoOptions>,
    permission_mode: Option<String>,
) -> i32 {
    let st = Style::new();
    let session = match open_session(dir, model, repo, permission_mode) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", st.red(&format!("error: {e}")));
            return 1;
        }
    };

    eprintln!("{}", st.dim("gitagent — interactive. Type a message; Ctrl-D or /exit to quit."));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut exit = 0;

    loop {
        eprint!("{}", st.cyan("› "));
        let _ = std::io::stderr().flush();

        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            _ => break, // EOF (Ctrl-D) or read error
        };
        let msg = line.trim();
        if msg.is_empty() {
            continue;
        }
        if matches!(msg, "/exit" | "/quit" | ":q") {
            break;
        }

        let mut turn = session.send(msg.to_string());
        let code = render_events(&mut turn, &st).await;
        if code != 0 {
            exit = code;
        }
        usage_line(&st, turn.usage(), "· ");
    }

    eprintln!("{}", st.dim("bye."));
    exit
}

// ── goal (test-driven) mode ─────────────────────────────────────────────────

/// Run the self-correcting goal loop, rendering each attempt. Returns exit code.
pub async fn run_goal_cli(goal: Goal, dir: PathBuf) -> i32 {
    let st = Style::new();
    eprintln!("{}", st.dim(&format!("goal: {}", preview(&goal.prompt, 80))));
    eprintln!("{}", st.dim(&format!("verify: {}", goal.verify.clone().unwrap_or_default())));

    match run_goal(&dir, goal).await {
        Ok(o) => {
            for a in &o.trajectory {
                let mark = if a.passed { st.green("✓") } else { st.yellow("↻") };
                eprintln!("{}", st.dim(&format!("attempt {} {mark} · {} tool step(s)", a.n, a.steps.len())));
            }
            if o.passed {
                println!("{}", st.green(&format!("✓ verified after {} attempt(s)", o.attempts)));
                0
            } else {
                eprintln!("{}", st.red(&format!("✗ not verified after {} attempt(s)", o.attempts)));
                1
            }
        }
        Err(e) => {
            eprintln!("{}", st.red(&format!("error: {e}")));
            1
        }
    }
}
