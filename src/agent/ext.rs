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
}

/// Parse the manifest header off a script. A manifest starts at a comment
/// line carrying `--- llm-tool: <name>` (`#` or `//` prefix) and extends
/// over the following comment lines; the first non-comment line ends it.
/// Fields: `description:`, `args: name (type) desc` (repeatable),
/// `arg-mode: argv`, `interpreter: <prog>`, `timeout: <secs>`.
pub fn parse_tool_manifest(text: &str, path: &Path) -> Option<ExecToolSpec> {
    let mut name: Option<String> = None;
    let mut description = String::new();
    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();
    let mut arg_mode_argv = false;
    let mut interpreter: Option<String> = None;
    let mut timeout: Option<u64> = None;
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

pub struct Ext {
    pub name: String,
    target: String,
    state: Mutex<Result<ExtState, String>>,
    tail: Arc<Mutex<VecDeque<String>>>,
    conn: Mutex<Option<Conn>>,
}

impl Ext {
    /// Send one request and await its reply, slicing the wait so ctrl+c
    /// stays responsive. No respawn: a dead extension stays dead until
    /// `/reload`; its tools then error out.
    fn request(&self, msg: &Value, timeout: Duration) -> Result<Value, String> {
        let guard = lock(&self.conn);
        let conn = guard
            .as_ref()
            .ok_or_else(|| format!("extension '{}' is not running (run /reload)", self.name))?;
        if conn.dead.load(Ordering::Relaxed) {
            return Err(format!(
                "extension '{}' is not running (run /reload)",
                self.name
            ));
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
                eprintln!("\x1b[2mextension '{}' failed: {reason}\x1b[0m", ext.name);
            }
        }
        Extensions {
            exts,
            script_tools: found.script_tools,
        }
    }

    /// Append one `ExtTool` per tool of every ready extension, plus one
    /// `ScriptTool` per manifest-carrying script.
    pub fn mount_tools(&self, out: &mut Vec<Box<dyn Tool>>) {
        for spec in &self.script_tools {
            let description = if spec.description.is_empty() {
                format!("Script tool {}", spec.name)
            } else {
                spec.description.clone()
            };
            out.push(Box::new(ScriptTool {
                spec: spec.clone(),
                description,
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
                out.push(Box::new(ExtTool {
                    ext: Arc::clone(ext),
                    tool_name: meta.name.clone(),
                    description,
                    schema: meta.schema.clone(),
                }));
            }
        }
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

fn failed(name: &str, reason: String) -> Arc<Ext> {
    Arc::new(Ext {
        name: name.to_string(),
        target: String::new(),
        state: Mutex::new(Err(reason)),
        tail: Arc::new(Mutex::new(VecDeque::new())),
        conn: Mutex::new(None),
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
        state: Mutex::new(Err("connecting".to_string())),
        tail: Arc::new(Mutex::new(VecDeque::new())),
        conn: Mutex::new(None),
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
            let result = self.request(
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
}

impl Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.spec.name
    }
    fn tier(&self) -> Tier {
        Tier::Exec
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

/// One extension tool mounted into the registry. Exec-tier: the approval
/// matrix asks by default, per-tool policies still win.
struct ExtTool {
    ext: Arc<Ext>,
    tool_name: String,
    description: String,
    schema: Value,
}

impl Tool for ExtTool {
    fn name(&self) -> &str {
        &self.tool_name
    }
    fn tier(&self) -> Tier {
        Tier::Exec
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
}
