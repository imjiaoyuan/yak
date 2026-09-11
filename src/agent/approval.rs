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
    #[allow(dead_code)] // used by tests/diagnostics; kept for the tier vocabulary
    pub fn label(self) -> &'static str {
        match self {
            Tier::Read => "read",
            Tier::Write => "write",
            Tier::Exec => "exec",
        }
    }

    /// An extension may declare a tool's tier (its `initialize` reply or a
    /// manifest `tier:` field). This is a trust decision made by whoever
    /// installed the extension: a tool claiming `read` runs unprompted in
    /// ask mode, so only lower the tier for tools you would let run anyway.
    pub fn parse(s: &str) -> Option<Tier> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" | "readonly" | "read-only" => Some(Tier::Read),
            "write" => Some(Tier::Write),
            "exec" | "execute" => Some(Tier::Exec),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// reads inside the working directory auto, everything else prompts
    AlwaysAsk,
    /// everything auto; the destructive-command list still forces a
    /// one-shot prompt
    #[default]
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
    /// command blacklist (`~/.llm/blacklist` + `.llm/blacklist`); a hit
    /// denies the bash call outright, ahead of the destructive list
    pub blacklist: crate::agent::blacklist::Blacklist,
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

/// Resolve whether a tool call may run. Precedence: explicit policy
/// (deny/prompt/allow) > yolo short-circuit (a short destructive-command
/// list still asks once) > tier gate. Explicit prompts and denies hold even
/// in yolo mode.
///
/// The gate is directory-scoped: reads inside the working directory run free,
/// reads outside it and any file write ask, and a bash command runs free
/// only when every command it would start is on the read-only whitelist —
/// writes, deletes, network fetches, interpreters and anything unrecognized
/// ask. `bash_command` is the raw command line for exec-tier tools.
pub fn resolve(
    name: &str,
    tier: Tier,
    escapes_cwd: bool,
    cfg: &ApprovalConfig,
    bash_command: Option<&str>,
) -> Decision {
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
        if let Tier::Exec = tier
            && let Some(cmd) = bash_command
        {
            let segments = split_compound(cmd);
            let positions = segments
                .iter()
                .flat_map(|seg| command_positions(seg, 0))
                .collect::<Vec<_>>();
            let hit = cfg.blacklist.denied(&segments, &positions);
            if !hit.is_empty() {
                return Decision::Deny(format!("command matches blacklist pattern '{hit}'"));
            }
            if dangerous_command(cmd) {
                // no reason line: the command itself is the explanation
                return Decision::Ask(String::new());
            }
        }
        return Decision::Auto;
    }
    match (tier, escapes_cwd) {
        (Tier::Read, false) => Decision::Auto,
        (Tier::Read, true) => Decision::Ask("reading outside the working directory".to_string()),
        (Tier::Write, _) => Decision::Ask("writing files requires approval".to_string()),
        (Tier::Exec, _) if bash_command.is_some_and(readonly_command) => Decision::Auto,
        (Tier::Exec, _) => {
            // the blacklist applies in ask mode too: a forbidden command
            // never reaches the prompt
            if let Some(cmd) = bash_command {
                let segments = split_compound(cmd);
                let positions = segments
                    .iter()
                    .flat_map(|seg| command_positions(seg, 0))
                    .collect::<Vec<_>>();
                let hit = cfg.blacklist.denied(&segments, &positions);
                if !hit.is_empty() {
                    return Decision::Deny(format!("command matches blacklist pattern '{hit}'"));
                }
            }
            Decision::Ask("running a non-read-only command requires approval".to_string())
        }
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

/// Read-only commands a bash call may start without asking. Deliberately
/// conservative: anything that can write, delete, fetch, interpret code or
/// is simply unknown asks. `git push` is as far from this list as it gets.
const READONLY_COMMANDS: &[&str] = &[
    // file inspection
    "ls", "cat", "head", "tail", "wc", "file", "stat", "du", "df", "readlink", "dirname",
    "basename", "realpath", "tree", // search
    "grep", "egrep", "fgrep", "rg", "ripgrep", "findstr",
    // text processing (no -i, no output redirection — checked separately)
    "sort", "uniq", "diff", "comm", "cmp", "cut", "column", "jq",
    // process / system inspection
    "ps", "pgrep", "uname", "whoami", "hostname", "date", "which", "type", "whereis", "echo",
    "printf", "true", "false", "test",
    "[",
    // git read-only subcommands are matched specially below
];

/// cargo subcommands that only touch the build cache and read the project
/// (the agent's core workflow; `cargo run`/`install`/`publish` stay gated).
const READONLY_CARGO: &[&str] = &[
    "build",
    "test",
    "check",
    "tree",
    "clippy",
    "metadata",
    "version",
    "locate-project",
    "pkgid",
    "rustc-version",
];

/// git subcommands that never mutate anything.
const READONLY_GIT: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "branch",
    "remote",
    "blame",
    "shortlog",
    "describe",
    "ls-files",
    "ls-remote",
    "rev-parse",
    "reflog",
    "grep",
    "cat-file",
    "config",
    "check-ignore",
];

/// True when every command a bash call would start is on the read-only
/// whitelist. Overrides checked separately: output redirection (a write in
/// disguise), `find` with `-delete`/`-exec` (whitelisted word, mutating
/// flags), and `git` subcommands.
pub fn readonly_command(command: &str) -> bool {
    if command.contains('>') {
        return false;
    }
    // command substitution smuggles any program past a whitelisted command
    // word (`echo $(rm -rf x)`) — reject it outright
    if command.contains("$(") || command.contains('`') {
        return false;
    }
    for seg in split_compound(command) {
        let positions = command_positions(&seg, 0);
        if positions.is_empty() {
            return false;
        }
        let mut rest = positions.iter();
        let first = rest.next().unwrap().as_str();
        // `git` is whitelisted only for its read-only subcommands (the
        // positions list stops at the command word, so the subcommand comes
        // from the segment tokens, past global flags like -C)
        if first == "git" {
            let toks = tokens(&seg);
            let sub = toks
                .iter()
                // skip "git" itself (the positions list already ends there
                // for direct calls; a `bash -c 'git status'` payload keeps
                // the whole line in tokens)
                .skip_while(|t| t.as_str() != "git")
                .skip(1)
                .find(|t| !t.starts_with('-'))
                .map(String::as_str)
                .unwrap_or("");
            if !READONLY_GIT.contains(&sub) {
                return false;
            }
            // `git branch x`, `git remote add x y` and `git config k v` all
            // mutate: these three subcommands are read-only only in their
            // bare, flag-only form (config additionally in its one-argument
            // `git config key` read form)
            if matches!(sub, "branch" | "remote" | "config") {
                let non_flag = toks
                    .iter()
                    .skip_while(|t| t.as_str() != "git")
                    .skip(2) // "git" and the subcommand itself
                    .filter(|t| !t.starts_with('-'))
                    .count();
                let read = match sub {
                    "config" => non_flag <= 1,
                    _ => non_flag == 0,
                };
                if !read {
                    return false;
                }
            }
            continue;
        }
        if first == "cargo" {
            let toks = tokens(&seg);
            let sub = toks
                .iter()
                .skip(1)
                .find(|t| !t.starts_with('-'))
                .map(String::as_str)
                .unwrap_or("");
            if !READONLY_CARGO.contains(&sub) {
                return false;
            }
            continue;
        }
        // `sort -o f` and `date -s ...` write through flags instead of `>`
        let flags_write = match first {
            "sort" => tokens(&seg)
                .iter()
                .any(|t| t == "-o" || t.starts_with("--output")),
            "date" => tokens(&seg).iter().any(|t| t == "-s" || t == "--set"),
            _ => false,
        };
        if flags_write {
            return false;
        }
        // `find` is deliberately absent from the whitelist: its mutating
        // forms (-delete/-exec) hide behind a read-only-looking command word
        if !READONLY_COMMANDS.contains(&first) {
            return false;
        }
    }
    true
}

/// Commands that stay behind a one-shot prompt even in yolo mode: deleting
/// files, privilege escalation and anything that can destroy a filesystem
/// or take the machine down. yolo auto-approves everything else; these ask
/// once (`Y` clears the one call, `a` the rest of the session).
const DANGEROUS_COMMANDS: &[&str] = &[
    "rm",
    "sudo",
    "su",
    "doas",
    "mkfs",
    "mkfs.ext2",
    "mkfs.ext4",
    "mkfs.xfs",
    "mkfs.btrfs",
    "mkfs.vfat",
    "dd",
    "shred",
    "wipefs",
    "fdisk",
    "sfdisk",
    "cfdisk",
    "parted",
    "shutdown",
    "reboot",
    "poweroff",
    "halt",
    "init",
];

/// True when a bash call would start a command on the destructive list,
/// carries a fork bomb, or redirects output into a real device node
/// (`> /dev/sda`). Sinks like `2>/dev/null` are hygiene, not destruction.
pub fn dangerous_command(command: &str) -> bool {
    if command.contains(":(){") || redirects_into_device(command) {
        return true;
    }
    split_compound(command).iter().any(|seg| {
        command_positions(seg, 0)
            .first()
            .is_some_and(|first| DANGEROUS_COMMANDS.contains(&first.as_str()))
    })
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
        eprintln!(
            "{}  {}{}",
            crate::theme::err().dim,
            req.reason,
            crate::theme::err().reset
        );
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
            blacklist: crate::agent::blacklist::Blacklist::default(),
        }
    }

    #[test]
    fn mode_tier_matrix() {
        assert_eq!(Mode::default(), Mode::Yolo);
        let read = resolve("read", Tier::Read, false, &cfg(Mode::AlwaysAsk, &[]), None);
        assert_eq!(read, Decision::Auto);
        let read_out = resolve("read", Tier::Read, true, &cfg(Mode::AlwaysAsk, &[]), None);
        assert!(matches!(read_out, Decision::Ask(_)));
        let write = resolve(
            "write",
            Tier::Write,
            false,
            &cfg(Mode::AlwaysAsk, &[]),
            None,
        );
        assert!(matches!(write, Decision::Ask(_)));
        let exec = resolve("bash", Tier::Exec, false, &cfg(Mode::AlwaysAsk, &[]), None);
        assert!(matches!(exec, Decision::Ask(_)));
        assert_eq!(
            resolve("bash", Tier::Exec, true, &cfg(Mode::Yolo, &[]), None),
            Decision::Auto
        );
    }

    #[test]
    fn readonly_bash_commands_run_free() {
        for cmd in [
            "ls -la",
            "git status",
            "git diff HEAD~1",
            "git log --oneline | head -5",
            "cat a.txt b.txt",
            "rg pattern src/",
            "cargo test --lib",
            "ps aux | grep llm",
            "bash -c 'git status'",
        ] {
            let d = resolve(
                "bash",
                Tier::Exec,
                false,
                &cfg(Mode::AlwaysAsk, &[]),
                Some(cmd),
            );
            assert_eq!(d, Decision::Auto, "{cmd} should run free");
        }
    }

    #[test]
    fn dangerous_bash_commands_ask() {
        for cmd in [
            "rm -rf build",
            "git push origin main",
            "git commit -m x",
            "git checkout -b feat",
            "curl https://x",
            "python script.py",
            "node build.js",
            "cat a.txt > b.txt",
            "sed -i 's/a/b/' f.txt",
            "find . -name x -delete",
            "find . -name x -exec rm {} ;",
            "echo hi | sudo tee /etc/hosts",
            "bash -c 'git push'",
            "npm install",
            "chmod +x run.sh",
        ] {
            let d = resolve(
                "bash",
                Tier::Exec,
                false,
                &cfg(Mode::AlwaysAsk, &[]),
                Some(cmd),
            );
            assert!(matches!(d, Decision::Ask(_)), "{cmd} should ask");
        }
    }

    #[test]
    fn yolo_still_prompts_once_for_destructive_commands() {
        for cmd in [
            "rm -rf build",
            "rm notes.txt",
            "sudo apt install x",
            "mkfs.ext4 /dev/sda1",
            "dd if=img of=/dev/sdb",
            "cat img.iso > /dev/sdb",
            "echo x >>/dev/sda",
            "shutdown now",
            "reboot",
            "echo hi | sudo tee /etc/hosts",
        ] {
            let d = resolve("bash", Tier::Exec, false, &cfg(Mode::Yolo, &[]), Some(cmd));
            assert!(matches!(d, Decision::Ask(_)), "{cmd} must prompt in yolo");
        }
        // the prompt carries no reason line: the command is the explanation
        let d = resolve(
            "bash",
            Tier::Exec,
            false,
            &cfg(Mode::Yolo, &[]),
            Some("rm -rf build"),
        );
        assert_eq!(d, Decision::Ask(String::new()));
        // everything else stays automatic
        for cmd in ["ls", "git status", "cargo test", "git push origin main"] {
            let d = resolve("bash", Tier::Exec, false, &cfg(Mode::Yolo, &[]), Some(cmd));
            assert_eq!(d, Decision::Auto, "{cmd} should stay auto in yolo");
        }
    }

    #[test]
    fn yolo_lets_dev_null_redirects_run_free() {
        // real-world exploration commands from the field: every `2>/dev/null`
        // is stream hygiene, not destruction — these must never prompt
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
            let d = resolve("bash", Tier::Exec, false, &cfg(Mode::Yolo, &[]), Some(cmd));
            assert_eq!(d, Decision::Auto, "{cmd} must run free in yolo");
        }
        // a real device target still prompts (dd of= paths are caught by
        // the program list, not the redirect scan)
        assert!(redirects_into_device("x >/dev/nvme0n1"));
        assert!(redirects_into_device("x > \"/dev/sda\""));
        assert!(!redirects_into_device("dd if=a of=/dev/nvme0n1"));
        assert!(!redirects_into_device("x 2>/dev/null"));
        assert!(!redirects_into_device("x > /dev/stdout"));
    }

    #[test]
    fn readonly_list_rejects_command_substitution_and_flag_writes() {
        for cmd in [
            "echo $(rm -rf x)",
            "cat `rm -rf x`",
            "echo hi | sort -o /tmp/f",
            "date -s 2030-01-01",
        ] {
            assert!(!readonly_command(cmd), "{cmd} is not read-only");
        }
        // bare and flag-only listing forms stay read-only
        assert!(readonly_command("git branch"));
        assert!(readonly_command("git branch -a"));
        assert!(readonly_command("git config user.name"));
        assert!(readonly_command("git remote -v"));
        assert!(readonly_command("sort input.txt"));
    }

    #[test]
    fn mutating_git_forms_are_not_readonly() {
        for cmd in [
            "git branch -D feat",
            "git branch new",
            "git remote add origin url",
            "git remote remove origin",
            "git config user.email a@b.c",
            "git config --global user.name n",
        ] {
            assert!(!readonly_command(cmd), "{cmd} must not run free");
        }
    }

    #[test]
    fn writes_always_ask_in_ask_mode() {
        let d = resolve(
            "write",
            Tier::Write,
            false,
            &cfg(Mode::AlwaysAsk, &[]),
            None,
        );
        assert!(matches!(d, Decision::Ask(_)));
        // an extension allow skips the ask at the gate, not here
        let yolo = resolve("write", Tier::Write, false, &cfg(Mode::Yolo, &[]), None);
        assert_eq!(yolo, Decision::Auto);
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
            None,
        );
        assert!(matches!(deny, Decision::Deny(_)));
        let allow = resolve(
            "bash",
            Tier::Exec,
            false,
            &cfg(Mode::AlwaysAsk, &[("bash", Policy::Allow)]),
            None,
        );
        assert_eq!(allow, Decision::Auto);
        let prompt = resolve(
            "read",
            Tier::Read,
            false,
            &cfg(Mode::Yolo, &[("read", Policy::Prompt)]),
            None,
        );
        assert!(matches!(prompt, Decision::Ask(_)));
    }
}
