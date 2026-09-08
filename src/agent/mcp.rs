//! MCP stdio client: config-declared servers mounted as agent tools.
//!
//! Servers speak newline-delimited JSON-RPC 2.0 over stdio (the MCP spec's
//! framing; the official TS/Python SDK servers all use it — the reference-agent shares
//! an LSP-style Content-Length client instead, which real servers reject).
//! Config is consent: a server named in config.json may spawn, there is no
//! trust store. Tools mount as `mcp__<server>__<tool>`, are Exec-tier (the
//! approval matrix asks by default) and ride the same chrome as built-ins.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::approval::Tier;
use super::tools::{MAX_BYTES, MAX_LINES, Tool, ToolOutput, truncate_tail};
use crate::core::config::expand_env;

/// MCP protocol revision this client speaks.
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Per-request timeout once connected.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Budget for spawn + initialize + tools/list at connect time.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// stderr/notification lines kept for `/mcp` diagnostics.
const TAIL_LINES: usize = 20;
/// Cap on a mounted tool name (provider schemas reject longer names).
const MAX_MOUNTED_NAME: usize = 64;
/// `recv_timeout` slice; keeps ctrl+c responsive while waiting.
const POLL_SLICE: Duration = Duration::from_millis(100);

/// Request-id → waiter map: the reader thread delivers replies here.
type PendingMap = Arc<Mutex<HashMap<u64, SyncSender<Result<Value, String>>>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ============================================================================
// config
// ============================================================================

#[derive(Clone)]
pub struct ServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub fn load() -> Vec<ServerSpec> {
    let Some(table) = crate::core::config::table("mcpServers") else {
        return Vec::new();
    };
    parse_table(&table)
}

/// Read the `mcpServers` table from a raw JSON config blob (test entry).
#[cfg(test)]
pub fn parse(raw: &str) -> Vec<ServerSpec> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let Some(table) = value.get("mcpServers").and_then(Value::as_object) else {
        return Vec::new();
    };
    parse_table(table)
}

/// Read the `mcpServers` table: map iteration order is the config file's
/// written order (serde_json's preserve-order feature), so mounting is
/// deterministic. The ambient environment is inherited and explicit `env`
/// entries (after `${VAR}` expansion) override it — a deliberate deviation
/// from the allowlist approach, documented in README.
fn parse_table(table: &serde_json::Map<String, Value>) -> Vec<ServerSpec> {
    let mut out = Vec::new();
    for (name, def) in table {
        let Some(command) = def.get("command").and_then(Value::as_str) else {
            if def.get("url").is_some() {
                eprintln!(
                    "Warning: mcp server '{name}' has a url; http servers are not supported yet, skipped"
                );
            } else {
                eprintln!("Warning: mcp server '{name}' has no command; skipped");
            }
            continue;
        };
        out.push(ServerSpec {
            name: name.clone(),
            command: expand_env(command),
            args: def
                .get("args")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(expand_env).collect())
                .unwrap_or_default(),
            env: def
                .get("env")
                .and_then(Value::as_object)
                .map(|e| {
                    e.iter()
                        .map(|(k, v)| (k.clone(), expand_env(v.as_str().unwrap_or_default())))
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    out
}

// ============================================================================
// registry
// ============================================================================

/// One advertised tool from `tools/list`.
#[derive(Clone)]
pub struct ToolMeta {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

pub enum ServerState {
    /// Handshake done, tool list known.
    Ready { tools: Vec<ToolMeta> },
    /// Spawn or handshake failed; its tools are not mounted.
    Failed { reason: String },
}

pub struct McpServer {
    spec: ServerSpec,
    state: Mutex<ServerState>,
    tail: Arc<Mutex<VecDeque<String>>>,
    conn: Mutex<Option<Conn>>,
}

/// The live child plus everything its reader thread needs to reach without
/// touching `McpServer` itself (threads must not keep the registry alive).
struct Conn {
    child: Child,
    writer: Sender<String>,
    pending: PendingMap,
    next_id: Arc<AtomicU64>,
    dead: Arc<AtomicBool>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        // kill then reap: a killed child never reaped stays a zombie for
        // the life of the REPL, one per server respawn
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct McpRegistry {
    servers: Vec<Arc<McpServer>>,
}

/// One `/mcp` row.
pub struct ServerRow {
    pub name: String,
    pub target: String,
    pub ready: bool,
    pub tools: usize,
    pub reason: String,
}

impl McpRegistry {
    /// Connect every configured server in parallel; a slow or dead server
    /// costs at most `CONNECT_TIMEOUT` and never aborts the others.
    pub fn connect(specs: &[ServerSpec], cwd: &Path) -> McpRegistry {
        let servers = std::thread::scope(|scope| {
            let handles: Vec<_> = specs
                .iter()
                .map(|spec| scope.spawn(|| connect_server(spec, cwd)))
                .collect();
            handles
                .into_iter()
                .zip(specs)
                .map(|(handle, spec)| {
                    handle
                        .join()
                        .unwrap_or_else(|_| failed(spec.clone(), "connect thread panicked"))
                })
                .collect::<Vec<_>>()
        });
        McpRegistry { servers }
    }

    /// An empty registry: no servers configured, or none wanted.
    pub fn empty() -> McpRegistry {
        McpRegistry {
            servers: Vec::new(),
        }
    }

    /// Append one `McpTool` per tool of every ready server.
    pub fn mount_tools(&self, out: &mut Vec<Box<dyn Tool>>) {
        for server in &self.servers {
            if let ServerState::Ready { tools } = &*lock(&server.state) {
                for meta in tools {
                    out.push(Box::new(McpTool {
                        server: Arc::clone(server),
                        mounted: mounted_name(&server.spec.name, &meta.name),
                        tool_name: meta.name.clone(),
                        description: if meta.description.is_empty() {
                            format!("MCP tool {} from server {}", meta.name, server.spec.name)
                        } else {
                            format!("{} (mcp server: {})", meta.description, server.spec.name)
                        },
                        schema: meta.schema.clone(),
                    }));
                }
            }
        }
    }

    /// Listing for `/mcp`; never connects.
    pub fn rows(&self) -> Vec<ServerRow> {
        self.servers
            .iter()
            .map(|server| {
                let (ready, tools, reason) = match &*lock(&server.state) {
                    ServerState::Ready { tools } => (true, tools.len(), String::new()),
                    ServerState::Failed { reason } => (false, 0, reason.clone()),
                };
                ServerRow {
                    name: server.spec.name.clone(),
                    target: format!("{} {}", server.spec.command, server.spec.args.join(" "))
                        .trim()
                        .to_string(),
                    ready,
                    tools,
                    reason,
                }
            })
            .collect()
    }

    /// Diagnostics tail (server stderr and notifications) for `/mcp`.
    pub fn tail_lines(&self, name: &str) -> Vec<String> {
        let Some(server) = self.servers.iter().find(|s| s.spec.name == name) else {
            return Vec::new();
        };
        lock(&server.tail).iter().cloned().collect()
    }
}

/// A server that never connected (spawn/handshake failure, or a connect
/// thread that panicked); its tools never mount.
fn failed(spec: ServerSpec, reason: &str) -> Arc<McpServer> {
    Arc::new(McpServer {
        spec,
        state: Mutex::new(ServerState::Failed {
            reason: reason.to_string(),
        }),
        tail: Arc::new(Mutex::new(VecDeque::new())),
        conn: Mutex::new(None),
    })
}

fn connect_server(spec: &ServerSpec, cwd: &Path) -> Arc<McpServer> {
    let server = failed(spec.clone(), "");
    match server.handshake(cwd, CONNECT_TIMEOUT) {
        Ok(tools) => *lock(&server.state) = ServerState::Ready { tools },
        Err(reason) => *lock(&server.state) = ServerState::Failed { reason },
    }
    server
}

impl McpServer {
    /// Spawn the process and run the initialize → tools/list handshake,
    /// returning the advertised tools.
    fn handshake(&self, cwd: &Path, timeout: Duration) -> Result<Vec<ToolMeta>, String> {
        self.spawn_and_initialize(cwd, timeout)?;
        let result = self.request("tools/list", json!({}), timeout)?;
        Ok(parse_tools(&result))
    }

    /// Spawn the process, run the initialize → initialized sequence and
    /// install the connection (first connect and respawn share it).
    fn spawn_and_initialize(&self, cwd: &Path, timeout: Duration) -> Result<(), String> {
        let conn = self.spawn_conn(cwd)?;
        self.initialize_conn(&conn, timeout)?;
        *lock(&self.conn) = Some(conn);
        Ok(())
    }

    fn initialize_conn(&self, conn: &Conn, timeout: Duration) -> Result<(), String> {
        self.request_on(conn, "initialize", initialize_params(), timeout)?;
        conn.writer
            .send(frame_line(
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            ))
            .map_err(|_| "server pipe closed".to_string())
    }

    fn spawn_conn(&self, cwd: &Path) -> Result<Conn, String> {
        let mut command = Command::new(&self.spec.command);
        command
            .args(&self.spec.args)
            .envs(self.spec.env.iter().cloned())
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot spawn '{}': {e}", self.spec.command))?;
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
        let tail = Arc::clone(&self.tail);

        let reader_pending = Arc::clone(&pending);
        let reader_writer = tx.clone();
        let reader_dead = Arc::clone(&dead);
        std::thread::spawn(move || {
            reader_loop(stdout, &reader_pending, &reader_writer, &tail, &reader_dead)
        });

        let stderr_tail = Arc::clone(&self.tail);
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                push_tail(&stderr_tail, line);
            }
        });

        Ok(Conn {
            child,
            writer: tx,
            pending,
            next_id: Arc::new(AtomicU64::new(0)),
            dead,
        })
    }

    /// Send one JSON-RPC request and await its reply, slicing the wait so
    /// ctrl+c stays responsive. A dead connection is respawned (with a fresh
    /// handshake) once; if that fails the error surfaces to the model.
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        // the respawn check must not hold the conn lock across it: the
        // respawn path locks conn itself, and waiting requests hold nothing
        // (replies correlate by id), so re-locking here would deadlock
        let needs_respawn = {
            let guard = lock(&self.conn);
            guard
                .as_ref()
                .is_none_or(|conn| conn.dead.load(Ordering::Relaxed))
        };
        if needs_respawn {
            self.respawn(timeout)?;
        }
        let guard = lock(&self.conn);
        let conn = guard
            .as_ref()
            .ok_or("connection unavailable (spawn failed)")?;
        self.request_on(conn, method, params, timeout)
    }

    /// Replace a dead (or missing) connection: spawn + handshake under the
    /// conn lock so concurrent callers respawn at most once, then refresh
    /// the advertised tool list (the dead process's cached list must not
    /// survive into the new incarnation).
    fn respawn(&self, timeout: Duration) -> Result<(), String> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        {
            let mut guard = lock(&self.conn);
            if guard
                .as_ref()
                .is_some_and(|conn| !conn.dead.load(Ordering::Relaxed))
            {
                return Ok(()); // another caller already respawned
            }
            let conn = self.spawn_conn(&cwd)?;
            self.initialize_conn(&conn, timeout)?;
            *guard = Some(conn);
        }
        // tools/list straight on the fresh connection — going through
        // request() could recurse into respawn() again when a server dies
        // right after its handshake; a failure here downgrades the state but
        // leaves the caller's own request to proceed (it may still work)
        let refreshed = {
            let guard = lock(&self.conn);
            match guard.as_ref() {
                Some(conn) => self.request_on(conn, "tools/list", json!({}), timeout),
                None => Err("connection unavailable".to_string()),
            }
        };
        match refreshed {
            Ok(result) => {
                let tools = parse_tools(&result);
                *lock(&self.state) = ServerState::Ready { tools };
            }
            Err(e) => *lock(&self.state) = ServerState::Failed { reason: e },
        }
        Ok(())
    }

    fn request_on(
        &self,
        conn: &Conn,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = conn.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = sync_channel(1);
        lock(&conn.pending).insert(id, tx);
        let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        if conn.writer.send(frame_line(&frame)).is_err() {
            lock(&conn.pending).remove(&id);
            return Err("server pipe closed".to_string());
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
                        return Err(format!("'{method}' timed out after {}s", timeout.as_secs()));
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("connection closed by server".to_string());
                }
            }
        }
    }
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "llm", "version": env!("CARGO_PKG_VERSION")}
    })
}

fn frame_line(frame: &Value) -> String {
    format!("{}\n", serde_json::to_string(frame).unwrap_or_default())
}

/// The writer thread owns the child's stdin: one line per frame, sent
/// through a channel so a stuck server can never block `execute()`.
fn writer_loop(mut stdin: ChildStdin, rx: Receiver<String>) {
    while let Ok(line) = rx.recv() {
        if stdin.write_all(line.as_bytes()).is_err() || stdin.flush().is_err() {
            break;
        }
    }
    // dropping stdin closes the pipe; the server sees EOF and exits
}

/// Route one decoded JSON-RPC message: replies reach their waiter by id,
/// server-to-client requests get a method-not-found reply, notifications
/// land in the diagnostics tail.
fn dispatch_message(
    value: &Value,
    pending: &Mutex<HashMap<u64, SyncSender<Result<Value, String>>>>,
    writer: &Sender<String>,
    tail: &Mutex<VecDeque<String>>,
) {
    let Some(id) = value.get("id") else {
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            push_tail(tail, format!("notification: {method}"));
        }
        return;
    };
    if value.get("method").is_some() {
        // server-to-client request (sampling, roots, ...): refuse politely
        let reply =
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}});
        let _ = writer.send(frame_line(&reply));
        return;
    }
    let Some(id) = id.as_u64() else { return };
    let mut map = lock(pending);
    if let Some(tx) = map.remove(&id) {
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("server error");
            let _ = tx.send(Err(format!("server error: {message}")));
        } else {
            let _ = tx.send(Ok(value.get("result").cloned().unwrap_or(Value::Null)));
        }
    }
    // unknown ids (replies arriving after a local timeout) are dropped
}

fn reader_loop(
    stdout: std::process::ChildStdout,
    pending: &Mutex<HashMap<u64, SyncSender<Result<Value, String>>>>,
    writer: &Sender<String>,
    tail: &Mutex<VecDeque<String>>,
    dead: &AtomicBool,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(value) => dispatch_message(&value, pending, writer, tail),
                    Err(_) => push_tail(
                        tail,
                        format!(
                            "unparsable line: {}",
                            &trimmed[..crate::core::text::floor_boundary(trimmed, 200)]
                        ),
                    ),
                }
            }
        }
    }
    dead.store(true, Ordering::Relaxed);
    // fail every waiter fast so blocked tool calls return promptly
    for (_, tx) in lock(pending).drain() {
        let _ = tx.send(Err("connection closed by server".to_string()));
    }
}

fn push_tail(tail: &Mutex<VecDeque<String>>, line: String) {
    let mut tail = lock(tail);
    if tail.len() >= TAIL_LINES {
        tail.pop_front();
    }
    tail.push_back(line);
}

fn parse_tools(result: &Value) -> Vec<ToolMeta> {
    let mut out = Vec::new();
    let Some(tools) = result.get("tools").and_then(Value::as_array) else {
        return out;
    };
    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        out.push(ToolMeta {
            name: name.to_string(),
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            schema: tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"})),
        });
    }
    out
}

/// `mcp__<server>__<tool>`: sanitized, length-capped with a stable hash
/// suffix on overflow (the reference agent's mounted_name shape).
fn mounted_name(server: &str, tool: &str) -> String {
    let sanitize = |raw: &str| -> String {
        raw.chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .collect()
    };
    let full = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    if full.chars().count() <= MAX_MOUNTED_NAME {
        return full;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    full.hash(&mut hasher);
    let suffix = format!("{:08x}", u32::try_from(hasher.finish()).unwrap_or(u32::MAX));
    let keep = MAX_MOUNTED_NAME - suffix.len() - 1;
    let truncated: String = full.chars().take(keep).collect();
    format!("{truncated}_{suffix}")
}

/// Map a `tools/call` result to the agent's ToolOutput: text blocks joined
/// with newlines, `isError` flagged, pretty JSON when no text came back.
fn result_to_output(result: &Value) -> ToolOutput {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut texts: Vec<String> = Vec::new();
    let mut non_text = 0usize;
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    texts.push(text.to_string());
                }
            } else {
                non_text += 1;
            }
        }
    }
    let text = if texts.is_empty() {
        serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
    } else {
        texts.join("\n")
    };
    let (mut out, truncated) = truncate_tail(&text, MAX_LINES, MAX_BYTES);
    if non_text > 0 {
        out.push_str(&format!("\n[+{non_text} non-text content blocks]\n"));
    }
    if truncated {
        out.push_str("\n[output truncated]\n");
    }
    // (kept as a two-step: the non-text note interleaves with the marker)
    if is_error {
        ToolOutput::err(out)
    } else {
        ToolOutput::ok(out)
    }
}

// ============================================================================
// mounted tool
// ============================================================================

struct McpTool {
    server: Arc<McpServer>,
    mounted: String,
    tool_name: String,
    description: String,
    schema: Value,
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.mounted
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
        super::tools::args_preview(&self.mounted, args)
    }
    fn execute(&self, args: &Value, _cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let params = json!({"name": self.tool_name, "arguments": args});
        match self.server.request("tools/call", params, REQUEST_TIMEOUT) {
            Ok(result) => result_to_output(&result),
            Err(e) => ToolOutput::err(format!(
                "mcp server '{}': {e} (see /mcp)",
                self.server.spec.name
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn parses_mcp_servers_table() {
        let raw = r#"{
          "mcpServers": {
            "fetch": {"command": "uvx", "args": ["mcp-server-fetch"]},
            "remote": {"command": "run", "env": {"TOKEN": "${MY_TOKEN}"}}
          }
        }"#;
        let specs = parse(raw);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].command, "uvx");
        assert_eq!(specs[0].args, vec!["mcp-server-fetch".to_string()]);
        assert_eq!(specs[1].env.len(), 1);
        assert_eq!(specs[1].env[0].0, "TOKEN");
    }

    #[test]
    fn missing_or_invalid_config_yields_nothing() {
        assert!(parse("{}").is_empty());
        assert!(parse("not json {{{").is_empty());
        assert!(parse(r#"{"mcpServers": []}"#).is_empty());
    }

    #[test]
    fn mounted_name_preserves_sanitizes_and_caps() {
        assert_eq!(
            mounted_name("github", "create_issue"),
            "mcp__github__create_issue"
        );
        assert_eq!(mounted_name("my server!", "a b"), "mcp__my_server___a_b");
        let long = mounted_name(&"s".repeat(40), &"t".repeat(60));
        assert!(long.chars().count() == MAX_MOUNTED_NAME);
        // stable: same inputs, same truncation hash
        assert_eq!(long, mounted_name(&"s".repeat(40), &"t".repeat(60)));
        assert!(long.starts_with("mcp__"));
    }

    #[test]
    fn result_mapping_joins_text_and_honors_iserror() {
        let result = json!({
            "content": [
                {"type": "text", "text": "line one"},
                {"type": "text", "text": "line two"}
            ]
        });
        let out = result_to_output(&result);
        assert_eq!(out.content, "line one\nline two");
        assert!(!out.is_error);

        let failed = json!({
            "isError": true,
            "content": [{"type": "text", "text": "boom"}]
        });
        let out = result_to_output(&failed);
        assert!(out.is_error);
        assert_eq!(out.content, "boom");
    }

    #[test]
    fn result_mapping_falls_back_to_json_and_counts_non_text() {
        let result = json!({"content": [{"type": "image", "data": "…"}]});
        let out = result_to_output(&result);
        assert!(out.content.contains("\"content\""));
        assert!(out.content.contains("[+1 non-text content blocks]"));
    }

    #[test]
    fn dispatch_delivers_response_by_id() {
        let pending = Mutex::new(HashMap::new());
        let (tx, rx) = sync_channel(1);
        pending.lock().unwrap().insert(7, tx);
        let (writer_tx, _writer_rx) = mpsc::channel();
        let tail = Mutex::new(VecDeque::new());
        let reply = json!({"jsonrpc":"2.0","id":7,"result":{"tools":[]}});
        dispatch_message(&reply, &pending, &writer_tx, &tail);
        let got = rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(got["tools"], json!([]));
        assert!(pending.lock().unwrap().is_empty());
    }

    #[test]
    fn dispatch_replies_to_server_requests_with_method_not_found() {
        let pending = Mutex::new(HashMap::new());
        let (writer_tx, writer_rx) = mpsc::channel();
        let tail = Mutex::new(VecDeque::new());
        let req = json!({"jsonrpc":"2.0","id":"abc","method":"sampling/createMessage","params":{}});
        dispatch_message(&req, &pending, &writer_tx, &tail);
        let reply: Value =
            serde_json::from_str(&writer_rx.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap();
        assert_eq!(reply["id"], json!("abc"));
        assert_eq!(reply["error"]["code"], json!(-32601));
    }

    #[test]
    fn dispatch_routes_notifications_to_tail_and_drops_unknown_ids() {
        let pending = Mutex::new(HashMap::new());
        let (writer_tx, _writer_rx) = mpsc::channel();
        let tail = Mutex::new(VecDeque::new());
        dispatch_message(
            &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{}}),
            &pending,
            &writer_tx,
            &tail,
        );
        assert!(lock(&tail).iter().any(|l| l.contains("progress")));
        // a reply whose waiter already timed out is dropped silently
        dispatch_message(
            &json!({"jsonrpc":"2.0","id":99,"result":null}),
            &pending,
            &writer_tx,
            &tail,
        );
        assert!(pending.lock().unwrap().is_empty());
    }

    #[test]
    fn parse_tools_reads_name_description_schema() {
        let result = json!({"tools": [
            {"name": "echo", "description": "Echo text", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
            {"description": "no name is skipped"},
            {"name": "bare"}
        ]});
        let tools = parse_tools(&result);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].description, "Echo text");
        assert_eq!(tools[1].schema, json!({"type": "object"}));
    }

    /// A minimal newline-JSON-RPC server in sh: echoes every request id
    /// back with an empty result, good enough for initialize + tools/list.
    #[cfg(unix)]
    fn write_echo_server(dir: &std::path::Path) -> std::path::PathBuf {
        let script = dir.join("server.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nwhile IFS= read -r line; do\n  id=$(printf '%s' \"$line\" | sed 's/.*\"id\":\\([0-9]*\\).*/\\1/')\n  printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"tools\":[]}}\\n' \"$id\"\ndone\n",
        )
        .unwrap();
        script
    }

    /// Killing the server process must not hang the next call: request()
    /// used to hold the conn lock across its own respawn path and deadlock.
    /// A hang here manifests as the test timing out, not a clean failure.
    #[cfg(unix)]
    #[test]
    fn dead_server_respawns_instead_of_deadlocking() {
        let dir = std::env::temp_dir().join(format!("llm-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = write_echo_server(&dir);
        let spec = ServerSpec {
            name: "echo".into(),
            command: "sh".into(),
            args: vec![script.display().to_string()],
            env: Vec::new(),
        };
        let server = connect_server(&spec, std::path::Path::new("."));
        assert!(
            matches!(&*lock(&server.state), ServerState::Ready { .. }),
            "echo server must connect"
        );
        // kill the live child and wait for the reader thread to flag it dead
        {
            let mut guard = lock(&server.conn);
            guard.as_mut().expect("connected").child.kill().unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !lock(&server.conn)
            .as_ref()
            .is_some_and(|c| c.dead.load(Ordering::Relaxed))
        {
            assert!(
                Instant::now() < deadline,
                "reader never noticed the dead child"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // the old code deadlocked here (conn lock held across respawn);
        // now it respawns and the request goes through on the new process
        let result = server.request("tools/list", json!({}), Duration::from_secs(5));
        assert!(result.is_ok(), "respawned request failed: {result:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
