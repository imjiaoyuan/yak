//! Three-tier tool approval (the style): every tool declares a tier, a mode
//! sets the baseline, per-tool policies override, and `a` on a prompt allows
//! the tool for the rest of the session.

use std::collections::HashMap;
use std::io::Write;

use crate::agent::{ApprovalRequest, ApprovalResponse};

/// What a tool is allowed to touch. Unknown tools are treated as `Exec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// read-only filesystem access
    Read,
    /// mutates the workspace but runs no arbitrary code
    Write,
    /// shells out or spawns processes
    Exec,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// reads inside the working directory auto, everything else prompts
    #[default]
    AlwaysAsk,
    /// everything auto; the danger table still forces a one-shot prompt
    Yolo,
}

impl Mode {
    /// Canonical lowercase name (also what the UI displays).
    pub fn label(self) -> &'static str {
        match self {
            Mode::AlwaysAsk => "ask",
            Mode::Yolo => "yolo",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "always-ask" | "ask" => Some(Mode::AlwaysAsk),
            "yolo" | "full-auto" => Some(Mode::Yolo),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Allow,
    Deny,
    Prompt,
}

impl Policy {
    pub fn parse(s: &str) -> Option<Policy> {
        match s {
            "allow" => Some(Policy::Allow),
            "deny" => Some(Policy::Deny),
            "prompt" => Some(Policy::Prompt),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct ApprovalConfig {
    pub mode: Mode,
    pub tool_policies: HashMap<String, Policy>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// run without asking
    Auto,
    /// ask the user; carries the reason shown in the prompt
    Ask(String),
    /// never run; carries the reason fed back to the model
    Deny(String),
}

/// Resolve whether a tool call may run. Precedence follows oh-my-the reference:
/// explicit policy (deny/prompt/allow) > yolo short-circuit > mode-vs-tier.
/// Explicit prompts and denies hold even in yolo mode.
///
/// `escapes_cwd` marks a path argument that leaves the working directory;
/// Linux-style gate: reads inside the tree are free, everything else asks.
pub fn resolve(name: &str, tier: Tier, escapes_cwd: bool, cfg: &ApprovalConfig) -> Decision {
    match cfg.tool_policies.get(name) {
        Some(Policy::Deny) => {
            return Decision::Deny(format!("tool '{name}' is denied by configuration"));
        }
        Some(Policy::Prompt) => {
            return Decision::Ask(format!("tool '{name}' is set to prompt"));
        }
        Some(Policy::Allow) => return Decision::Auto,
        _ => {}
    }
    if cfg.mode == Mode::Yolo {
        return Decision::Auto;
    }
    match (tier, escapes_cwd) {
        (Tier::Read, false) => Decision::Auto,
        (Tier::Read, true) => Decision::Ask("reading outside the working directory".to_string()),
        (Tier::Write, _) => Decision::Ask("modifying files requires approval".to_string()),
        (Tier::Exec, _) => Decision::Ask("running commands requires approval".to_string()),
    }
}

/// True when a path argument leaves the working directory. Canonicalizes
/// both sides when possible so symlinks cannot smuggle a path out; falls
/// back to a lexical check for paths that do not exist yet.
pub fn escapes_cwd(cwd: &std::path::Path, arg: &str) -> bool {
    let target = normalize(&crate::agent::tools::resolve_path(cwd, arg));
    if let (Ok(base), Ok(t)) = (cwd.canonicalize(), target.canonicalize()) {
        return !t.starts_with(&base);
    }
    !target.starts_with(normalize(cwd))
}

/// Lexically drop `.` and resolve `..` without touching the filesystem.
fn normalize(p: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// y/N/a prompt on the terminal. Fails closed (Deny) when no interactive
/// terminal is available.
pub fn prompt_approval(req: &ApprovalRequest, pre: Vec<u8>) -> ApprovalResponse {
    let verb = crate::agent::tools::display_verb(req.tool);
    // the same activity line the tool log prints (bold $, command in green)
    crate::agent::tools::print_action_line(verb, req.preview, req.diff);
    if !req.reason.is_empty() {
        eprintln!("\x1b[2m  {reason}\x1b[0m", reason = req.reason);
    }
    use crate::term::lineedit::{ApprovalKey, read_approval_key};
    eprint!("  \x1b[1m\x1b[36mAllow?\x1b[0m \x1b[1m[Y/n/a]\x1b[0m ");
    let _ = std::io::stderr().flush();
    match read_approval_key(pre) {
        Some(ApprovalKey::Yes) => ApprovalResponse::Allow,
        Some(ApprovalKey::Always) => ApprovalResponse::AllowSession,
        // n, ctrl-c, ctrl-d, esc → deny; the tool result carries the reason
        Some(_) => ApprovalResponse::Deny,
        // no raw terminal: fail closed with a hint
        None => {
            eprintln!(
                "Error: approval needed for {tier:?}-tier tool '{tool}' but no terminal is available. \
                 Re-run with --yolo or an allow policy.",
                tier = req.tier,
                tool = req.tool,
            );
            ApprovalResponse::Deny
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: Mode, policies: &[(&str, Policy)]) -> ApprovalConfig {
        ApprovalConfig {
            mode,
            tool_policies: policies.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    #[test]
    fn mode_tier_matrix() {
        let read = resolve("read", Tier::Read, false, &cfg(Mode::AlwaysAsk, &[]));
        assert_eq!(read, Decision::Auto);
        let read_out = resolve("read", Tier::Read, true, &cfg(Mode::AlwaysAsk, &[]));
        assert!(matches!(read_out, Decision::Ask(_)));
        let write = resolve("write", Tier::Write, false, &cfg(Mode::AlwaysAsk, &[]));
        assert!(matches!(write, Decision::Ask(_)));
        let exec = resolve("bash", Tier::Exec, false, &cfg(Mode::AlwaysAsk, &[]));
        assert!(matches!(exec, Decision::Ask(_)));
        assert_eq!(
            resolve("bash", Tier::Exec, true, &cfg(Mode::Yolo, &[])),
            Decision::Auto
        );
    }

    #[test]
    fn escapes_detection() {
        let cwd = std::path::Path::new("/home/user/proj");
        assert!(!escapes_cwd(cwd, "src/main.rs"));
        assert!(!escapes_cwd(cwd, "./src/../src/main.rs"));
        assert!(!escapes_cwd(cwd, "/home/user/proj/a/b.txt"));
        assert!(!escapes_cwd(cwd, "."));
        assert!(escapes_cwd(cwd, "../outside.txt"));
        assert!(escapes_cwd(cwd, "/etc/passwd"));
        assert!(escapes_cwd(cwd, "~/notes.txt"));
    }

    #[test]
    fn policies_override_modes() {
        let deny = resolve(
            "bash",
            Tier::Exec,
            false,
            &cfg(Mode::Yolo, &[("bash", Policy::Deny)]),
        );
        assert!(matches!(deny, Decision::Deny(_)));
        let allow = resolve(
            "bash",
            Tier::Exec,
            false,
            &cfg(Mode::AlwaysAsk, &[("bash", Policy::Allow)]),
        );
        assert_eq!(allow, Decision::Auto);
        let prompt = resolve(
            "read",
            Tier::Read,
            false,
            &cfg(Mode::Yolo, &[("read", Policy::Prompt)]),
        );
        assert!(matches!(prompt, Decision::Ask(_)));
    }
}
