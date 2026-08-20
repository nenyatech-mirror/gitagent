//! Ira desktop — a Tauri shell around the embedded Ira UI server.
//!
//! Starts the same server `ira --ui` uses (in-process, same single binary) and
//! opens a native window on it. Agent dir resolution: CLI arg → IRA_AGENT_DIR →
//! ~/.ira/agent (auto-scaffolded), so double-clicking the app Just Works.

#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::path::PathBuf;

fn resolve_agent_dir() -> PathBuf {
    // 1) explicit arg, 2) env, 3) cwd if it's an agent, 4) ~/.ira/agent (scaffold).
    if let Some(arg) = std::env::args().nth(1) {
        return PathBuf::from(arg);
    }
    if let Ok(env_dir) = std::env::var("IRA_AGENT_DIR") {
        return PathBuf::from(env_dir);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("agent.yaml").exists() {
        return cwd;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".ira").join("agent")
}

fn scaffold_if_missing(dir: &PathBuf) {
    if dir.join("agent.yaml").exists() {
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(
        dir.join("agent.yaml"),
        "name: Ira\ndescription: Local assistant.\n\nmodel:\n  preferred: \"ollama:gemma4:e2b-it-qat@http://localhost:11434/v1\"\n  fallback: []\n  constraints:\n    max_tokens: 8192\n\ntools: [cli, read, write, edit, memory, pdf]\n\nruntime:\n  max_turns: 20\n",
    );
    let _ = std::fs::create_dir_all(dir.join("memory"));
    let _ = std::fs::write(dir.join("memory").join("MEMORY.md"), "# Memory\n");
}

fn main() {
    // Desktop branding (the CLI/browser UI keeps its default).
    std::env::set_var("IRA_BRAND", "Lyzr Edgespace");

    let dir = resolve_agent_dir();
    scaffold_if_missing(&dir);
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    ira::sdk::env::load_env(&dir);

    // The server needs a live Tokio runtime for the app's whole lifetime.
    let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().expect("tokio runtime")));
    let url = rt
        .block_on(ira::ui::start_server(dir, None, None, false))
        .expect("start Ira server");

    tauri::Builder::default()
        .setup(move |app| {
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(url.parse().expect("server url")),
            )
            .title("Lyzr Edgespace")
            .inner_size(1240.0, 860.0)
            .min_inner_size(720.0, 480.0)
            .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run Ira desktop");
}
