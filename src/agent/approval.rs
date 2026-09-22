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

impl Tier {
    /// An extension may declare a tool's tier (its `initialize` reply or a
    /// manifest `tier:` field). This is a trust decision made by whoever
    /// installed the extension: a tool claiming `read` joins the read-only
    /// parallel batch, so only lower the tier for tools you would let run anyway.
    pub fn parse(s: &str) -> Option<Tier> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" | "readonly" | "read-only" => Some(Tier::Read),
            "write" => Some(Tier::Write),
            "exec" | "execute" => Some(Tier::Exec),
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
    pub tool_policies: HashMap<String, Policy>,
    /// command ask-list (`~/.llm/blacklist` + `.llm/blacklist`); a hit
    /// forces the approval prompt in either mode, after the destructive
    /// list
    pub blacklist: crate::agent::blacklist::Blacklist,
    /// blacklist patterns approved with `a` this session: they skip the
    /// ask until the process exits, never persisted
    pub blacklist_session_allows: Vec<String>,
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

/// The ask-list pattern a bash command hits, if any (`None` = no match or a
/// `!` line exempted it).
pub fn blacklist_hit(cfg: &ApprovalConfig, cmd: &str) -> Option<String> {
    let segments = split_compound(cmd);
    let positions = segments
        .iter()
        .flat_map(|seg| command_positions(seg, 0))
        .collect::<Vec<_>>();
    match cfg.blacklist.evaluate(&segments, &positions) {
        crate::agent::blacklist::Match::Deny(pattern) => Some(pattern),
        _ => None,
    }
}

/// Resolve whether a tool call may run. Precedence: the hardcoded refusal
/// list > the user's ask-list (always prompts, immune to allow policies) >
/// explicit policy (deny/prompt/allow) > auto. Everything not caught by one
/// of those runs: there is no ask mode — a call either passes the blacklist
/// and policies, or it does not. `bash_command` is the raw command line for
/// exec-tier tools, and `escapes_cwd` is the tool's own answer
/// (`Tool::escapes_cwd`). `resolve_with_hit` is the entry the loop uses: it
/// has the ask-list pattern in hand already (it carries it down to the prompt
/// and to `a`), so a command line is lexed once per call rather than twice.
pub fn resolve_with_hit(
    name: &str,
    tier: Tier,
    escapes_cwd: bool,
    cfg: &ApprovalConfig,
    bash_command: Option<&str>,
    hit: Option<String>,
) -> Decision {
    // Shell commands face two file-independent layers: the hardcoded refusals
    // first (privilege escalation, filesystem and machine destruction, a fork
    // bomb, a write into a device node — a refusal, not a prompt: no run makes
    // these the right answer), then the user's ask-list, whose hit forces the
    // approval prompt unless a `!` line exempted the pattern or `a` approved
    // it earlier this session.
    let mut blacklist_ask: Option<String> = None;
    if let Some(pattern) = hit
        && !cfg.blacklist_session_allows.iter().any(|a| a == &pattern)
    {
        blacklist_ask = Some(format!(
            "command matches blacklist pattern '{pattern}' — approval required"
        ));
    }
    // the `outside-cwd` directive gives a path leaving the working directory
    // the same standing a matched pattern has
    if escapes_cwd
        && cfg.blacklist.asks_outside_cwd()
        && !cfg
            .blacklist_session_allows
            .iter()
            .any(|a| a == crate::agent::blacklist::OUTSIDE_CWD)
    {
        blacklist_ask =
            Some("path outside the working directory (blacklist outside-cwd)".to_string());
    }
    if let (Tier::Exec, Some(cmd)) = (tier, bash_command)
        && let Some(reason) = forbidden_command(cmd)
    {
        return Decision::Deny(reason);
    }
    match cfg.tool_policies.get(name) {
        Some(Policy::Deny) => {
            return Decision::Deny(format!("tool '{name}' is denied by configuration"));
        }
        Some(Policy::Prompt) => {
            return Decision::Ask(format!("tool '{name}' is set to prompt"));
        }
        // an allow policy cannot answer a blacklist ask: the file gates
        // every run of the pattern, that is its point
        Some(Policy::Allow) if blacklist_ask.is_none() => return Decision::Auto,
        _ => {}
    }
    if let Some(reason) = blacklist_ask {
        return Decision::Ask(reason);
    }
    Decision::Auto
}

/// True when a call's `path` argument leaves the working directory.
pub fn args_escape_cwd(cwd: &std::path::Path, args: &serde_json::Value) -> bool {
    args.get("path")
        .and_then(|p| p.as_str())
        .is_some_and(|p| escapes_cwd(cwd, p))
}

/// True when a shell command line names a path outside the working
/// directory. Best-effort by nature — a shell can build a path no lexer
/// sees — so this gates, it does not sandbox. Every token that names a path
/// is resolved (with `$VAR`/`${VAR}` expanded from the environment); a
/// path-naming token whose expansion cannot be resolved statically counts
/// as escaping rather than as harmless.
pub fn command_escapes_cwd(cwd: &std::path::Path, command: &str) -> bool {
    split_compound(command).iter().any(|seg| {
        tokens(seg)
            .iter()
            .filter_map(|tok| token_path(tok))
            .any(|p| p.contains('$') || p.contains('`') || escapes_cwd(cwd, &p))
    })
}

/// The path one command token names, if it names one: a literal path (it
/// carries a separator or starts at `~`), the value half of `--flag=path`
/// or `VAR=path`. Flags and ordinary words are not paths. Both separators
/// count: PowerShell on Windows takes `\` as well as `/`.
fn token_path(tok: &str) -> Option<String> {
    if let Some((_, value)) = tok.split_once('=') {
        return token_path(value);
    }
    if tok.starts_with('-') {
        return None;
    }
    let path = expand_env(tok);
    (path.contains('/') || path.contains('\\') || path.starts_with('~')).then_some(path)
}

/// `$VAR`/`${VAR}` replaced from the environment, so `cat $HOME/x` is
/// checked against the real home. Anything else (`$1`, `$(...)`, an unknown
/// variable) is left exactly as written, which marks the token unresolvable.
fn expand_env(tok: &str) -> String {
    if !tok.contains('$') {
        return tok.to_string();
    }
    let mut out = String::new();
    let mut chars = tok.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphanumeric() || n == '_' {
                name.push(n);
                chars.next();
            } else {
                break;
            }
        }
        let closed = !braced || chars.peek() == Some(&'}');
        match (closed, name.is_empty(), std::env::var(&name)) {
            (true, false, Ok(value)) => {
                if braced {
                    chars.next();
                }
                out.push_str(&value);
            }
            // not a plain variable reference: keep the text as it was
            _ => {
                out.push('$');
                if braced {
                    out.push('{');
                }
                out.push_str(&name);
            }
        }
    }
    out
}

/// True when a path argument leaves the working directory. Both sides are
/// canonicalized so a symlink cannot smuggle a path out — for a target that
/// does not exist yet (`write`, `edit`), the deepest existing ancestor is
/// the part that gets followed. The cwd rides the same helper on purpose: a
/// working directory can itself be reached through a symlink (`/tmp` is
/// `/private/tmp` on macOS) or not exist yet, and resolving only one side
/// compares a symlink-free path against a lexical one — every path inside
/// then looks like it left.
pub fn escapes_cwd(cwd: &std::path::Path, arg: &str) -> bool {
    let target = canonical_with_missing(&normalize(&crate::agent::tools::resolve_path(cwd, arg)));
    !target.starts_with(canonical_with_missing(cwd))
}

/// [`Path::canonicalize`] for a path that may not exist yet: the existing
/// prefix is resolved through symlinks and the missing tail is rejoined
/// verbatim, so `link-to-elsewhere/new-file.txt` lands where the write would.
fn canonical_with_missing(target: &std::path::Path) -> std::path::PathBuf {
    let mut probe = target.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = probe.canonicalize() {
            let mut out = real;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out;
        }
        match (probe.file_name(), probe.parent()) {
            (Some(name), Some(parent)) if parent != probe => {
                tail.push(name.to_os_string());
                probe = parent.to_path_buf();
            }
            // nothing of the path is left to resolve: keep it lexical
            _ => return target.to_path_buf(),
        }
    }
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

/// Split a command line into segments on `&&`, `||`, `;`, `|`, `&` and
/// newlines, respecting single/double quotes.
fn split_compound(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    current.push(c);
                }
                '\n' | ';' => {
                    segments.push(std::mem::take(&mut current));
                }
                '&' => {
                    if chars.peek() == Some(&'&') {
                        chars.next();
                    }
                    segments.push(std::mem::take(&mut current));
                }
                '|' => {
                    if chars.peek() == Some(&'|') {
                        chars.next();
                    }
                    segments.push(std::mem::take(&mut current));
                }
                _ => current.push(c),
            },
        }
    }
    segments.push(current);
    segments
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whitespace tokens of a segment with surrounding quotes stripped, so a
/// `bash -c 'git status'` payload reads as its own command line.
fn tokens(seg: &str) -> Vec<String> {
    seg.split_whitespace()
        .map(|t| t.trim_matches(['\'', '"']).to_string())
        .collect()
}

/// Programs that only ever run another command: whatever follows them (past
/// their own flags and argument) is a command position too.
const WRAPPERS: &[&str] = &[
    "xargs", "nohup", "setsid", "stdbuf", "nice", "ionice", "time", "watch", "env", "timeout",
    "command", "exec",
];

/// Shells whose `-c` argument is a full command line of its own.
const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "ash"];

/// Every token of a segment that starts a command: the first word (past any
/// leading env assignments), whatever a wrapper runs next, and `shell -c`
/// payloads recursed one level. `xargs rm`, `env git push` and
/// `bash -c 'git status'` all surface their inner command; a bare argument
/// like the `rm` in `grep rm notes.txt` never does.
fn command_positions(seg: &str, depth: usize) -> Vec<String> {
    let toks = tokens(seg);
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() && toks[i].contains('=') {
        i += 1;
    }
    while i < toks.len() {
        let t = toks[i].as_str();
        if SHELLS.contains(&t)
            && matches!(
                toks.get(i + 1).map(String::as_str),
                Some("-c") | Some("-lc")
            )
            && i + 2 < toks.len()
        {
            if depth < 2 {
                out.extend(command_positions(&toks[i + 2..].join(" "), depth + 1));
            }
            return out;
        }
        out.push(t.to_string());
        if WRAPPERS.contains(&t) {
            i += 1;
            while i < toks.len() && toks[i].starts_with('-') {
                i += 1;
            }
            if t == "timeout" && i < toks.len() && !toks[i].contains('=') {
                i += 1; // the duration word
            }
            continue;
        }
        return out;
    }
    out
}

/// The commands that must never run, in either mode, whatever any config key
/// or ask-list file says: privilege escalation, filesystem creation and
/// destruction, and machine-level control. Hardcoded on purpose — this is
/// the core a user cannot switch off, unlike the ask-list file, whose hits
/// prompt instead of refusing.
const FORBIDDEN_COMMANDS: &[&str] = &[
    // privilege escalation
    "sudo",
    "su",
    "doas",
    // filesystem creation and destruction
    "mkfs",
    "mkfs.ext2",
    "mkfs.ext4",
    "mkfs.xfs",
    "mkfs.btrfs",
    "mkfs.vfat",
    "mkswap",
    "fdisk",
    "sfdisk",
    "cfdisk",
    "parted",
    "wipefs",
    "shred",
    "dd",
    // machine-level control
    "shutdown",
    "reboot",
    "poweroff",
    "halt",
    "init",
];

/// The hardcoded first line of defence: `Some(reason)` when the command must
/// never run. Checked before the blacklist file and before either approval
/// mode, so no file edit and no flag can turn any of it back on.
fn forbidden_command(command: &str) -> Option<String> {
    if command.contains(":(){") {
        return Some("command is a fork bomb".to_string());
    }
    if redirects_into_device(command) {
        return Some("command writes into a device node".to_string());
    }
    for seg in split_compound(command) {
        if let Some(first) = command_positions(&seg, 0).first()
            && FORBIDDEN_COMMANDS.contains(&first.as_str())
        {
            return Some(format!("'{first}' is never allowed"));
        }
        if deletes_the_root(&seg) {
            return Some("command deletes the whole filesystem".to_string());
        }
    }
    None
}

/// `rm -rf /` and friends: an `rm` whose target word *is* the root or a home
/// shorthand. Matched on the whole word, so `rm -rf /tmp/build` stays the
/// ordinary cleanup it is and only the whole-filesystem case is refused.
fn deletes_the_root(segment: &str) -> bool {
    let toks = tokens(segment);
    let words: Vec<&str> = toks
        .iter()
        .map(String::as_str)
        .filter(|t| !t.starts_with('-'))
        .collect();
    if words.first() != Some(&"rm") {
        return false;
    }
    words[1..]
        .iter()
        .any(|w| matches!(*w, "/" | "/*" | "~" | "~/" | "$HOME" | "${HOME}"))
}

/// Redirect targets that only ever discard or pass bytes through: writing
/// these destroys nothing.
const SAFE_DEVICES: &[&str] = &["null", "zero", "full", "stdout", "stderr", "tty", "console"];

/// True when the command redirects output into a real device node. Every
/// `>`/`>>`/`2>`/`&>` form is checked; `/dev/null` and friends are exempt
/// (they are the standard way to silence a stream), block devices are not.
fn redirects_into_device(command: &str) -> bool {
    let bytes = command.as_bytes();
    let mut i = 0usize;
    while let Some(rel) = command[i..].find('>') {
        let at = i + rel;
        i = at + 1;
        let mut j = at + 1;
        // `>>`, `>&`, spaces and quoted targets interleave: skip them all
        // together before the redirect destination
        while matches!(
            bytes.get(j),
            Some(b'&') | Some(b'>') | Some(b'"') | Some(b' ')
        ) {
            j += 1;
        }
        if command[j..].starts_with("/dev/") {
            let rest = &command[j + 5..];
            let name = rest
                .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '"' | '\''))
                .next()
                .unwrap_or("");
            if !name.is_empty() && !SAFE_DEVICES.contains(&name) {
                return true;
            }
        }
    }
    false
}

/// y/N/a prompt on the terminal. Fails closed (Deny) when no interactive
/// terminal is available.
pub fn prompt_approval(req: &ApprovalRequest, pre: Vec<u8>) -> ApprovalResponse {
    let verb = crate::agent::tools::display_verb(req.tool);
    // the same activity line the tool log prints (bold $, command in green)
    crate::agent::tools::print_action_line(verb, req.preview, req.diff);
    if !req.reason.is_empty() {
        let p = crate::theme::err();
        if let Some(pattern) = req.pattern {
            // the matched ask-list pattern is the thing to read: bold on red
            eprintln!(
                "{}  blacklist hit:{} {}{}{pattern}{}{} — asks every time; a spares it for the session{}",
                p.dim, p.reset, p.bold, p.red, p.reset, p.dim, p.reset
            );
        } else {
            eprintln!("{}  {}{}", p.dim, req.reason, p.reset);
        }
    }
    use crate::term::lineedit::{ApprovalKey, read_approval_key};
    eprint!(
        "  {}{}Allow?{} {}[Y/n/a]{} ",
        crate::theme::err().bold,
        crate::theme::err().cyan,
        crate::theme::err().reset,
        crate::theme::err().bold,
        crate::theme::err().reset
    );
    let _ = std::io::stderr().flush();
    match read_approval_key(pre) {
        Some(ApprovalKey::Yes) => ApprovalResponse::Allow,
        Some(ApprovalKey::Always) => ApprovalResponse::AllowSession,
        // n, ctrl-c, ctrl-d, esc → deny; the tool result carries the reason
        Some(_) => ApprovalResponse::Deny,
        // no raw terminal: fail closed with a hint
        None => {
            if let Some(pattern) = req.pattern {
                eprintln!(
                    "Error: '{pattern}' is on the ask-list but no terminal is available to \
                     approve it. Approve interactively, or remove the pattern from the blacklist file."
                );
            } else {
                eprintln!(
                    "Error: approval needed for '{tool}' but no terminal is available. \
                     Add an allow policy or remove the gating line.",
                    tool = req.tool,
                );
            }
            ApprovalResponse::Deny
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(policies: &[(&str, Policy)]) -> ApprovalConfig {
        ApprovalConfig {
            tool_policies: policies.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            blacklist: crate::agent::blacklist::Blacklist::default(),
            blacklist_session_allows: Vec::new(),
        }
    }

    /// [`resolve_with_hit`] with the ask-list looked up here — the shape the
    /// loop had before it started handing the pattern down itself.
    fn resolve(
        name: &str,
        tier: Tier,
        escapes_cwd: bool,
        cfg: &ApprovalConfig,
        bash_command: Option<&str>,
    ) -> Decision {
        let hit = bash_command.and_then(|cmd| blacklist_hit(cfg, cmd));
        resolve_with_hit(name, tier, escapes_cwd, cfg, bash_command, hit)
    }

    #[test]
    fn the_outside_cwd_directive_asks() {
        let mut config = cfg(&[]);
        config.blacklist = crate::agent::blacklist::Blacklist::parse("outside-cwd");
        // the directive asks for a path that leaves the working directory
        assert!(matches!(
            resolve("read", Tier::Read, true, &config, None),
            Decision::Ask(_)
        ));
        assert!(matches!(
            resolve("write", Tier::Write, true, &config, None),
            Decision::Ask(_)
        ));
        assert_eq!(
            resolve("read", Tier::Read, false, &config, None),
            Decision::Auto,
            "a path inside stays free"
        );
        // an `a` answer spared the directive for the session
        config
            .blacklist_session_allows
            .push(crate::agent::blacklist::OUTSIDE_CWD.to_string());
        assert_eq!(
            resolve("read", Tier::Read, true, &config, None),
            Decision::Auto
        );
        // and without the directive, nothing asks
        assert_eq!(
            resolve("read", Tier::Read, true, &cfg(&[]), None),
            Decision::Auto
        );
    }

    #[test]
    fn the_hardcoded_core_is_refused() {
        // no blacklist file, no config: these are refused anyway
        for cmd in [
            "sudo apt install x",
            "su -",
            "doas rm x",
            "mkfs.ext4 /dev/sda1",
            "dd if=img of=/dev/sdb",
            "shutdown now",
            "reboot",
            "halt",
            "echo hi | sudo tee /etc/hosts",
            "cat img.iso > /dev/sdb",
            "echo x >>/dev/sda",
            ":(){ :|:& };:",
            "rm -rf /",
            "rm -rf ~",
            "rm -rf /*",
        ] {
            let d = resolve("bash", Tier::Exec, false, &cfg(&[]), Some(cmd));
            assert!(
                matches!(d, Decision::Deny(_)),
                "{cmd} must be denied, got {d:?}"
            );
        }
        // the reason names the offending command, so the model can adapt
        let d = resolve("bash", Tier::Exec, false, &cfg(&[]), Some("sudo -i"));
        assert_eq!(d, Decision::Deny("'sudo' is never allowed".into()));
    }

    #[test]
    fn ordinary_commands_run_free() {
        for cmd in [
            "ls",
            "git status",
            "cargo test",
            "npm init -y",
            "git init",
            // an rm of real paths is normal cleanup: only the whole-root
            // case is refused
            "rm -rf /tmp/build",
            "rm -rf ./build",
            "rm notes.txt",
            "rm -rf /home/me/proj/target",
        ] {
            let d = resolve("bash", Tier::Exec, false, &cfg(&[]), Some(cmd));
            assert_eq!(d, Decision::Auto, "{cmd} should run free");
        }
    }

    #[test]
    fn a_blacklist_line_cannot_re_enable_a_hardcoded_refusal() {
        let mut c = cfg(&[]);
        c.blacklist = crate::agent::blacklist::Blacklist::parse("!sudo\n!rm -rf /");
        assert!(matches!(
            resolve("bash", Tier::Exec, false, &c, Some("sudo -i")),
            Decision::Deny(_)
        ));
        assert!(matches!(
            resolve("bash", Tier::Exec, false, &c, Some("rm -rf /")),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn a_blacklisted_command_asks() {
        let mut c = cfg(&[]);
        c.blacklist = crate::agent::blacklist::Blacklist::parse("rm\n!rm -rf ./build");
        // the hit is a prompt now, not a refusal
        match resolve("bash", Tier::Exec, false, &c, Some("rm notes.txt")) {
            Decision::Ask(reason) => {
                assert!(reason.contains("'rm'"), "{reason}");
            }
            other => panic!("rm notes.txt must ask, got {other:?}"),
        }
        // and the `!` line punches the hole the user asked for
        assert_eq!(
            resolve("bash", Tier::Exec, false, &c, Some("rm -rf ./build")),
            Decision::Auto
        );
        // the ask survives an allow policy: gating every run is the point
        let mut c = cfg(&[("bash", Policy::Allow)]);
        c.blacklist = crate::agent::blacklist::Blacklist::parse("rm");
        assert!(matches!(
            resolve("bash", Tier::Exec, false, &c, Some("rm notes.txt")),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn an_always_answer_spares_the_pattern_for_the_session() {
        let mut c = cfg(&[]);
        c.blacklist = crate::agent::blacklist::Blacklist::parse("rm\ngit push --force*");
        c.blacklist_session_allows.push("rm".into());
        assert_eq!(
            resolve("bash", Tier::Exec, false, &c, Some("rm notes.txt")),
            Decision::Auto
        );
        // a pattern not yet approved still asks
        assert!(matches!(
            resolve(
                "bash",
                Tier::Exec,
                false,
                &c,
                Some("git push --force origin")
            ),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn the_default_ask_list_gates_rm_and_force_pushes() {
        let mut c = cfg(&[]);
        c.blacklist = crate::agent::blacklist::Blacklist::parse(
            &crate::agent::blacklist::Blacklist::default_file(),
        );
        assert!(matches!(
            resolve("bash", Tier::Exec, false, &c, Some("rm -rf ./build")),
            Decision::Ask(_)
        ));
        assert!(matches!(
            resolve(
                "bash",
                Tier::Exec,
                false,
                &c,
                Some("git push --force origin main")
            ),
            Decision::Ask(_)
        ));
        // ordinary work runs free
        assert_eq!(
            resolve("bash", Tier::Exec, false, &c, Some("cargo test")),
            Decision::Auto
        );
    }

    #[test]
    fn dev_null_redirects_run_free() {
        // real-world exploration commands from the field: every `2>/dev/null`
        // is stream hygiene, not destruction
        for cmd in [
            "ls -la scripts/ evo/ && echo --- && cat requirements.txt && \
             ls tests 2>/dev/null || echo \"no tests dir\"",
            "ls results/ | head -30; git check-ignore -v a.zip b.zip 2>/dev/null; \
             du -sh .git 2>/dev/null",
            "cargo build 2>/dev/null",
            "echo hi >/dev/null",
            "wc -l <file >>/dev/null",
            "python x.py &>/dev/null",
        ] {
            let d = resolve("bash", Tier::Exec, false, &cfg(&[]), Some(cmd));
            assert_eq!(d, Decision::Auto, "{cmd} must run free");
        }
        // a real device target is refused by the shape check; `dd of=` paths
        // are caught by the program list, not the redirect scan
        assert!(forbidden_command("x >/dev/nvme0n1").is_some());
        assert!(forbidden_command("x > \"/dev/sda\"").is_some());
        assert!(!redirects_into_device("dd if=a of=/dev/nvme0n1"));
        assert!(!redirects_into_device("x 2>/dev/null"));
        assert!(!redirects_into_device("x > /dev/stdout"));
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
    #[cfg(unix)]
    fn a_cwd_that_does_not_exist_is_resolved_like_the_paths_under_it() {
        // the bug this pins: resolving only the target left a lexical cwd on
        // one side and a symlink-free path on the other, so a path *inside*
        // the working directory looked like it had left it — which is how a
        // `/var/folders/...` cwd on macOS failed every relative path
        let real = crate::core::testutil::scratch_dir("escapes-real");
        let root = crate::core::testutil::scratch_dir("escapes-root");
        std::os::unix::fs::symlink(&real, root.join("link")).unwrap();
        let cwd = root.join("link").join("absent");
        assert!(!escapes_cwd(&cwd, "src/main.rs"));
        assert!(escapes_cwd(&cwd, "../outside.txt"));
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn a_symlinked_directory_does_not_smuggle_a_new_file_out_of_the_cwd() {
        let outside = crate::core::testutil::scratch_dir("escapes-outside");
        let cwd = crate::core::testutil::scratch_dir("escapes-cwd");
        std::os::unix::fs::symlink(&outside, cwd.join("link")).unwrap();
        // the file does not exist yet — exactly what a write targets
        assert!(!outside.exists() || outside.is_dir());
        assert!(escapes_cwd(&cwd, "link/new.txt"));
        assert!(escapes_cwd(&cwd, "link/deep/new.txt"));
        // and the cwd's own symlinked prefix is not a false positive: a
        // path inside the directory stays inside however it is reached
        std::fs::create_dir_all(cwd.join("real")).unwrap();
        assert!(!escapes_cwd(&cwd, "real/new.txt"));
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn a_path_argument_is_checked() {
        let cwd = std::path::Path::new("/home/user/proj");
        assert!(!args_escape_cwd(cwd, &json!({"path": "src/a.rs"})));
        assert!(args_escape_cwd(cwd, &json!({"path": "/etc/passwd"})));
        assert!(args_escape_cwd(cwd, &json!({"path": "../secrets"})));
        assert!(!args_escape_cwd(cwd, &json!({"pattern": "x"})));
    }

    #[test]
    fn a_command_line_is_scanned_for_paths_outside_the_cwd() {
        let cwd = std::path::Path::new("/home/user/proj");
        let escapes = |cmd: &str| command_escapes_cwd(cwd, cmd);
        // the project's own workflow is untouched
        assert!(!escapes("cargo test --all-targets"));
        assert!(!escapes("rg foo src/agent && cat src/main.rs"));
        assert!(!escapes("git commit -m 'fix src/x'"));
        // a read-only whitelisted word is no longer a way around the gate
        assert!(escapes("cat /etc/passwd"));
        assert!(escapes("ls ~/.ssh"));
        assert!(escapes("cd /tmp && ls"));
        assert!(escapes("rg --files ../other"));
        assert!(escapes("cat --file=/etc/hosts"));
        // `$HOME` is expanded, so the real target is what gets checked
        assert!(escapes("cat $HOME/.ssh/id_rsa"));
        // an expansion that cannot be resolved is not assumed harmless
        assert!(escapes("cat $SECRET_DIR/x"));
        assert!(escapes("cat `pwd`/x"));
        // a bare word, a flag, or a value the `=` half of a flag: not paths
        assert!(!escapes("awk '{print $1}' src/main.rs"));
        assert!(!escapes("cargo test --features=serde"));
    }

    #[test]
    fn a_token_naming_a_path_is_recognized_on_every_platform() {
        // Windows takes `\` as a separator; on unix the token is just a
        // relative name, so the resolution stays the platform's business
        assert!(token_path(r"..\other\secrets.txt").is_some());
        assert!(token_path(r"C:\Windows\win.ini").is_some());
        assert!(token_path("src/main.rs").is_some());
        assert!(token_path("--file=/etc/hosts").is_some());
        assert!(token_path("~/notes.txt").is_some());
        // flags and ordinary words are not paths
        assert!(token_path("--verbose").is_none());
        assert!(token_path("main.rs").is_none());
        assert!(token_path("--features=serde").is_none());
    }

    #[test]
    fn per_tool_policies_win() {
        let deny = resolve(
            "bash",
            Tier::Exec,
            false,
            &cfg(&[("bash", Policy::Deny)]),
            None,
        );
        assert!(matches!(deny, Decision::Deny(_)));
        let allow = resolve(
            "bash",
            Tier::Exec,
            false,
            &cfg(&[("bash", Policy::Allow)]),
            None,
        );
        assert_eq!(allow, Decision::Auto);
        let prompt = resolve(
            "read",
            Tier::Read,
            false,
            &cfg(&[("read", Policy::Prompt)]),
            None,
        );
        assert!(matches!(prompt, Decision::Ask(_)));
    }
}
