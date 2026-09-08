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

/// Executable files in the home directories; project wins by name.
pub fn discover(cwd: &Path) -> Vec<PathBuf> {
    let disabled = crate::core::config::disabled_extensions();
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for dir in discover_dirs(cwd) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() || !is_executable(&path) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if disabled.iter().any(|d| d == stem) || !seen.insert(stem.to_string()) {
                continue;
            }
            out.push(path);
        }
    }
    out
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
            &json!({"id": next_id(), "type": "call_tool", "name": name, "args": args}),
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
            &json!({"id": next_id(), "type": "run_command", "name": name, "args": args}),
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
            &json!({"id": next_id(), "type": "event", "name": name, "params": params}),
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
}

impl Extensions {
    /// Spawn and handshake every discovered extension in parallel; a slow
    /// or broken one costs at most `CONNECT_TIMEOUT` and never aborts the
    /// others — it lands in the list as Failed with a reason.
    pub fn connect(cwd: &Path) -> Extensions {
        let paths = discover(cwd);
        let exts = std::thread::scope(|scope| {
            let handles: Vec<_> = paths
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
        Extensions { exts }
    }

    /// Append one `ExtTool` per tool of every ready extension.
    pub fn mount_tools(&self, out: &mut Vec<Box<dyn Tool>>) {
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

    /// Diagnostics tail for one extension.
    pub fn tail_lines(&self, name: &str) -> Vec<String> {
        self.exts
            .iter()
            .find(|e| e.name == name)
            .map(|ext| lock(&ext.tail).iter().cloned().collect())
            .unwrap_or_default()
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
    dead.store(true, Ordering::Relaxed);
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
