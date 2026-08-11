//! agent.yaml manifest (subset).

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub preferred: String,
    #[serde(default)]
    pub fallback: Vec<String>,
    /// Sampling constraints forwarded to the provider request.
    #[serde(default)]
    pub constraints: Option<Constraints>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Constraints {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub top_p: Option<f64>,
}

/// Permission gating (Claude-Code-style modes + allow/deny rules).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PermissionConfig {
    /// "" | "default" | "plan" | "acceptEdits" | "bypass".
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Runtime {
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
}

fn default_max_turns() -> u32 {
    50
}

/// Compliance guardrails injected into the system prompt.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Compliance {
    #[serde(default)]
    pub standards: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
}

/// Sandbox wrapper for `cli` execution. `wrapper` is an argv prefix (e.g.
/// `["docker","run","--rm","-i","-v","{cwd}:/work","-w","/work","alpine","sh","-c"]`);
/// the `{cwd}` token is substituted and the command is appended as the final
/// argument, so nothing is shell-interpolated — the command can't break out.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Sandbox {
    #[serde(default)]
    pub wrapper: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentManifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub model: ModelConfig,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub runtime: Option<Runtime>,
    #[serde(default)]
    pub permissions: Option<PermissionConfig>,
    #[serde(default)]
    pub compliance: Option<Compliance>,
    #[serde(default)]
    pub sandbox: Option<Sandbox>,
    /// A parent agent to inherit from — a git URL (cloned into .gitagent/deps/)
    /// or a local path.
    #[serde(default)]
    pub extends: Option<String>,
    /// Agent packages this agent depends on — each cloned from git at load time.
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

/// A git-sourced dependency (gitagent protocol): cloned into .gitagent/deps/<name>.
#[derive(Debug, Clone, Deserialize)]
pub struct Dependency {
    pub name: String,
    pub source: String,
    /// Branch/tag to clone (`--branch`); omitted → default branch.
    #[serde(default)]
    pub version: Option<String>,
}

impl AgentManifest {
    pub fn max_turns(&self) -> u32 {
        self.runtime.as_ref().map(|r| r.max_turns).unwrap_or(50)
    }
}
