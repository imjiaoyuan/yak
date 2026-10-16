//! The extension host: user-built executables that extend the agent with
//! custom tools, slash commands and event hooks (the extensions, done
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
//! Discovery is two homes, project overriding user by name: `~/.yak/extensions/`
//! and the nearest `.yak/extensions/` walking up from the working directory.
//! An entry is any executable file (shebang decides the language); config
//! `extensions.disabled` skips one by name. Extension tools are Exec-tier —
//! the approval matrix asks by default. stdout carries only the protocol;
//! stderr is dimmed into a diagnostics tail.
//!
//! This file is the host itself — connection state, request dispatch and the
//! tool wrappers. `manifest.rs` parses the `# --- yak-tool:` headers (and the
//! argv execution of those scripts), `proto.rs` holds the two stdio loops the
//! connections run on, and `roots.rs` finds the homes.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::approval::Tier;
use super::tools::{MAX_BYTES, MAX_LINES, MAX_TOKENS, Tool, ToolOutput, truncate_marked_budgeted};
use proto::{lossy_lines, reader_loop, writer_loop};
use script::ScriptTool;

/// Budget for spawn + initialize at startup and on `/reload`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-tool-call timeout (config `extensions.tool_timeout` overrides).
const TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// Event-hook timeout: extensions must be quick at turn boundaries.
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
/// stderr lines kept for diagnostics.
const TAIL_LINES: usize = 20;
/// stderr lines held for the live tool log while a call is in flight; a
/// chatty extension cannot grow the buffer without bound.
const PROGRESS_LINES: usize = 64;
/// `recv_timeout` slice; keeps ctrl+c responsive while waiting.
const POLL_SLICE: Duration = Duration::from_millis(20);

/// One tool advertised by the initialize handshake.
#[derive(Clone)]
pub struct ToolMeta {
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// declared trust tier; an absent or unknown value stays exec
    pub tier: Tier,
}

/// One skill advertised by the initialize reply: name, description and
/// whether the model may pick it up on its own. The REPL surface (banner,
/// `/help`, `/skill:<name>`) reads these; the system-prompt listing rides
/// whatever `system` section the extension contributes.
#[derive(Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    /// false = excluded from the system-prompt list; only /skill:name works
    pub model_invocation: bool,
}

pub struct ExtState {
    /// advertised tools, commands and event subscriptions
    tools: Vec<ToolMeta>,
    commands: Vec<String>,
    events: Vec<String>,
    /// a section appended to the agent system prompt (the `<available_skills>`
    /// block when this is the skills plugin); None when the extension
    /// contributes nothing
    system_section: Option<String>,
    /// advertised skills, for the REPL surfaces
    skills: Vec<SkillMeta>,
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
    /// stderr the extension wrote since the last drain: the tool-call waiter
    /// hands these to the live tool log, so progress a resident extension
    /// prints is visible while it works and still never enters the model's
    /// context (the tool result is the protocol reply alone)
    progress: Arc<Mutex<VecDeque<String>>>,
    /// id-less `{"type":"lane", ..}` frames the extension streamed since
    /// the last drain: same delivery shape as stderr progress, but these
    /// are live status rows the view rewrites in place
    lanes: Arc<Mutex<VecDeque<(String, String)>>>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        // a polite shutdown first: hand the frame to the writer thread, then
        // give it a moment to reach the child and the child a moment to flush
        // and exit on its own — only then kill. Sending alone never worked:
        // the writer writes asynchronously and the kill below always won the
        // race, so the documented shutdown frame never actually arrived.
        if self
            .writer
            .send("{\"type\":\"shutdown\"}\n".to_string())
            .is_ok()
        {
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                match self.child.try_wait() {
                    Ok(Some(_)) => return, // exited cleanly, nothing to kill
                    Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                    Err(_) => break,
                }
            }
        }
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

    /// Take the progress lines written since the last call, draining the
    /// buffer (the caller discards them to start a call on a clean slate).
    fn take_progress(&self) -> Vec<String> {
        crate::core::sync::lock(&self.progress).drain(..).collect()
    }

    /// Take the lane frames written since the last drain, same discipline.
    fn take_lanes(&self) -> Vec<(String, String)> {
        crate::core::sync::lock(&self.lanes).drain(..).collect()
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
        match &*crate::core::sync::lock(&self.state) {
            Ok(_) => Ok(()),
            Err(reason) => Err(reason.clone()),
        }
    }

    fn is_alive(&self) -> bool {
        let mut guard = crate::core::sync::lock(&self.conn);
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
        let _serialize = crate::core::sync::lock(&self.respawn);
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
    /// stays responsive. The connection is revived on demand first. `log`,
    /// when given, receives the extension's stderr as live progress.
    fn request<'a, 'b>(
        &self,
        msg: &Value,
        timeout: Duration,
        mut log: Option<&'a mut (dyn FnMut(crate::agent::tools::ToolProgress) + 'b)>,
    ) -> Result<Value, String> {
        self.ensure_alive()?;
        match self.request_live(msg, timeout, log.as_deref_mut()) {
            Err(e) if self.should_retry(&e) => {
                self.respawn();
                match &*crate::core::sync::lock(&self.state) {
                    Ok(_) => self.request_live(msg, timeout, log),
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
    fn request_live<'a, 'b>(
        &self,
        msg: &Value,
        timeout: Duration,
        mut log: Option<&'a mut (dyn FnMut(crate::agent::tools::ToolProgress) + 'b)>,
    ) -> Result<Value, String> {
        let mut guard = crate::core::sync::lock(&self.conn);
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
        crate::core::sync::lock(&conn.pending).insert(id, tx);
        // a call starts on a clean slate: stderr or lanes from an earlier
        // call (or an idle chatty extension) must not replay as this call's
        // progress
        conn.take_progress();
        conn.take_lanes();
        // a host-built frame is plain data; a serialization failure would
        // corrupt the framing (an empty line to the child), so it aborts
        // loudly instead of degrade silently
        let mut frame = serde_json::to_string(msg).expect("host-built frame serializes");
        frame.push('\n');
        if conn.writer.send(frame).is_err() {
            crate::core::sync::lock(&conn.pending).remove(&id);
            return Err(format!("extension '{}' pipe closed", self.name));
        }
        let deadline = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(POLL_SLICE) {
                Ok(result) => {
                    // whatever stderr or lanes arrived alongside the reply
                    // still counts as progress for this call
                    if let Some(f) = log.as_mut() {
                        for line in conn.take_progress() {
                            (**f)(crate::agent::tools::ToolProgress::line(line));
                        }
                        for (lane, text) in conn.take_lanes() {
                            (**f)(crate::agent::tools::ToolProgress::Lane { lane, text });
                        }
                    }
                    return result;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // stderr written while we wait: the human sees the
                    // extension working; the model never sees these lines
                    if let Some(f) = log.as_mut() {
                        for line in conn.take_progress() {
                            (**f)(crate::agent::tools::ToolProgress::line(line));
                        }
                        for (lane, text) in conn.take_lanes() {
                            (**f)(crate::agent::tools::ToolProgress::Lane { lane, text });
                        }
                    }
                    if crate::core::http::interrupted() {
                        crate::core::sync::lock(&conn.pending).remove(&id);
                        // the caller is walking away from this call: say so,
                        // so an extension that owns long-lived work of its
                        // own (the subagent example's child processes) can
                        // stop it instead of answering nobody
                        let _ = conn.writer.send(interrupt_frame(id));
                        return Err("interrupted".to_string());
                    }
                    // the process can vanish without the reader having said
                    // so yet: stop waiting, and let `request` respawn instead
                    // of burning the full timeout on a corpse
                    if conn.exited() {
                        crate::core::sync::lock(&conn.pending).remove(&id);
                        return Err(format!("extension '{}' is not running", self.name));
                    }
                    if Instant::now() >= deadline {
                        crate::core::sync::lock(&conn.pending).remove(&id);
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
    pub fn call_tool(
        &self,
        name: &str,
        args: &Value,
        call_id: Option<&str>,
        log: &mut dyn FnMut(crate::agent::tools::ToolProgress),
    ) -> Result<String, String> {
        let timeout = match &*crate::core::sync::lock(&self.state) {
            Ok(state) => state.tool_timeout,
            Err(_) => TOOL_TIMEOUT,
        };
        let mut frame =
            json!({"id": next_id(), "type": "call_tool", "v": 1, "name": name, "args": args});
        if let Some(call_id) = call_id {
            // the model's own tool_call_id, so a stateful extension can key
            // per-call state (calls in the read-only batch run concurrently)
            frame["tool_call_id"] = json!(call_id);
        }
        let result = self.request(&frame, timeout, Some(log))?;
        Ok(match result.get("result") {
            Some(Value::String(s)) => s.clone(),
            // a segment list: each item is a string, or `{"text": ..,
            // "console": true}` for console lines — laid out
            // segmented way, so several output items cannot run together
            Some(Value::Array(items)) if !items.is_empty() => format_segments(items),
            Some(other) => crate::jsonfmt::dumps_indent(other, 2),
            // an error frame already surfaced as Err from `request`, so a
            // reply with neither result nor error broke the protocol — say
            // so instead of handing the model an empty success
            None => {
                return Err(format!(
                    "extension '{}' replied without a result",
                    self.name
                ));
            }
        })
    }

    /// Run one extension-registered slash command. The whole reply object
    /// comes back: `result` prints, `task` submits as one agent task (the
    /// `/skill:<name>` shape — a command whose answer is work for the model,
    /// not text for the terminal), so the caller, not the host, interprets.
    pub fn run_command(&self, name: &str, args: &str) -> Result<Value, String> {
        let result = self.request(
            &json!({"id": next_id(), "type": "run_command", "v": 1, "name": name, "args": args}),
            TOOL_TIMEOUT,
            None,
        )?;
        result
            .get("result")
            .cloned()
            .ok_or_else(|| format!("extension '{}' replied without a result", self.name))
    }

    /// Fire an event hook. `tool_call` expects a reply — the extension may
    /// deny the call (`{"decision":"deny","reason":..}`) or rewrite its
    /// arguments (`{"args":{..}}`); every other event is fire-and-forget.
    pub fn fire(&self, name: &str, params: &Value) -> Result<Option<Value>, String> {
        let subscribed = crate::core::sync::lock(&self.state)
            .as_ref()
            .is_ok_and(|s| s.events.iter().any(|e| e == name));
        if !subscribed {
            return Ok(None);
        }
        let result = self.request(
            &json!({"id": next_id(), "type": "event", "v": 1, "name": name, "params": params}),
            EVENT_TIMEOUT,
            None,
        )?;
        Ok(result.get("result").cloned())
    }

    /// A dim diagnostics line (extension errors at event boundaries).
    fn note(&self, line: String) {
        let mut tail = crate::core::sync::lock(&self.tail);
        if tail.len() >= TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    /// This connection's own system-prompt section, when its initialize
    /// reply contributed one (a failed extension contributes nothing).
    pub fn system_section(&self) -> Option<String> {
        crate::core::sync::lock(&self.state)
            .as_ref()
            .ok()
            .and_then(|s| s.system_section.clone())
    }

    /// This connection's own advertised skills (empty on any failure).
    pub fn skills(&self) -> Vec<SkillMeta> {
        crate::core::sync::lock(&self.state)
            .as_ref()
            .map(|s| s.skills.clone())
            .unwrap_or_default()
    }
}

/// Lay out a segmented tool result so the model can tell items apart when
/// more than one output segment rides the reply. Each non-console segment
/// starts with a `==> text N/M <==` line, and console segments follow all
/// other output in one `<console_output>` block. An empty list or a list
/// with no text and no console lines renders as its pretty JSON — the
/// extension probably meant a plain value.
fn format_segments(items: &[Value]) -> String {
    let seg_text = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("text").map(|t| match t {
                Value::String(s) => s.clone(),
                other => crate::jsonfmt::dumps_indent(other, 2),
            }),
            _ => None,
        }
    };
    let is_console =
        |v: &Value| matches!(v, Value::Object(o) if o.get("console") == Some(&Value::Bool(true)));
    let total = items
        .iter()
        .filter(|v| !is_console(v) && seg_text(v).is_some())
        .count();
    let mut out: Vec<String> = Vec::new();
    let mut console: Vec<String> = Vec::new();
    let mut index = 0usize;
    for v in items {
        if is_console(v) {
            if let Some(t) = seg_text(v) {
                console.push(t);
            }
        } else if let Some(t) = seg_text(v) {
            index += 1;
            if total > 1 {
                out.push(format!("==> text {index}/{total} <==\n{t}"));
            } else {
                out.push(t);
            }
        }
    }
    if out.is_empty() && console.is_empty() {
        return crate::jsonfmt::dumps_indent(&Value::Array(items.to_vec()), 2);
    }
    if !console.is_empty() {
        out.push(format!(
            "<console_output>\n{}\n</console_output>",
            console.join("\n")
        ));
    }
    out.join("\n")
}

#[derive(Clone)]
pub struct Extensions {
    pub(crate) exts: Vec<Arc<Ext>>,
    pub(crate) script_tools: Vec<ExecToolSpec>,
}

/// A background connect started by [`Extensions::connect_async`]: discovery
/// has already run, the handshakes are still in flight. `join` settles them
/// into a ready host — until then the caller keeps doing its own startup
/// work, so the spawn cost overlaps instead of stacking.
pub struct ExtensionsConnecting {
    handle: Option<std::thread::JoinHandle<Vec<Arc<Ext>>>>,
    script_tools: Vec<ExecToolSpec>,
}

impl ExtensionsConnecting {
    pub fn join(mut self) -> Extensions {
        let exts = match self.handle.take() {
            Some(h) => h.join().unwrap_or_else(|_| {
                vec![failed("extension", "connect thread panicked".to_string())]
            }),
            None => Vec::new(),
        };
        // failure notes print here, on the caller's thread: from inside the
        // background connect they race the caller's own startup output
        // (banners, fork notices) on stderr and can tear a line in half
        for ext in &exts {
            if let Err(reason) = &*crate::core::sync::lock(&ext.state) {
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
            script_tools: self.script_tools,
        }
    }
}

impl Extensions {
    /// The plugins' file mtimes, newest first, as (mtime-or-zero, index)
    /// pairs: the listing surfaces order shared by the banner row and
    /// `/status`, so the two never disagree. A file that cannot be stat'ed
    /// (a placeholder, a deleted script) sorts last, in registry order.
    fn newest_first(paths: &[Option<std::path::PathBuf>]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..paths.len()).collect();
        order.sort_by_key(|&i| {
            let mtime = paths[i]
                .as_deref()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok());
            match mtime {
                // Reverse: the newest mtime leads. None sinks, and equal
                // mtimes (a batch install) keep discovery order
                Some(t) => std::cmp::Reverse(
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                ),
                None => std::cmp::Reverse(u64::MIN),
            }
        });
        order
    }

    /// Every mounted plugin, for the banner: a resident extension's stem
    /// (marked when it never connected) followed by each manifest script
    /// tool's declared name — both live in the same registry, so both
    /// belong in the one row. Newest first: the row reads as "what landed
    /// lately", so a freshly added plugin leads instead of hiding behind
    /// the veterans in an arbitrary directory order.
    pub fn plugin_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .exts
            .iter()
            .map(|ext| match &*crate::core::sync::lock(&ext.state) {
                Ok(_) => ext.name.clone(),
                Err(_) => format!("{} (failed)", ext.name),
            })
            .collect();
        names.extend(self.script_tools.iter().map(|s| s.name.clone()));
        let paths: Vec<Option<std::path::PathBuf>> = self
            .exts
            .iter()
            .map(|e| e.path.clone())
            .chain(self.script_tools.iter().map(|s| Some(s.path.clone())))
            .collect();
        let sorted: Vec<String> = Self::newest_first(&paths)
            .into_iter()
            .map(|i| names[i].clone())
            .collect();
        sorted
    }
    /// An empty host: no residents, no script tools. The agent loop holds
    /// one unconditionally — with nothing discovered it fires no events and
    /// mounts nothing, which replaces the old `Option<&Extensions>`.
    /// Production paths always hold a real connected host; only tests need
    /// the empty one.
    #[cfg(test)]
    pub fn empty() -> Extensions {
        Self::EMPTY
    }

    /// The host with no extensions: fires nothing, mounts nothing (the
    /// const form of [`Extensions::empty`], usable where a `&'static` is
    /// not needed).
    #[cfg(test)]
    pub(crate) const EMPTY: Extensions = Extensions {
        exts: Vec::new(),
        script_tools: Vec::new(),
    };

    /// Discover everything, and spawn+handshake the resident extensions on
    /// a background thread while the caller keeps doing startup work
    /// (stdin, attachments, the thread store, the system prompt); `join`
    /// settles the handshake. A slow or broken one costs at most
    /// `CONNECT_TIMEOUT` and never aborts the others — it lands in the
    /// list as Failed with a reason.
    ///
    /// An error return before `join` exits without waiting: the children
    /// see stdin EOF when this process dies and exit per the protocol —
    /// the same contract a crash or kill -9 already relies on. `Drop` must
    /// not join instead: a flag error would then block on a slow handshake.
    pub fn connect_async(cwd: &Path) -> ExtensionsConnecting {
        let found = discover(cwd);
        let resident = found.resident;
        let handle = std::thread::spawn(move || {
            std::thread::scope(|scope| {
                let handles: Vec<_> = resident
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
            })
        });
        ExtensionsConnecting {
            handle: Some(handle),
            script_tools: found.script_tools,
        }
    }

    /// The synchronous form: discovery plus handshake, all on this thread.
    pub fn connect(cwd: &Path) -> Extensions {
        Self::connect_async(cwd).join()
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
            let Ok(state) = &*crate::core::sync::lock(&ext.state) else {
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
            crate::core::sync::lock(&ext.state)
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

    /// Fire `compact` across every subscriber and return the replacement
    /// compaction when one extension offers it: the last reply shaped
    /// `{"decision":"compact","summary":..,"keep_recent_tokens":n}` wins
    /// (the token field is optional; without it the caller keeps its own
    /// value). Every other event reply observes only, and the whole path is
    /// fail-open: a dead, slow, silent or malformed extension returns
    /// `None` and the caller runs its built-in compaction instead.
    pub fn replace_compaction(&self, params: &Value) -> Option<(String, Option<u32>)> {
        let mut replacement: Option<(String, Option<u32>)> = None;
        for ext in &self.exts {
            match ext.fire("compact", params) {
                Ok(Some(reply)) => {
                    if reply.get("decision").and_then(Value::as_str) == Some("compact")
                        && let Some(summary) = reply.get("summary").and_then(Value::as_str)
                    {
                        replacement = Some((
                            summary.to_string(),
                            reply
                                .get("keep_recent_tokens")
                                .and_then(Value::as_u64)
                                .and_then(|n| u32::try_from(n).ok()),
                        ));
                    }
                }
                Ok(None) => {}
                Err(e) => ext.note(e),
            }
        }
        replacement
    }

    /// Fire an event on every extension subscribed to it. Replies are
    /// fire-and-forget: per the protocol only `tool_call`, `tool_result` and
    /// `compact` replies are interpreted (here via
    /// [`Extensions::gate_tool_call`], in `rewrite_tool_result` and in
    /// `replace_compaction`) — an extension answering any other event
    /// with a `decision` is out of contract and the reply is dropped, not
    /// acted on. Failures land in the extension's diagnostics tail.
    pub fn fire(&self, name: &str, params: &Value) {
        for ext in &self.exts {
            if let Err(e) = ext.fire(name, params) {
                ext.note(e);
            }
        }
    }

    /// The `tool_call` gate across every subscriber: `Err` carries the deny
    /// reason (the call does not run; the model reads the reason as an
    /// error result), `Ok((pre_allowed, rewritten_args))` reports whether an
    /// extension pre-allowed the call (skipping the approval prompt) and
    /// the last argument rewrite, if any. The first deny wins.
    pub fn gate_tool_call(&self, params: &Value) -> Result<(bool, Option<Value>), String> {
        let mut pre_allowed = false;
        let mut args: Option<Value> = None;
        for ext in &self.exts {
            match ext.fire("tool_call", params) {
                Ok(Some(reply)) => {
                    let decision = reply.get("decision").and_then(Value::as_str);
                    if decision == Some("deny") {
                        let reason = reply
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("denied by extension");
                        return Err(reason.to_string());
                    }
                    if decision == Some("allow") {
                        pre_allowed = true;
                    }
                    if let Some(rewritten) = reply.get("args") {
                        args = Some(rewritten.clone());
                    }
                }
                Ok(None) => {}
                Err(e) => ext.note(e),
            }
        }
        Ok((pre_allowed, args))
    }

    /// The extension that registered a slash command, if any.
    pub fn command_owner(&self, name: &str) -> Option<Arc<Ext>> {
        self.exts
            .iter()
            .find(|ext| {
                crate::core::sync::lock(&ext.state)
                    .as_ref()
                    .is_ok_and(|s| s.commands.iter().any(|c| c == name))
            })
            .cloned()
    }

    /// The system-prompt sections every ready extension contributes, in
    /// discovery order (a fresh prompt embeds them; a continuation keeps
    /// its stored prompt, sections included, verbatim).
    pub fn system_sections(&self) -> Vec<String> {
        self.exts
            .iter()
            .filter_map(|ext| ext.system_section())
            .collect()
    }

    /// Every skill the ready extensions advertise, for the banner, `/help`
    /// and completion. Registry order; name merging is the provider's
    /// business (the skills plugin shadows by name before advertising).
    pub fn skills(&self) -> Vec<SkillMeta> {
        self.exts.iter().flat_map(|ext| ext.skills()).collect()
    }

    /// Every slash command any ready extension registered, for completion.
    pub fn command_names(&self) -> Vec<String> {
        self.exts
            .iter()
            .filter_map(|ext| {
                crate::core::sync::lock(&ext.state)
                    .as_ref()
                    .ok()
                    .map(|s| s.commands.clone())
            })
            .flatten()
            .collect()
    }

    /// Listing for `/tools`: (name, target, tools, commands, reason).
    pub fn rows(&self) -> Vec<(String, String, usize, usize, String)> {
        let paths: Vec<Option<PathBuf>> = self.exts.iter().map(|e| e.path.clone()).collect();
        let rows: Vec<_> = self
            .exts
            .iter()
            .map(|ext| match &*crate::core::sync::lock(&ext.state) {
                Ok(state) => (
                    ext.name.clone(),
                    ext.target.clone(),
                    state.tools.len(),
                    state.commands.len(),
                    String::new(),
                ),
                Err(reason) => (ext.name.clone(), ext.target.clone(), 0, 0, reason.clone()),
            })
            .collect();
        // the same newest-first order the banner's plugin row shows, so the
        // two listings never disagree about who leads
        Self::newest_first(&paths)
            .into_iter()
            .map(|i| rows[i].clone())
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

/// The host-to-extension notice that a call was abandoned (ctrl+c): the
/// cancelled request's id, plus a fresh id so the frame keeps the protocol's
/// shape. No reply is expected, and an extension that ignores it simply
/// finishes into a reply nobody is waiting for.
fn interrupt_frame(cancelled: u64) -> String {
    let mut frame =
        json!({"id": next_id(), "type": "interrupt", "v": 1, "cancelled": cancelled}).to_string();
    frame.push('\n');
    frame
}

/// A per-extension tool-call budget, when the initialize reply asks for one:
/// `"tool_timeout": <seconds>`. Zero and non-numeric values are ignored, and
/// the long end is clamped so a wedged extension cannot park the turn past
/// an hour (ctrl+c still interrupts a running call at any point).
fn parse_tool_timeout(result: &Value) -> Option<Duration> {
    const MAX: u64 = 3600;
    let secs = result.get("tool_timeout")?.as_u64().filter(|s| *s > 0)?;
    Some(Duration::from_secs(secs.min(MAX)))
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
            *crate::core::sync::lock(&self.conn) = Some(conn);
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
                            .to_string(),
                        "user_dir": crate::core::config::user_dir().display().to_string()
                    },
                }),
                CONNECT_TIMEOUT,
                None,
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
            let system_section = result
                .get("system")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string);
            let skills = parse_skills(result);
            // a long-running tool (the subagent example) may ask for its
            // own call budget in the initialize reply; the config value
            // stays the default for extensions that do not
            let tool_timeout = parse_tool_timeout(result)
                .unwrap_or_else(crate::core::config::extension_tool_timeout);
            *crate::core::sync::lock(&self.state) = Ok(ExtState {
                tools,
                commands,
                events,
                system_section,
                skills,
                tool_timeout,
            });
            Ok(())
        })();
        if let Err(reason) = outcome {
            *crate::core::sync::lock(&self.conn) = None; // drop the child
            *crate::core::sync::lock(&self.state) = Err(reason);
        }
    }

    fn spawn_conn(&self, path: &Path) -> Result<Conn, String> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        let mut command = Command::new(path);
        // Children launched by an extension must use this harness build,
        // rather than an unrelated (possibly older) yak found on PATH.
        if std::env::var_os("YAK_BIN").is_none() {
            command.env(
                "YAK_BIN",
                std::env::current_exe()
                    .map_err(|e| format!("cannot resolve the running yak executable: {e}"))?,
            );
        }
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
        let progress: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let lanes: Arc<Mutex<VecDeque<(String, String)>>> = Arc::new(Mutex::new(VecDeque::new()));

        let reader_pending = Arc::clone(&pending);
        let reader_dead = Arc::clone(&dead);
        let reader_tail = Arc::clone(&self.tail);
        let reader_lanes = Arc::clone(&lanes);
        std::thread::spawn(move || {
            reader_loop(
                stdout,
                &reader_pending,
                &reader_dead,
                &reader_tail,
                &reader_lanes,
            )
        });

        let stderr_tail = Arc::clone(&self.tail);
        let stderr_progress = Arc::clone(&progress);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            for line in lossy_lines(&mut reader, &mut buf) {
                {
                    let mut p = crate::core::sync::lock(&stderr_progress);
                    if p.len() >= PROGRESS_LINES {
                        p.pop_front();
                    }
                    p.push_back(line.clone());
                }
                let mut t = crate::core::sync::lock(&stderr_tail);
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
            progress,
            lanes,
        })
    }
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The `skills` array of an initialize reply: `{name, description,
/// model_invocation?}` objects, an absent `model_invocation` reading true
/// (the skill is listed to the model). Entries without a name are dropped.
fn parse_skills(result: &Value) -> Vec<SkillMeta> {
    result
        .get("skills")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    let name = s.get("name")?.as_str()?;
                    Some(SkillMeta {
                        name: name.to_string(),
                        description: s
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        model_invocation: s
                            .get("model_invocation")
                            .and_then(Value::as_bool)
                            .unwrap_or(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
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
    fn execute(
        &self,
        args: &Value,
        cwd: &Path,
        log: &mut dyn FnMut(crate::agent::tools::ToolProgress),
    ) -> ToolOutput {
        // direct calls (tests) carry no call id: the frame then omits
        // `tool_call_id` rather than inventing one. The agent loop routes
        // through execute_call, which has the real id.
        self.run(args, None, cwd, log)
    }
    fn execute_call(
        &self,
        call: &crate::providers::ToolCall,
        cwd: &Path,
        log: &mut dyn FnMut(crate::agent::tools::ToolProgress),
    ) -> ToolOutput {
        self.run(&call.arguments, Some(call.id.as_str()), cwd, log)
    }
}

impl ExtTool {
    fn run(
        &self,
        args: &Value,
        call_id: Option<&str>,
        _cwd: &Path,
        log: &mut dyn FnMut(crate::agent::tools::ToolProgress),
    ) -> ToolOutput {
        match self.ext.call_tool(&self.tool_name, args, call_id, log) {
            Ok(text) => {
                if text.len() > MAX_BYTES {
                    // Same cap shape as the builtin tools (lines, then
                    // bytes, then the token estimate), but marked: a
                    // silent cut would leave the model arguing with a
                    // tool that never said its answer was incomplete.
                    ToolOutput::ok(truncate_marked_budgeted(
                        &text, MAX_LINES, MAX_BYTES, MAX_TOKENS,
                    ))
                } else {
                    ToolOutput::ok(text)
                }
            }
            Err(e) => ToolOutput::err(e),
        }
    }
}

mod manifest;
mod proto;
mod roots;
mod script;

pub use manifest::{ExecToolSpec, discover};
pub use roots::discover_dirs;

#[cfg(test)]
pub(crate) mod tests;
