//! The extension host: user-built executables that extend the agent with
//! custom tools, slash commands and event hooks (the reference's extensions, done
//! out-of-process). One extension = one executable speaking newline-delimited
//! JSON over stdio:
//!
//! ```text
//! → {"id":1,"type":"initialize","params":{"version":..,"cwd":..}}
//! ← {"id":1,"result":{"tools":[{"name","description","parameters"}],..}}
//! → {"id":2,"type":"call_tool","name":..,"args":{..}}   ← {"id":2,"result":..}
//! → {"id":3,"type":"run_command","name":..,"args":".."} ← {"id":3,"result":".."}
//! → {"id":4,"type":"event","name":"tool_call","params":{..}} ← {"id":4,"result":{..}}
//! → {"type":"shutdown"}                                  (then stdin closes)
//! ```
//!
//! Discovery is two homes, project overriding user by name: `~/.llm/extensions/`
//! and the nearest `.llm/extensions/` walking up from the working directory.
//! An entry is any executable file (shebang decides the language); config
//! `extensions.disabled` skips one by name. Extension tools are Exec-tier —
//! the approval matrix asks by default. stdout carries only the protocol;
//! stderr is dimmed into a diagnostics tail.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::approval::Tier;
use super::tools::{MAX_BYTES, MAX_LINES, Tool, ToolOutput, truncate_tail};

/// Budget for spawn + initialize at startup and on `/reload`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-tool-call timeout (config `extensions.tool_timeout` overrides).
const TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// Event-hook timeout: extensions must be quick at turn boundaries.
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
/// stderr lines kept for diagnostics.
const TAIL_LINES: usize = 20;
/// `recv_timeout` slice; keeps ctrl+c responsive while waiting.
const POLL_SLICE: Duration = Duration::from_millis(100);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ============================================================================
// discovery
// ============================================================================

/// The extension homes, nearest-first within a root: project (walking up
/// from cwd) then user. Same name in both → project wins.
pub fn discover_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // packages: project pkg first, then user pkg (the same nearest-wins
    // ordering the home directories below use)
    dirs.extend(crate::commands::pkg::extension_dirs(true));
    dirs.extend(crate::commands::pkg::extension_dirs(false));
    if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".llm/extensions", true) {
        dirs.push(d);
    }
    dirs.push(crate::core::config::user_dir().join("extensions"));
    dirs
}

/// The plugin-surface roots live-reload watches: every extension root
/// ([`discover_dirs`]) and skill root (the same walk `skills::discover`
/// takes), plus config.json (settings, disabled extensions, tool policies).
pub fn fingerprint_roots(cwd: &Path) -> (Vec<PathBuf>, PathBuf) {
    let mut roots = discover_dirs(cwd);
    roots.extend(crate::commands::pkg::skill_dirs(true));
    roots.extend(crate::commands::pkg::skill_dirs(false));
    let user = crate::core::config::user_dir();
    roots.push(user.join(".agents/skills"));
    roots.push(user.join("skills"));
    if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".agents/skills", true) {
        roots.push(d);
    }
    if let Some(d) = crate::core::paths::nearest_dir_up(cwd, ".llm/skills", true) {
        roots.push(d);
    }
    (roots, crate::core::config::config_path())
}

/// A cheap change probe over the plugin surfaces. The REPL snapshots this
/// at startup and re-runs the /reload path at a task boundary whenever the
/// value moves, so a file the agent (or the user) drops into an extensions
/// or skills dir mid-session is live on the next task — the reference's
/// reload-runtime without the manual step.
pub fn plugin_fingerprint(cwd: &Path) -> u64 {
    let (roots, config) = fingerprint_roots(cwd);
    hash_roots(&roots, &config)
}

fn hash_roots(roots: &[PathBuf], config: &Path) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let stamp = |p: &Path| -> (u64, u64) {
        match std::fs::metadata(p) {
            Ok(m) => (
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                m.len(),
            ),
            Err(_) => (0, 0),
        }
    };
    let mut h = DefaultHasher::new();
    let mut roots: Vec<&PathBuf> = roots.iter().collect();
    roots.sort();
    for root in roots {
        h.write(root.as_os_str().as_encoded_bytes());
        let Ok(rd) = std::fs::read_dir(root) else {
            h.write_u8(0); // absent root: a stable placeholder
            continue;
        };
        // every entry of the root, and — skills live one level down — each
        // subdirectory's immediate children; deeper trees stop mattering
        let mut entries: Vec<(String, u64, u64, Vec<(String, u64)>)> = Vec::new();
        for e in rd.flatten() {
            let (m, len) = stamp(&e.path());
            let mut children = Vec::new();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Ok(sub) = std::fs::read_dir(e.path())
            {
                for c in sub.flatten() {
                    let (cm, _) = stamp(&c.path());
                    children.push((c.file_name().to_string_lossy().into_owned(), cm));
                }
                children.sort();
            }
            entries.push((
                e.file_name().to_string_lossy().into_owned(),
                m,
                len,
                children,
            ));
        }
        entries.sort();
        for (name, m, len, children) in entries {
            h.write(name.as_bytes());
            h.write_u64(m);
            h.write_u64(len);
            for (cn, cm) in children {
                h.write(cn.as_bytes());
                h.write_u64(cm);
            }
        }
    }
    let (cm, clen) = stamp(config);
    h.write(config.as_os_str().as_encoded_bytes());
    h.write_u64(cm);
    h.write_u64(clen);
    h.finish()
}

/// Discovered extension entries, split by form: resident executables
/// (spawned once, speaking the protocol) and manifest script tools (one
/// comment header on any script in any language; the host runs the
/// protocol around each call). Project wins by stem name across both.
pub struct Discovered {
    pub resident: Vec<PathBuf>,
    pub script_tools: Vec<crate::agent::ext::ExecToolSpec>,
}

/// Scan the home directories for extension files.
pub fn discover(cwd: &Path) -> Discovered {
    let disabled = crate::core::config::disabled_extensions();
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Discovered {
        resident: Vec::new(),
        script_tools: Vec::new(),
    };
    for dir in discover_dirs(cwd) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if disabled.iter().any(|d| d == stem) || !seen.insert(stem.to_string()) {
                continue;
            }
            // a `--- llm-tool:` manifest header makes any script a tool —
            // no exec bit needed (the host runs it through the declared
            // interpreter), which also makes the form work on Windows
            if let Some(spec) = exec_tool_manifest(&path) {
                out.script_tools.push(spec);
            } else if is_executable(&path) {
                out.resident.push(path);
            }
        }
    }
    out
}

/// A `--- llm-tool:` manifest parsed from a script's leading comment block.
#[derive(Clone, Debug)]
pub struct ExecToolSpec {
    pub path: PathBuf,
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// single-argument form: the one declared argument arrives as argv[1]
    /// (plain text, no JSON) — shell scripts never need to parse stdin
    pub arg_mode_argv: bool,
    /// explicit interpreter (`# interpreter: python`); None = the file runs
    /// itself (shebang / exec bit / Windows association)
    pub interpreter: Option<String>,
    pub timeout: u64,
    /// declared trust tier (`# tier: write`); defaults to exec
    pub tier: Tier,
}

/// Parse the manifest header off a script. A manifest starts at a comment
/// line carrying `--- llm-tool: <name>` (`#` or `//` prefix) and extends
/// over the following comment lines; the first non-comment line ends it.
/// Fields: `description:`, `args: name (type) desc` (repeatable),
/// `arg-mode: argv`, `interpreter: <prog>`, `timeout: <secs>`,
/// `tier: read|write|exec`.
pub fn parse_tool_manifest(text: &str, path: &Path) -> Option<ExecToolSpec> {
    let mut name: Option<String> = None;
    let mut description = String::new();
    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();
    let mut arg_mode_argv = false;
    let mut interpreter: Option<String> = None;
    let mut timeout: Option<u64> = None;
    let mut tier = Tier::Exec;
    let mut in_header = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        let comment = trimmed
            .strip_prefix('#')
            .or_else(|| trimmed.strip_prefix("//"))
            .map(str::trim);
        let Some(comment) = comment else {
            if in_header {
                break; // first non-comment line ends the header
            }
            continue;
        };
        if !in_header {
            // lines before the marker (shebang, license header) are skipped,
            // not fatal — only a non-comment line ends the scan
            let Some(rest) = comment.strip_prefix("--- llm-tool:") else {
                continue;
            };
            name = Some(rest.trim().to_string());
            in_header = true;
            continue;
        }
        if let Some(rest) = comment.strip_prefix("description:") {
            description = rest.trim().to_string();
        } else if let Some(rest) = comment.strip_prefix("args:") {
            parse_arg_field(rest, &mut properties, &mut required);
        } else if matches!(comment.strip_prefix("arg-mode:"), Some(v) if v.trim() == "argv") {
            arg_mode_argv = true;
        } else if let Some(rest) = comment.strip_prefix("interpreter:") {
            let prog = rest.trim();
            if !prog.is_empty() {
                interpreter = Some(crate::core::config::expand_env(prog));
            }
        } else if let Some(rest) = comment.strip_prefix("timeout:") {
            timeout = rest.trim().parse::<u64>().ok().filter(|t| *t > 0);
        } else if let Some(rest) = comment.strip_prefix("tier:") {
            // an unknown tier stays exec (the safe default)
            tier = Tier::parse(rest).unwrap_or(Tier::Exec);
        }
    }
    Some(ExecToolSpec {
        path: path.to_path_buf(),
        name: name?,
        description,
        schema: json!({
            "type": "object",
            "properties": Value::Object(properties),
            "required": required,
        }),
        arg_mode_argv,
        interpreter,
        timeout: timeout.unwrap_or_else(|| crate::core::config::extension_tool_timeout().as_secs()),
        tier,
    })
}

/// `args: name (type) the description` → one schema property + required.
fn parse_arg_field(
    rest: &str,
    properties: &mut serde_json::Map<String, Value>,
    required: &mut Vec<Value>,
) {
    let Some((name, rest)) = rest.trim().split_once(char::is_whitespace) else {
        return;
    };
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    let (type_, description) = match rest.trim().split_once(char::is_whitespace) {
        Some((t, d)) if t.starts_with('(') && t.ends_with(')') => {
            (&t[1..t.len() - 1], d.trim().to_string())
        }
        _ => ("string", rest.trim().to_string()),
    };
    let type_ = match type_ {
        "int" | "integer" => "integer",
        "number" | "float" => "number",
        "bool" | "boolean" => "boolean",
        "list" | "array" => "array",
        _ => "string",
    };
    let mut prop = serde_json::Map::new();
    prop.insert("type".to_string(), json!(type_));
    if !description.is_empty() {
        prop.insert("description".to_string(), json!(description));
    }
    properties.insert(name.to_string(), Value::Object(prop));
    required.push(json!(name));
}

/// Read a file's head and parse its manifest, if it carries one.
fn exec_tool_manifest(path: &Path) -> Option<ExecToolSpec> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 4096];
    let n = f.read(&mut head).unwrap_or(0);
    let text = String::from_utf8_lossy(&head[..n]);
    parse_tool_manifest(&text, path).filter(|s| !s.name.is_empty())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("exe") | Some("bat") | Some("cmd") | Some("ps1")
    )
}

// ============================================================================
// one extension process
// ============================================================================

/// One tool advertised by the initialize handshake.
#[derive(Clone)]
pub struct ToolMeta {
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// declared trust tier; an absent or unknown value stays exec
    pub tier: Tier,
}

pub struct ExtState {
    /// advertised tools, commands and event subscriptions
    tools: Vec<ToolMeta>,
    commands: Vec<String>,
    events: Vec<String>,
    /// how long tool calls wait before the extension is dropped
    tool_timeout: Duration,
}

/// Request-id → waiter map: the reader thread delivers replies here.
type PendingMap = Arc<Mutex<HashMap<u64, SyncSender<Result<Value, String>>>>>;

/// The live child plus everything its reader thread needs without touching
/// `Ext` (threads must not keep the host alive).
struct Conn {
    child: Child,
    writer: Sender<String>,
    pending: PendingMap,
    dead: Arc<AtomicBool>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        // a polite shutdown first: the extension may want to flush state
        let _ = self.writer.send("{\"type\":\"shutdown\"}\n".to_string());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Conn {
    /// Has the child exited? The reader thread marks `dead` only once it sees
    /// EOF, which can lag the process by a moment — and a request written
    /// into that gap waits out the whole timeout instead of respawning.
    /// Asking the OS directly closes it.
    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

pub struct Ext {
    pub name: String,
    target: String,
    /// the discovered file; `None` for a placeholder that never connected
    path: Option<PathBuf>,
    state: Mutex<Result<ExtState, String>>,
    tail: Arc<Mutex<VecDeque<String>>>,
    conn: Mutex<Option<Conn>>,
    /// serializes respawns: two concurrent tool calls on a dead extension
    /// must not race two children into the same slot
    respawn: Mutex<()>,
}

impl Ext {
    /// Bring a dead extension back before using it: a crash or an exit no
    /// longer kills the tool until `/reload`. Runs at most one spawn per
    /// request; if the respawn itself fails, `state` carries the reason.
    fn ensure_alive(&self) -> Result<(), String> {
        if self.is_alive() {
            return Ok(());
        }
        if self.path.is_none() {
            return Err(format!("extension '{}' is not running", self.name));
        }
        self.respawn();
        match &*lock(&self.state) {
            Ok(_) => Ok(()),
            Err(reason) => Err(reason.clone()),
        }
    }

    fn is_alive(&self) -> bool {
        let mut guard = lock(&self.conn);
        let Some(conn) = guard.as_mut() else {
            return false;
        };
        if conn.dead.load(Ordering::Relaxed) {
            return false;
        }
        if conn.exited() {
            conn.dead.store(true, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Spawn a fresh child under the respawn lock, re-checking liveness after
    /// acquiring it (another caller may have finished the job while we
    /// waited). A no-op for a placeholder that never had a path.
    fn respawn(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let _serialize = lock(&self.respawn);
        if !self.is_alive() {
            self.spawn_and_handshake(&path);
        }
    }

    /// The child can die between `ensure_alive`'s liveness check and the
    /// write — the reader thread marks `dead` a moment later — so a call that
    /// fails on a broken pipe gets one respawn and retry instead of leaking
    /// the race to the model.
    fn recoverable(error: &str) -> bool {
        error.contains("closed its stdout")
            || error.contains("pipe closed")
            || error.contains("is not running")
    }

    /// Send one request and await its reply, slicing the wait so ctrl+c
    /// stays responsive. The connection is revived on demand first.
    fn request(&self, msg: &Value, timeout: Duration) -> Result<Value, String> {
        self.ensure_alive()?;
        match self.request_live(msg, timeout) {
            Err(e) if self.should_retry(&e) => {
                self.respawn();
                match &*lock(&self.state) {
                    Ok(_) => self.request_live(msg, timeout),
                    Err(reason) => Err(reason.clone()),
                }
            }
            other => other,
        }
    }

    /// A call that failed because the child died is worth one respawn and a
    /// retry: the pipe was closed, the process was gone, or the reply never
    /// came *because* the process had exited. A live-but-slow extension keeps
    /// its single timeout instead, so a slow tool is never run twice.
    fn should_retry(&self, error: &str) -> bool {
        self.path.is_some() && (Self::recoverable(error) || !self.is_alive())
    }

    /// The transport half of `request`, for callers that already hold a
    /// live connection (the handshake itself: respawning from inside
    /// `ensure_alive` would re-enter the respawn lock).
    fn request_live(&self, msg: &Value, timeout: Duration) -> Result<Value, String> {
        let mut guard = lock(&self.conn);
        let conn = guard
            .as_mut()
            .ok_or_else(|| format!("extension '{}' is not running", self.name))?;
        if conn.dead.load(Ordering::Relaxed) {
            return Err(format!("extension '{}' is not running", self.name));
        }
        let id = msg
            .get("id")
            .and_then(Value::as_u64)
            .expect("host messages always carry an id");
        let (tx, rx) = sync_channel(1);
        lock(&conn.pending).insert(id, tx);
        let mut frame = serde_json::to_string(msg).unwrap_or_default();
        frame.push('\n');
        if conn.writer.send(frame).is_err() {
            lock(&conn.pending).remove(&id);
            return Err(format!("extension '{}' pipe closed", self.name));
        }
        let deadline = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(POLL_SLICE) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if crate::core::http::interrupted() {
                        lock(&conn.pending).remove(&id);
                        return Err("interrupted".to_string());
                    }
                    // the process can vanish without the reader having said
                    // so yet: stop waiting, and let `request` respawn instead
                    // of burning the full timeout on a corpse
                    if conn.exited() {
                        lock(&conn.pending).remove(&id);
                        return Err(format!("extension '{}' is not running", self.name));
                    }
                    if Instant::now() >= deadline {
                        lock(&conn.pending).remove(&id);
                        return Err(format!(
                            "extension '{}' timed out after {}s",
                            self.name,
                            timeout.as_secs()
                        ));
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(format!("extension '{}' closed its stdout", self.name));
                }
            }
        }
    }

    /// Run one extension tool: the result string is the tool output.
    pub fn call_tool(&self, name: &str, args: &Value) -> Result<String, String> {
        let timeout = match &*lock(&self.state) {
            Ok(state) => state.tool_timeout,
            Err(_) => TOOL_TIMEOUT,
        };
        let result = self.request(
            &json!({"id": next_id(), "type": "call_tool", "v": 1, "name": name, "args": args}),
            timeout,
        )?;
        Ok(match result.get("result") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => crate::jsonfmt::dumps_indent(other, 2),
            None => String::new(),
        })
    }

    /// Run one extension-registered slash command; the reply prints.
    pub fn run_command(&self, name: &str, args: &str) -> Result<String, String> {
        let result = self.request(
            &json!({"id": next_id(), "type": "run_command", "v": 1, "name": name, "args": args}),
            TOOL_TIMEOUT,
        )?;
        Ok(result
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// Fire an event hook. `tool_call` expects a reply — the extension may
    /// deny the call (`{"decision":"deny","reason":..}`) or rewrite its
    /// arguments (`{"args":{..}}`); every other event is fire-and-forget.
    pub fn fire(&self, name: &str, params: &Value) -> Result<Option<Value>, String> {
        let subscribed = lock(&self.state)
            .as_ref()
            .is_ok_and(|s| s.events.iter().any(|e| e == name));
        if !subscribed {
            return Ok(None);
        }
        let result = self.request(
            &json!({"id": next_id(), "type": "event", "v": 1, "name": name, "params": params}),
            EVENT_TIMEOUT,
        )?;
        Ok(result.get("result").cloned())
    }

    /// A dim diagnostics line (extension errors at event boundaries).
    fn note(&self, line: String) {
        let mut tail = lock(&self.tail);
        if tail.len() >= TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}

// ============================================================================
// the host
// ============================================================================

pub struct Extensions {
    exts: Vec<Arc<Ext>>,
    script_tools: Vec<ExecToolSpec>,
}

impl Extensions {
    /// Spawn and handshake every discovered extension in parallel; a slow
    /// or broken one costs at most `CONNECT_TIMEOUT` and never aborts the
    /// others — it lands in the list as Failed with a reason.
    pub fn connect(cwd: &Path) -> Extensions {
        let found = discover(cwd);
        let exts = std::thread::scope(|scope| {
            let handles: Vec<_> = found
                .resident
                .iter()
                .map(|p| scope.spawn(|| connect_one(p)))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        failed("extension", "connect thread panicked".to_string())
                    })
                })
                .collect::<Vec<_>>()
        });
        for ext in &exts {
            if let Err(reason) = &*lock(&ext.state) {
                eprintln!(
                    "{}extension '{}' failed: {reason}{}",
                    crate::theme::err().dim,
                    ext.name,
                    crate::theme::err().reset
                );
            }
        }
        Extensions {
            exts,
            script_tools: found.script_tools,
        }
    }

    /// Append one `ExtTool` per tool of every ready extension, plus one
    /// `ScriptTool` per manifest-carrying script. Names are flat across the
    /// whole registry (builtins included), so a collision — the obvious
    /// case is two extensions both shipping a `search` tool — would silently
    /// shadow one of them; instead the later tool is namespaced to
    /// `<stem>__<tool>`, which its own `call_tool` never sees (the wire name
    /// stays what the extension advertised).
    pub fn mount_tools(&self, out: &mut Vec<Box<dyn Tool>>) {
        let mut taken: std::collections::HashSet<String> =
            out.iter().map(|t| t.name().to_string()).collect();
        for spec in &self.script_tools {
            let description = if spec.description.is_empty() {
                format!("Script tool {}", spec.name)
            } else {
                spec.description.clone()
            };
            let exposed = unique_tool_name(&spec.name, "script", &mut taken);
            out.push(Box::new(ScriptTool {
                spec: spec.clone(),
                description,
                exposed,
            }));
        }
        for ext in &self.exts {
            let Ok(state) = &*lock(&ext.state) else {
                continue;
            };
            for meta in &state.tools {
                let description = if meta.description.is_empty() {
                    format!("Extension tool {} from {}", meta.name, ext.name)
                } else {
                    format!("{} (extension: {})", meta.description, ext.name)
                };
                let exposed = unique_tool_name(&meta.name, &ext.name, &mut taken);
                out.push(Box::new(ExtTool {
                    ext: Arc::clone(ext),
                    tool_name: meta.name.clone(),
                    exposed,
                    description,
                    schema: meta.schema.clone(),
                    tier: meta.tier,
                }));
            }
        }
    }

    /// Does any ready extension subscribe to this event? Callers use it to
    /// skip building a large payload (a full tool result) when nobody will
    /// read it.
    pub fn subscribes(&self, name: &str) -> bool {
        self.exts.iter().any(|ext| {
            lock(&ext.state)
                .as_ref()
                .is_ok_and(|s| s.events.iter().any(|e| e == name))
        })
    }

    /// Fire `tool_result` and return a replacement for the model-visible
    /// result when an extension offers one (`{"content": ".."}`). The event
    /// carries the tool's full content, so merely subscribing is the opt-in
    /// to receive — and be trusted with — it; a reply without `content`
    /// observes only. Last rewrite wins, and the whole path is fail-open: a
    /// dead, slow, silent or malformed extension leaves the tool's own
    /// result exactly as produced. The caller re-caps the replacement.
    pub fn rewrite_tool_result(&self, params: &Value) -> Option<String> {
        let mut replacement: Option<String> = None;
        for ext in &self.exts {
            match ext.fire("tool_result", params) {
                Ok(Some(reply)) => {
                    if let Some(content) = reply.get("content").and_then(Value::as_str) {
                        replacement = Some(content.to_string());
                    }
                }
                Ok(None) => {}
                Err(e) => ext.note(e),
            }
        }
        replacement
    }

    /// Fire an event on every extension subscribed to it. `tool_call` may
    /// deny (Err carries the reason) or rewrite the arguments (Ok(Some)).
    pub fn fire(&self, name: &str, params: &Value) -> Result<Option<Value>, String> {
        let mut outcome: Option<Value> = None;
        for ext in &self.exts {
            match ext.fire(name, params) {
                Ok(reply) => {
                    if let Some(reply) = reply {
                        if reply.get("decision").and_then(Value::as_str) == Some("deny") {
                            let reason = reply
                                .get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or("denied by extension");
                            return Err(reason.to_string());
                        }
                        if let Some(args) = reply.get("args") {
                            outcome = Some(args.clone());
                        }
                    }
                }
                Err(e) => ext.note(e),
            }
        }
        Ok(outcome)
    }

    /// The extension that registered a slash command, if any.
    pub fn command_owner(&self, name: &str) -> Option<Arc<Ext>> {
        self.exts
            .iter()
            .find(|ext| {
                lock(&ext.state)
                    .as_ref()
                    .is_ok_and(|s| s.commands.iter().any(|c| c == name))
            })
            .cloned()
    }

    /// Every slash command any ready extension registered, for completion.
    pub fn command_names(&self) -> Vec<String> {
        self.exts
            .iter()
            .filter_map(|ext| lock(&ext.state).as_ref().ok().map(|s| s.commands.clone()))
            .flatten()
            .collect()
    }

    /// Listing for `/tools`: (name, target, tools, commands, reason).
    pub fn rows(&self) -> Vec<(String, String, usize, usize, String)> {
        self.exts
            .iter()
            .map(|ext| match &*lock(&ext.state) {
                Ok(state) => (
                    ext.name.clone(),
                    ext.target.clone(),
                    state.tools.len(),
                    state.commands.len(),
                    String::new(),
                ),
                Err(reason) => (ext.name.clone(), ext.target.clone(), 0, 0, reason.clone()),
            })
            .collect()
    }
}

/// Claim a registry-wide tool name, namespacing with the owner when the
/// plain name is already taken (`search` → `websearch__search`); a rebuilt
/// registry is the only caller, so this needs no global state.
fn unique_tool_name(
    wanted: &str,
    owner: &str,
    taken: &mut std::collections::HashSet<String>,
) -> String {
    if taken.insert(wanted.to_string()) {
        return wanted.to_string();
    }
    let base = format!("{owner}__{wanted}");
    let mut candidate = base.clone();
    let mut n = 2;
    while !taken.insert(candidate.clone()) {
        candidate = format!("{base}{n}");
        n += 1;
    }
    candidate
}

fn failed(name: &str, reason: String) -> Arc<Ext> {
    Arc::new(Ext {
        name: name.to_string(),
        target: String::new(),
        path: None,
        state: Mutex::new(Err(reason)),
        tail: Arc::new(Mutex::new(VecDeque::new())),
        conn: Mutex::new(None),
        respawn: Mutex::new(()),
    })
}

fn connect_one(path: &Path) -> Arc<Ext> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("extension")
        .to_string();
    let ext = Arc::new(Ext {
        name,
        target: path.display().to_string(),
        path: Some(path.to_path_buf()),
        state: Mutex::new(Err("connecting".to_string())),
        tail: Arc::new(Mutex::new(VecDeque::new())),
        conn: Mutex::new(None),
        respawn: Mutex::new(()),
    });
    ext.spawn_and_handshake(path);
    ext
}

impl Ext {
    /// Spawn the process, install the connection, then run the initialize
    /// handshake. Any failure downgrades the state; the tool list is empty.
    fn spawn_and_handshake(&self, path: &Path) {
        let outcome = (|| -> Result<(), String> {
            let conn = self.spawn_conn(path)?;
            *lock(&self.conn) = Some(conn);
            let result = self.request_live(
                &json!({
                    "id": next_id(),
                    "type": "initialize",
                    "v": 1,
                    "params": {
                        "version": env!("CARGO_PKG_VERSION"),
                        "cwd": std::env::current_dir()
                            .unwrap_or_else(|_| Path::new(".").to_path_buf())
                            .display()
                            .to_string()
                    },
                }),
                CONNECT_TIMEOUT,
            )?;
            let result = result
                .get("result")
                .ok_or_else(|| "initialize reply carries no result".to_string())?;
            let tools = parse_tools(result);
            let commands = result
                .get("commands")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let events = result
                .get("events")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let tool_timeout = crate::core::config::extension_tool_timeout();
            *lock(&self.state) = Ok(ExtState {
                tools,
                commands,
                events,
                tool_timeout,
            });
            Ok(())
        })();
        if let Err(reason) = outcome {
            *lock(&self.conn) = None; // drop the child
            *lock(&self.state) = Err(reason);
        }
    }

    fn spawn_conn(&self, path: &Path) -> Result<Conn, String> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        let mut command = Command::new(path);
        command
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot spawn {}: {e}", path.display()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "no stdin pipe".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "no stdout pipe".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "no stderr pipe".to_string())?;

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || writer_loop(stdin, rx));

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(AtomicBool::new(false));

        let reader_pending = Arc::clone(&pending);
        let reader_dead = Arc::clone(&dead);
        let reader_tail = Arc::clone(&self.tail);
        std::thread::spawn(move || {
            reader_loop(stdout, &reader_pending, &reader_dead, &reader_tail)
        });

        let stderr_tail = Arc::clone(&self.tail);
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                let mut t = lock(&stderr_tail);
                if t.len() >= TAIL_LINES {
                    t.pop_front();
                }
                t.push_back(line);
            }
        });

        Ok(Conn {
            child,
            writer: tx,
            pending,
            dead,
        })
    }
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn parse_tools(result: &Value) -> Vec<ToolMeta> {
    result
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| {
                    let name = t.get("name").and_then(Value::as_str)?;
                    Some(ToolMeta {
                        name: name.to_string(),
                        description: t
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        schema: t
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                        tier: t
                            .get("tier")
                            .and_then(Value::as_str)
                            .and_then(Tier::parse)
                            .unwrap_or(Tier::Exec),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One manifest-carrying script mounted as a tool: the host runs the
/// protocol around each call (spawn, feed arguments, collect output,
/// timeout), the script itself only prints its result.
struct ScriptTool {
    spec: ExecToolSpec,
    description: String,
    /// registry name; differs from `spec.name` only on a collision
    exposed: String,
}

impl Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.exposed
    }
    fn tier(&self) -> Tier {
        self.spec.tier
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        self.spec.schema.clone()
    }
    fn preview(&self, args: &Value) -> String {
        super::tools::args_preview(&self.spec.name, args)
    }
    fn execute(&self, args: &Value, cwd: &Path, log: &mut dyn FnMut(&str)) -> ToolOutput {
        let mut command = match self.spec.interpreter.as_deref() {
            Some(prog) => {
                let mut c = std::process::Command::new(prog);
                c.arg(&self.spec.path);
                c
            }
            None => std::process::Command::new(&self.spec.path),
        };
        command
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // argv mode: the single declared argument rides as plain argv[1],
        // so shell scripts never need to parse JSON
        let stdin_payload = if self.spec.arg_mode_argv {
            let value = args
                .as_object()
                .and_then(|o| o.values().next())
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => crate::jsonfmt::dumps_indent(other, 0),
                })
                .unwrap_or_default();
            command.arg(value);
            None
        } else {
            Some(format!(
                "{}\n",
                serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string())
            ))
        };
        let outcome = crate::platform::run_with_progress(
            command,
            stdin_payload.as_deref(),
            self.spec.timeout,
            crate::core::http::interrupt_flag(),
            log,
        );
        if outcome.interrupted {
            return ToolOutput::err("script tool interrupted");
        }
        if outcome.timed_out {
            return ToolOutput::err(format!(
                "script tool timed out after {}s: {}",
                self.spec.timeout,
                self.spec.path.display()
            ));
        }
        super::tools::finish_process_output(outcome.stdout, outcome.stderr, outcome.code)
    }
}

// ============================================================================
// IO loops (the MCP client's shape)
// ============================================================================

fn writer_loop(mut stdin: ChildStdin, rx: Receiver<String>) {
    while let Ok(line) = rx.recv() {
        if stdin.write_all(line.as_bytes()).is_err() {
            return;
        }
        let _ = stdin.flush();
    }
}

/// Read reply lines, correlate by id, dim non-matching lines into the tail.
fn reader_loop(
    stdout: impl std::io::Read,
    pending: &PendingMap,
    dead: &AtomicBool,
    tail: &Mutex<VecDeque<String>>,
) {
    let reader = BufReader::new(stdout);
    for line in reader.lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            let mut t = lock(tail);
            if t.len() >= TAIL_LINES {
                t.pop_front();
            }
            t.push_back(line);
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            let mut t = lock(tail);
            if t.len() >= TAIL_LINES {
                t.pop_front();
            }
            t.push_back(line);
            continue;
        };
        let sender = lock(pending).remove(&id);
        if let Some(tx) = sender {
            if value.get("error").is_some() {
                let _ = tx.send(Err(value["error"]
                    .as_str()
                    .unwrap_or("extension error")
                    .to_string()));
            } else {
                let _ = tx.send(Ok(value));
            }
        }
    }
    // the process is gone: fail everything still waiting (the pending map
    // holds a sender clone, so waiters alone would never see a disconnect)
    dead.store(true, Ordering::Relaxed);
    for (_, tx) in lock(pending).drain() {
        let _ = tx.send(Err("extension closed its stdout".to_string()));
    }
}

// ============================================================================
// the Tool wrapper
// ============================================================================

/// One extension tool mounted into the registry. The extension declares its
/// tier (`initialize`, or `# tier:` in a script manifest); an absent or
/// unknown value stays `exec`, and per-tool policies still win.
struct ExtTool {
    ext: Arc<Ext>,
    /// the name the extension knows over the wire
    tool_name: String,
    /// registry name; differs from `tool_name` only on a collision
    exposed: String,
    description: String,
    schema: Value,
    tier: Tier,
}

impl Tool for ExtTool {
    fn name(&self) -> &str {
        &self.exposed
    }
    fn tier(&self) -> Tier {
        self.tier
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        self.schema.clone()
    }
    fn preview(&self, args: &Value) -> String {
        super::tools::args_preview(&self.tool_name, args)
    }
    fn execute(&self, args: &Value, _cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        match self.ext.call_tool(&self.tool_name, args) {
            Ok(text) => {
                if text.len() > MAX_BYTES {
                    let (capped, _) = truncate_tail(&text, MAX_LINES, MAX_BYTES);
                    ToolOutput::ok(capped)
                } else {
                    ToolOutput::ok(text)
                }
            }
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_python_manifest_header() {
        let text = "#!/usr/bin/env python3\n# --- llm-tool: wordcount\n# description: count characters\n# args: text (string) the text\n# arg-mode: argv\n# interpreter: python\nimport sys\n";
        let spec = parse_tool_manifest(text, Path::new("/x/wordcount")).expect("manifest");
        assert_eq!(spec.name, "wordcount");
        assert_eq!(spec.description, "count characters");
        assert!(spec.arg_mode_argv);
        assert_eq!(spec.interpreter.as_deref(), Some("python"));
        assert_eq!(spec.schema["properties"]["text"]["type"], json!("string"));
        assert_eq!(spec.schema["required"][0], json!("text"));
    }

    #[test]
    fn plain_scripts_have_no_manifest() {
        assert!(parse_tool_manifest("#!/bin/sh\necho hi\n", Path::new("/x/s")).is_none());
    }

    #[test]
    fn manifest_tier_defaults_to_exec_and_parses_declarations() {
        let base = "#!/bin/sh\n# --- llm-tool: t\n";
        let plain = parse_tool_manifest(base, Path::new("/x/t")).expect("manifest");
        assert_eq!(plain.tier, Tier::Exec, "absent tier stays exec");
        let read = parse_tool_manifest(&format!("{base}# tier: read\n"), Path::new("/x/t"))
            .expect("manifest");
        assert_eq!(read.tier, Tier::Read);
        let write = parse_tool_manifest(&format!("{base}# tier: Write\n"), Path::new("/x/t"))
            .expect("manifest");
        assert_eq!(write.tier, Tier::Write, "case-insensitive");
        let bogus = parse_tool_manifest(&format!("{base}# tier: whatever\n"), Path::new("/x/t"))
            .expect("manifest");
        assert_eq!(
            bogus.tier,
            Tier::Exec,
            "an unknown tier must not lower trust"
        );
    }

    #[test]
    fn initialize_tools_carry_their_declared_tier() {
        let result = json!({"tools": [
            {"name": "safe", "tier": "read", "parameters": {}},
            {"name": "writer", "tier": "WRITE", "parameters": {}},
            {"name": "shell", "tier": "exec", "parameters": {}},
            {"name": "unlabeled", "parameters": {}},
            {"name": "nonsense", "tier": "nope", "parameters": {}},
        ]});
        let tiers: Vec<Tier> = parse_tools(&result).iter().map(|t| t.tier).collect();
        assert_eq!(
            tiers,
            vec![Tier::Read, Tier::Write, Tier::Exec, Tier::Exec, Tier::Exec]
        );
    }

    #[test]
    fn colliding_tool_names_are_namespaced_not_shadowed() {
        let mut taken: std::collections::HashSet<String> =
            ["read".to_string()].into_iter().collect();
        // a built-in already holds the plain name: the extension keeps its
        // own identity via the owner prefix
        assert_eq!(
            unique_tool_name("read", "websearch", &mut taken),
            "websearch__read"
        );
        // a second extension with the same tool gets its own prefix too
        assert_eq!(unique_tool_name("read", "todo", &mut taken), "todo__read");
        // an unprefixed name is untouched
        assert_eq!(
            unique_tool_name("deploy", "websearch", &mut taken),
            "deploy"
        );
        // ...and is then itself reserved
        assert_eq!(
            unique_tool_name("deploy", "todo", &mut taken),
            "todo__deploy"
        );
    }

    /// A resident extension that dies between calls must come back on the
    /// next use. The script exits right after the first `call_tool`, so the
    /// second call exercises the respawn path end to end via real stdio.
    #[cfg(unix)]
    #[test]
    fn a_dead_extension_is_respawned_on_next_use() {
        let dir = std::env::temp_dir().join(format!("llm-ext-respawn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("flaky");
        let counter = dir.join("count");
        // plain shell: reply to initialize, answer the first call_tool then
        // exit (simulating a crash), answering further calls needs a respawn
        let body = format!(
            r#"#!/bin/sh
c=$(cat {counter} 2>/dev/null || echo 0)
c=$((c+1))
echo $c > {counter}
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *initialize*)
      printf '{{"id":%s,"result":{{"tools":[{{"name":"ping","parameters":{{}}}}]}}}}\n' "$id"
      ;;
    *call_tool*)
      printf '{{"id":%s,"result":"alive"}}\n' "$id"
      if [ "$c" -le 1 ]; then exit 1; fi
      ;;
  esac
done
"#,
            counter = counter.display()
        );
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let ext = connect_stub(&script);
        assert!(
            lock(&ext.state).is_ok(),
            "initial handshake must succeed; state says {:?}",
            lock(&ext.state).as_ref().err()
        );
        assert_eq!(ext.call_tool("ping", &json!({})).unwrap(), "alive");
        // the child exited after answering: the next call must respawn it
        let out = ext.call_tool("ping", &json!({}));
        assert_eq!(out.unwrap(), "alive", "respawned, not an error");
        let n: u32 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(n >= 2, "the script must have run twice, saw {n}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_tracks_dropped_and_edited_files() {
        let dir = std::env::temp_dir().join(format!("llm-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.json");
        std::fs::write(&config, b"{}").unwrap();
        let root = dir.join("extensions");
        std::fs::create_dir_all(&root).unwrap();

        let h1 = hash_roots(&[root.clone()], &config);
        assert_eq!(
            h1,
            hash_roots(&[root.clone()], &config),
            "stable with no changes"
        );

        std::fs::write(root.join("tool.py"), b"# --- llm-tool: t\n").unwrap();
        let h3 = hash_roots(&[root.clone()], &config);
        assert_ne!(h3, h1, "a dropped file moves the fingerprint");

        std::fs::write(root.join("tool.py"), b"# --- llm-tool: t2\n").unwrap();
        let h4 = hash_roots(&[root.clone()], &config);
        assert_ne!(h4, h3, "an edit moves it too");

        // skills live one level down: a SKILL.md in a subdirectory counts
        let sk = dir.join("skills");
        std::fs::create_dir_all(sk.join("mine")).unwrap();
        let h5 = hash_roots(&[root.clone(), sk.clone()], &config);
        std::fs::write(sk.join("mine/SKILL.md"), b"x").unwrap();
        assert_ne!(
            hash_roots(&[root.clone(), sk], &config),
            h5,
            "a subdirectory file moves it"
        );

        std::fs::write(&config, b"{\"a\":1}").unwrap();
        let h7 = hash_roots(&[root.clone()], &config);
        std::fs::write(&config, b"{\"a\":1,\"b\":2}").unwrap();
        assert_ne!(hash_roots(&[root], &config), h7, "a config edit moves it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write an executable `sh` stub into a fresh temp dir and return its
    /// path. Discovery is bypassed on purpose: the test connects the script
    /// directly, so it never reads the real `~/.llm` (which would make the
    /// test non-hermetic). Plain `sh` like the respawn test above — a python
    /// stub would start far heavier under a fully parallel test run.
    #[cfg(unix)]
    fn write_stub_extension(name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("llm-ext-test-{}", crate::core::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A resident extension that answers protocol lines: it advertises
    /// `tool_result` and rewrites any result containing ORIGINAL, answering
    /// `null` (observe only) otherwise.
    #[cfg(unix)]
    const REWRITER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *initialize*) printf '{"id":%s,"result":{"events":["tool_result"]}}\n' "$id" ;;
    *ORIGINAL*) printf '{"id":%s,"result":{"content":"REWRITTEN log"}}\n' "$id" ;;
    *) printf '{"id":%s,"result":null}\n' "$id" ;;
  esac
done
"#;

    /// Connect a stub that was just written. `exec` on a file some other
    /// thread's forked child still holds the inherited write fd for fails
    /// with ETXTBSY ("Text file busy") — a real hazard only because a fully
    /// parallel test run forks constantly. The retry fires on that message
    /// alone, so a genuinely broken stub fails exactly as it would without
    /// it; the reason still rides the assertion below.
    #[cfg(unix)]
    fn connect_stub(path: &Path) -> Arc<Ext> {
        let mut ext = connect_one(path);
        for _ in 0..20 {
            let busy = matches!(lock(&ext.state).as_ref(), Err(e) if e.contains("Text file busy"));
            if !busy {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            ext = connect_one(path);
        }
        ext
    }

    #[test]
    #[cfg(unix)]
    fn a_tool_result_subscriber_can_replace_the_result() {
        let host = Extensions {
            exts: vec![connect_stub(&write_stub_extension("rewriter", REWRITER))],
            script_tools: Vec::new(),
        };
        assert!(
            host.subscribes("tool_result"),
            "handshake advertised it; state says {:?}",
            lock(&host.exts[0].state).as_ref().err()
        );
        assert!(!host.subscribes("turn_end"), "only the advertised event");

        let params = json!({
            "tool": "bash", "summary": "s", "is_error": false, "content": "ORIGINAL log"
        });
        assert_eq!(
            host.rewrite_tool_result(&params).as_deref(),
            Some("REWRITTEN log"),
            "a content reply replaces the model-visible result"
        );
        // observe-only: no content in the reply leaves the tool's result alone
        let untouched =
            json!({"tool": "bash", "summary": "s", "is_error": false, "content": "plain"});
        assert_eq!(host.rewrite_tool_result(&untouched), None);
    }

    #[test]
    #[cfg(unix)]
    fn a_dead_extension_cannot_swallow_a_tool_result() {
        // exits the moment an event arrives: the request fails, and the
        // failure must leave the result untouched (fail-open)
        let crashes = "#!/bin/sh\nwhile IFS= read -r line; do\n  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')\n  case \"$line\" in\n    *initialize*) printf '{\"id\":%s,\"result\":{\"events\":[\"tool_result\"]}}\\n' \"$id\" ;;\n    *event*) exit 1 ;;\n  esac\ndone\n";
        let host = Extensions {
            exts: vec![connect_stub(&write_stub_extension("crashy", crashes))],
            script_tools: Vec::new(),
        };
        let params = json!({"tool": "bash", "summary": "s", "is_error": false, "content": "x"});
        assert_eq!(
            host.rewrite_tool_result(&params),
            None,
            "a crashed extension falls back to the tool's own result"
        );
    }
}
