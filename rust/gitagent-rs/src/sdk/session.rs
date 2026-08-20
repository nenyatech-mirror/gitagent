//! Local-repo session mode: clone a git repo, work on a `gitagent/session-<hash>`
//! branch, and commit/push on finalize. Faithful to the TS `initLocalSession`,
//! but every git call is argv (`Command`) — never a shell string — so the
//! load-time RCE the TS version shipped with (extends/session injection) can't
//! happen here.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct RepoOptions {
    pub url: String,
    pub token: String,
    /// Where to clone / reuse the working copy.
    pub dir: PathBuf,
    /// Resume an existing session branch instead of creating a new one.
    pub session: Option<String>,
}

pub struct LocalSession {
    pub dir: PathBuf,
    pub branch: String,
    pub session_id: String,
    clean_url: String,
}

fn git(args: &[&str], cwd: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running git {args:?}"))?;
    if !out.status.success() {
        bail!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn authed_url(url: &str, token: &str) -> String {
    url.replacen("https://", &format!("https://{token}@"), 1)
}

fn clean_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        if let Some(at) = rest.find('@') {
            return format!("https://{}", &rest[at + 1..]);
        }
    }
    url.to_string()
}

fn default_branch(dir: &Path) -> String {
    if let Ok(r) = git(&["symbolic-ref", "refs/remotes/origin/HEAD"], dir) {
        return r.replace("refs/remotes/origin/", "");
    }
    if git(&["rev-parse", "--verify", "origin/main"], dir).is_ok() {
        "main".into()
    } else {
        "master".into()
    }
}

fn short_hex() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{:08x}", (nanos as u32) ^ (std::process::id()))
}

pub fn repo_name(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("agent")
        .trim_end_matches(".git")
        .to_string()
}

pub fn init_local_session(opts: RepoOptions) -> Result<LocalSession> {
    let dir = opts.dir;
    let aurl = authed_url(&opts.url, &opts.token);

    if !dir.exists() {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // argv — the URL is never shell-interpreted.
        let out = Command::new("git")
            .args(["clone", "--depth", "1", "--no-single-branch", &aurl])
            .arg(&dir)
            .output()
            .context("git clone")?;
        if !out.status.success() {
            bail!("git clone failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    } else {
        git(&["remote", "set-url", "origin", &aurl], &dir)?;
        git(&["fetch", "origin"], &dir).ok();
        let default = default_branch(&dir);
        git(&["checkout", &default], &dir).ok();
        git(&["reset", "--hard", &format!("origin/{default}")], &dir).ok();
    }

    let (branch, session_id) = match opts.session {
        Some(s) => {
            let id = s.strip_prefix("gitagent/session-").unwrap_or(&s).to_string();
            if git(&["checkout", &s], &dir).is_err() {
                git(&["checkout", "-b", &s, &format!("origin/{s}")], &dir).ok();
            }
            git(&["pull", "origin", &s], &dir).ok();
            (s, id)
        }
        None => {
            let id = short_hex();
            let branch = format!("gitagent/session-{id}");
            git(&["checkout", "-b", &branch], &dir)?;
            (branch, id)
        }
    };

    scaffold(&dir, &opts.url);

    Ok(LocalSession {
        dir,
        branch,
        session_id,
        clean_url: clean_url(&opts.url),
    })
}

fn scaffold(dir: &Path, url: &str) {
    let agent_yaml = dir.join("agent.yaml");
    if !agent_yaml.exists() {
        let name = repo_name(url);
        let _ = std::fs::write(
            &agent_yaml,
            format!(
                "spec_version: \"0.1.0\"\nname: {name}\nversion: 0.1.0\ndescription: Gitagent agent for {name}\nmodel:\n  preferred: \"openai:gpt-4o-mini\"\n  fallback: []\ntools: [cli, read, write, memory]\nruntime:\n  max_turns: 50\n"
            ),
        );
    }
    let mem = dir.join("memory").join("MEMORY.md");
    if !mem.exists() {
        let _ = std::fs::create_dir_all(dir.join("memory"));
        let _ = std::fs::write(mem, "# Memory\n");
    }
}

impl LocalSession {
    /// Commit any changes, push the session branch, and scrub the PAT from the
    /// remote URL. Best-effort (push may fail offline); scrub always runs.
    pub fn finalize(&self) -> Result<()> {
        let _ = git(&["add", "-A"], &self.dir);
        // `git diff --cached --quiet` exits non-zero when there ARE staged changes.
        let has_staged = !Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(&self.dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(true);
        if has_staged {
            git(&["commit", "-m", &format!("gitagent: session {}", self.branch)], &self.dir).ok();
        }
        git(&["push", "origin", &self.branch], &self.dir).ok();
        // Always scrub the token back out of .git/config.
        git(&["remote", "set-url", "origin", &self.clean_url], &self.dir).ok();
        Ok(())
    }
}
