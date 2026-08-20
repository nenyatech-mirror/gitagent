//! Load `~/.gitagent/.env` then `<agent-dir>/.env` into the process environment
//! (agent-dir wins). Mirrors the TS CLI's env precedence so API keys +
//! GITAGENT_MODEL_BASE_URL are picked up.

use std::path::Path;

pub fn load_env(agent_dir: &Path) {
    let mut paths = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(Path::new(&home).join(".gitagent").join(".env"));
    }
    paths.push(agent_dir.join(".env"));

    for p in paths {
        let Ok(content) = std::fs::read_to_string(&p) else { continue };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            // Strip a matching pair of surrounding quotes, if present.
            let v = if v.len() >= 2
                && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
            {
                &v[1..v.len() - 1]
            } else {
                v
            };
            std::env::set_var(k.trim(), v);
        }
    }
}
