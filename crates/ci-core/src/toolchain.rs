//! Toolchain requirements — the layer that tells a user WHAT a language needs, WHETHER it's
//! installed, and HOW to get it, instead of a cryptic spawn error mid-run. The shape is
//! language-blind (this crate never names node or rust-analyzer); each provider crate exposes a
//! `toolchain()` returning its own facts, and the registry builders consult it BEFORE
//! constructing a provider — so a repo without a language never probes (let alone installs)
//! that language's tools.
use std::process::{Command, Stdio};

/// One external tool a language provider depends on, and whether it was found.
#[derive(Debug, Clone)]
pub struct ToolStatus {
    /// Binary name(s) as the user knows them, e.g. `"node (+ npx)"`.
    pub tool: &'static str,
    /// What breaks without it — scoped honestly (some tools gate only the WRITE path).
    pub needed_for: &'static str,
    /// Actionable install instruction (command or URL).
    pub install: &'static str,
    /// The probed version line when present; `None` = missing.
    pub found: Option<String>,
}

/// A language's full toolchain check.
#[derive(Debug, Clone)]
pub struct ToolchainReport {
    pub lang: &'static str,
    pub tools: Vec<ToolStatus>,
}

impl ToolchainReport {
    /// True when every required tool was found.
    pub fn ok(&self) -> bool {
        self.tools.iter().all(|t| t.found.is_some())
    }

    pub fn missing(&self) -> impl Iterator<Item = &ToolStatus> {
        self.tools.iter().filter(|t| t.found.is_none())
    }

    /// The actionable message for everything missing, or `None` when complete. This is the
    /// text a user actually sees — one line per tool: what's absent, what it's for, how to
    /// install it.
    pub fn describe_missing(&self) -> Option<String> {
        let lines: Vec<String> = self
            .missing()
            .map(|t| format!("{} requires {} — needed for {}. Install: {}", self.lang, t.tool, t.needed_for, t.install))
            .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }
}

/// Probe one tool: run it with `args` (conventionally `--version`) and return the first output
/// line on success. `None` = not runnable (missing from PATH, not executable, wrong arch…).
/// Never inherits stdio — a probe must not pollute an MCP/JSON-RPC stream.
pub fn probe_tool(cmd: &mut Command) -> Option<String> {
    let out = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let first = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string();
    Some(if first.is_empty() { "present".into() } else { first })
}
