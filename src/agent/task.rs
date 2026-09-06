//! Sub-agents: markdown-defined agents executed as child processes of this
//! same binary (the reference's design — isolation for free, no shared state).

use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::approval::Tier;
use super::tools::{Tool, ToolOutput};
use crate::yaml;

/// Children default to a read-only set; the `task` tool is only available to
/// a child whose definition explicitly lists it.
pub const DEFAULT_CHILD_TOOLS: &str = "read,grep,glob,ls";
pub const MAX_DEPTH: u32 = 3;
/// Cap on a sub-agent's collected output feeding the parent's context —
/// the same 50KB bash/grep results live under (a 256KB result would land
/// in the parent's context as ~64k tokens in one tool result).
pub const MAX_OUTPUT_BYTES: usize = 50 * 1024;
pub const MAX_PARALLEL: usize = 8;
pub const DEFAULT_CONCURRENCY: usize = 4;
/// Wall-clock budget per sub-agent before it is killed (the `timeout`
/// argument overrides).
pub const DEFAULT_TASK_TIMEOUT: u64 = 600;

#[derive(Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
    pub tools: Vec<String>,
    /// JSON Schema for the child's final output, from the definition's
    /// single-line `output_schema:` frontmatter field
    pub output_schema: Option<Value>,
    pub body: String,
}

/// Parse an agent definition: `---` yaml frontmatter `---` then the body,
/// which becomes the sub-agent's system prompt.
pub fn parse_agent_md(text: &str, fallback_name: &str) -> Option<AgentDef> {
    let (fm, body) = crate::yaml::split_frontmatter(text)?;
    let body = body
        .trim_start_matches('-')
        .trim_start_matches('\n')
        .to_string();
    let parsed = yaml::parse(fm).ok()?;
    let map = parsed.as_map()?;
    let name = map
        .get("name")
        .cloned()
        .unwrap_or_else(|| fallback_name.to_string());
    let description = map.get("description").cloned().unwrap_or_default();
    let model = map.get("model").cloned();
    let tools = map
        .get("tools")
        .map(|csv| {
            csv.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if name.is_empty() {
        return None;
    }
    let output_schema = map.get("output_schema").and_then(|j| {
        match serde_json::from_str::<Value>(j) {
            Ok(schema) => Some(schema),
            Err(e) => {
                eprintln!(
                    "Warning: agent '{name}' has an invalid output_schema (must be single-line JSON): {e}"
                );
                None
            }
        }
    });
    Some(AgentDef {
        name,
        description,
        model,
        tools,
        output_schema,
        body,
    })
}

/// Discover agent definitions: `user_dir/agents/*.md` plus the nearest
/// `.llm/agents/` walking up from cwd (project definitions override user
/// ones with the same name).
pub fn discover(user_dir: &Path, cwd: &Path) -> Vec<AgentDef> {
    let mut defs: Vec<AgentDef> = Vec::new();
    let load_dir = |dir: &Path, defs: &mut Vec<AgentDef>| {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let fallback = path.file_stem().and_then(|s| s.to_str()).unwrap_or("agent");
            if let Some(def) = parse_agent_md(&text, fallback) {
                defs.push(def);
            }
        }
    };
    load_dir(&user_dir.join("agents"), &mut defs);
    // nearest project dir wins
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let candidate = d.join(".llm/agents");
        if candidate.is_dir() {
            load_dir(&candidate, &mut defs);
            break;
        }
        dir = d.parent();
    }
    // later entries (project) override earlier (user) by name
    let mut merged: Vec<AgentDef> = Vec::new();
    for def in defs {
        match merged.iter().position(|d| d.name == def.name) {
            Some(i) => merged[i] = def,
            None => merged.push(def),
        }
    }
    merged
}

/// RAII guard: kills the child if it is still running when dropped, so an
/// aborted parent can never orphan a sub-agent process.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

/// Progress line channel: workers send pre-formatted dim lines, the thread
/// running `execute` drains them into the UI log while children run.
type Progress<'a> = Option<&'a std::sync::mpsc::Sender<String>>;

fn send_line(progress: Progress, line: String) {
    if let Some(tx) = progress {
        let _ = tx.send(line);
    }
}

/// `512 -> "512 chars"`, `4321 -> "4.3k chars"` — the live-output size of a
/// running sub-agent.
pub(crate) fn kchars(n: usize) -> String {
    if n < 1000 {
        format!("{n} chars")
    } else {
        format!("{:.1}k chars", n as f64 / 1000.0)
    }
}

/// Run jobs on scoped threads while streaming their progress lines through
/// `log`; returns the outputs in input order. The drain loop ends when the
/// last worker drops its sender, so every line is out before we join.
fn run_jobs<F>(jobs: Vec<F>, log: &mut dyn FnMut(&str)) -> Vec<ToolOutput>
where
    F: FnOnce(&std::sync::mpsc::Sender<String>) -> ToolOutput + Send,
{
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|job| {
                let tx = tx.clone();
                s.spawn(move || job(&tx))
            })
            .collect();
        drop(tx);
        for line in rx {
            log(&line);
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("sub-agent thread panicked"))
            .collect()
    })
}

/// Append with a hard byte cap; returns whether the cap was hit.
fn append_bounded(buf: &mut String, chunk: &str, cap: usize) -> bool {
    if buf.len() >= cap {
        return true;
    }
    let room = cap - buf.len();
    if chunk.len() <= room {
        buf.push_str(chunk);
        false
    } else {
        let end = crate::core::text::floor_boundary(chunk, room);
        buf.push_str(&chunk[..end]);
        buf.push_str("\n[output truncated]\n");
        true
    }
}

/// Replace `{previous}` with the previous step's output (unresolved tokens
/// stay visible so misses are obvious to the model).
fn apply_previous(task: &str, previous: &str) -> String {
    task.replace("{previous}", previous)
}

/// Carries the parent's model so children can inherit it, plus the
/// `[agent.roles]` map for a cheaper default sub-agent model.
#[derive(Default)]
pub struct TaskTool {
    pub parent_model: Option<String>,
    pub roles: std::collections::BTreeMap<String, String>,
}

impl TaskTool {
    fn model_for(&self, def: &AgentDef) -> Option<String> {
        def.model
            .clone()
            .or_else(|| self.roles.get("task").cloned())
            .or_else(|| self.parent_model.clone())
    }
}

impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn tier(&self) -> Tier {
        Tier::Exec
    }
    fn description(&self) -> &str {
        "Delegate work to a named sub-agent (definitions in agents/*.md). Modes: \
         {agent, task} for one; {tasks: [{agent, task}], concurrency} for parallel \
         (live per-agent progress, results in original order); {chain: [{agent, task}]} \
         for sequential where {previous} inserts the prior step's output."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string", "description": "Sub-agent name (single mode)"},
                "task": {"type": "string", "description": "Task text (single mode)"},
                "tasks": {"type": "array", "description": "Parallel mode", "items": {
                    "type": "object",
                    "properties": {"agent": {"type": "string"}, "task": {"type": "string"}},
                    "required": ["agent", "task"]
                }},
                "concurrency": {"type": "integer"},
                "timeout": {"type": "integer", "description": "Seconds before a sub-agent is killed (default 600)"},
                "outputSchema": {"type": "object", "description": "JSON Schema for the final output; overrides the agent definition's output_schema (single mode)"},
                "chain": {"type": "array", "description": "Sequential mode", "items": {
                    "type": "object",
                    "properties": {"agent": {"type": "string"}, "task": {"type": "string"}},
                    "required": ["agent", "task"]
                }}
            }
        })
    }
    fn preview(&self, args: &Value) -> String {
        let count = args["tasks"]
            .as_array()
            .or_else(|| args["chain"].as_array())
            .map(|a| a.len());
        match count {
            Some(n) => format!("fan out {} sub-agent task(s)", n),
            None => format!(
                "sub-agent '{}': {}",
                args["agent"].as_str().unwrap_or("?"),
                short(args["task"].as_str().unwrap_or("")),
            ),
        }
    }
    fn execute(&self, args: &Value, cwd: &Path, log: &mut dyn FnMut(&str)) -> ToolOutput {
        let depth: u32 = std::env::var("LLM_AGENT_DEPTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if depth >= MAX_DEPTH {
            return ToolOutput::err(format!("sub-agent nesting limit ({MAX_DEPTH}) reached"));
        }
        let timeout = args["timeout"]
            .as_u64()
            .unwrap_or(DEFAULT_TASK_TIMEOUT)
            .max(1);
        let defs = discover(&crate::core::config::user_dir(), cwd);

        if let Some(tasks) = args["tasks"].as_array() {
            if tasks.len() > MAX_PARALLEL {
                return ToolOutput::err(format!("at most {MAX_PARALLEL} parallel tasks"));
            }
            let concurrency = args["concurrency"]
                .as_u64()
                .unwrap_or(DEFAULT_CONCURRENCY as u64) as usize;
            let mut specs = Vec::new();
            for t in tasks {
                let Some(name) = t["agent"].as_str() else {
                    return ToolOutput::err("each task needs an agent name");
                };
                let def = match lookup_def(&defs, name) {
                    Ok(d) => d,
                    Err(e) => return e,
                };
                let schema = schema_for(&def, t);
                specs.push((def, t["task"].as_str().unwrap_or("").to_string(), schema));
            }
            let mut results: Vec<(String, ToolOutput)> = Vec::new();
            for chunk in specs.chunks(concurrency.max(1)) {
                let names: Vec<String> = chunk.iter().map(|(d, _, _)| d.name.clone()).collect();
                let jobs: Vec<_> = chunk
                    .iter()
                    .map(|(def, task, schema)| {
                        spawn_job(
                            def,
                            task,
                            cwd,
                            depth,
                            self.model_for(def),
                            schema.as_ref(),
                            timeout,
                        )
                    })
                    .collect();
                let outs = run_jobs(jobs, log);
                results.extend(names.into_iter().zip(outs));
            }
            let sections: Vec<String> = results
                .iter()
                .map(|(name, out)| render_section(name, out))
                .collect();
            let any_error = results.iter().any(|(_, o)| o.is_error);
            let out = sections.join("\n");
            return if any_error {
                ToolOutput::err(out)
            } else {
                ToolOutput::ok(out)
            };
        }

        if let Some(chain) = args["chain"].as_array() {
            let mut sections: Vec<String> = Vec::new();
            let mut previous = String::new();
            for (i, step) in chain.iter().enumerate() {
                let Some(name) = step["agent"].as_str() else {
                    return ToolOutput::err("each chain step needs an agent name");
                };
                let def = match lookup_def(&defs, name) {
                    Ok(d) => d,
                    Err(e) => return e,
                };
                let task = apply_previous(step["task"].as_str().unwrap_or(""), &previous);
                let schema = schema_for(&def, step);
                let step_name = def.name.clone();
                let mut out = run_one(
                    spawn_job(
                        &def,
                        &task,
                        cwd,
                        depth,
                        self.model_for(&def),
                        schema.as_ref(),
                        timeout,
                    ),
                    log,
                );
                let is_error = out.is_error;
                let content = std::mem::take(&mut out.content);
                previous = content.clone();
                sections.push(format!("## step {}: {}\n{}", i + 1, step_name, content));
                if is_error {
                    return ToolOutput::err(sections.join("\n"));
                }
            }
            return ToolOutput::ok(sections.join("\n"));
        }

        // single mode
        let Some(name) = args["agent"].as_str() else {
            return ToolOutput::err("provide agent+task, tasks[] or chain[]");
        };
        let def = match lookup_def(&defs, name) {
            Ok(d) => d,
            Err(e) => return e,
        };
        let schema = schema_for(&def, args);
        let task = args["task"].as_str().unwrap_or("").to_string();
        let agent_name = def.name.clone();
        let out = run_one(
            spawn_job(
                &def,
                &task,
                cwd,
                depth,
                self.model_for(&def),
                schema.as_ref(),
                timeout,
            ),
            log,
        );
        render_section_result(&agent_name, out)
    }
}

pub(crate) fn short(s: &str) -> String {
    let mut out: String = s.chars().take(60).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}

/// Resolve an agent name, or the unknown-agent error naming what exists.
fn lookup_def(defs: &[AgentDef], name: &str) -> Result<AgentDef, ToolOutput> {
    defs.iter()
        .find(|d| d.name == name)
        .cloned()
        .ok_or_else(|| ToolOutput::err(format!("unknown agent '{name}'{}", list_defs(defs))))
}

/// One child spawn packaged as a run_jobs job (owning its inputs).
fn spawn_job(
    def: &AgentDef,
    task: &str,
    cwd: &Path,
    depth: u32,
    model: Option<String>,
    schema: Option<&Value>,
    timeout: u64,
) -> impl FnOnce(&std::sync::mpsc::Sender<String>) -> ToolOutput + Send {
    let def = def.clone();
    let task = task.to_string();
    let schema = schema.cloned();
    move |tx| {
        run_child(
            &def,
            &task,
            cwd,
            depth,
            model.as_deref(),
            schema.as_ref(),
            timeout,
            Some(tx),
        )
    }
}

/// Run a single job through the shared pool plumbing.
fn run_one<F: FnOnce(&std::sync::mpsc::Sender<String>) -> ToolOutput + Send>(
    job: F,
    log: &mut dyn FnMut(&str),
) -> ToolOutput {
    run_jobs(vec![job], log).pop().expect("one job, one output")
}

fn list_defs(defs: &[AgentDef]) -> String {
    if defs.is_empty() {
        " (no agents defined; add user or .llm/agents/*.md files)".to_string()
    } else {
        format!(
            " (available: {})",
            defs.iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn render_section(name: &str, out: &ToolOutput) -> String {
    if out.is_error {
        format!("## {name} (failed)\n{}", out.content)
    } else {
        format!("## {name}\n{}", out.content)
    }
}

fn render_section_result(name: &str, out: ToolOutput) -> ToolOutput {
    if out.is_error {
        ToolOutput::err(render_section(name, &out))
    } else {
        ToolOutput::ok(render_section(name, &out))
    }
}

/// The per-call schema: the task's `outputSchema` overrides the agent
/// definition's `output_schema` when both are set.
fn schema_for(def: &AgentDef, call: &Value) -> Option<Value> {
    if call["outputSchema"].is_object() {
        Some(call["outputSchema"].clone())
    } else {
        def.output_schema.clone()
    }
}

/// Spawn this binary as a headless sub-agent and collect its final text.
/// The child's stdout is parsed on its own thread while this loop slices
/// the wait, so esc interrupts and the wall-clock timeout both land
/// promptly instead of blocking on the next output line.
#[allow(clippy::too_many_arguments)]
fn run_child(
    def: &AgentDef,
    task: &str,
    cwd: &Path,
    depth: u32,
    model: Option<&str>,
    schema: Option<&Value>,
    timeout: u64,
    progress: Progress<'_>,
) -> ToolOutput {
    let Ok(exe) = std::env::current_exe() else {
        return ToolOutput::err("cannot resolve current executable");
    };
    let tools_csv = if def.tools.is_empty() {
        DEFAULT_CHILD_TOOLS.to_string()
    } else {
        def.tools.join(",")
    };
    // an output schema rides the system prompt: the final message must be
    // one JSON value the parent can validate
    let system = match schema {
        Some(schema) => format!(
            "{}\n\n## Output format\nYour final message must be exactly one JSON value \
             matching this schema (no prose, no code fence):\n{}",
            def.body,
            serde_json::to_string(schema).unwrap_or_default()
        ),
        None => def.body.clone(),
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "agent",
        "--mode",
        "json",
        "--no-session",
        "--approval-mode",
        "yolo",
        "--tools",
        &tools_csv,
        "--system-prompt",
        &system,
    ]);
    if let Some(model) = model {
        cmd.arg("-m").arg(model);
    }
    cmd.arg(format!("Task: {task}"));
    cmd.current_dir(cwd)
        .env("LLM_AGENT_DEPTH", (depth + 1).to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            send_line(progress, format!("{} · failed: cannot spawn", def.name));
            return ToolOutput::err(format!("cannot spawn sub-agent: {e}"));
        }
    };
    send_line(progress, format!("{} · {}", def.name, short(task)));
    let mut guard = ChildGuard(child);
    let stdout = guard.0.stdout.take();
    // drain stderr on its own thread so a chatty child can never block on a
    // full pipe while we are still reading stdout
    let mut stderr_handle = guard.0.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = err.read_to_string(&mut buf);
            buf
        })
    });

    /// One parsed JSONL event from the child.
    enum ChildEvent {
        Delta(String),
        Done(Option<String>),
        Error(Option<String>),
    }
    let mut text = String::new();
    let mut done_text: Option<String> = None;
    let mut error: Option<String> = None;
    let mut timed_out = false;
    let mut interrupted = false;
    // sub-agent token spend, surfaced on the done progress line (children
    // bill the same API account but the parent's usage counter never sees it)
    let (in_tx, in_rx) = std::sync::mpsc::channel::<(u64, u64, u64)>();
    let mut child_tokens: (u64, u64, u64) = (0, 0, 0);
    // live progress: one heartbeat line per running child, throttled so a
    // fast talker cannot flood the log
    let mut last_beat = std::time::Instant::now();
    let reader = stdout.map(|stdout| {
        let (tx, rx) = std::sync::mpsc::channel::<ChildEvent>();
        let usage_tx = in_tx.clone();
        let handle = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(|l| l.ok())
            {
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let forwarded = match event["type"].as_str() {
                    Some("delta") => event["text"]
                        .as_str()
                        .map(|t| ChildEvent::Delta(t.to_string())),
                    Some("done") => {
                        Some(ChildEvent::Done(event["text"].as_str().map(str::to_string)))
                    }
                    Some("turn_end") => {
                        if let Some(u) = event["usage"].as_array() {
                            let n = |i: usize| u.get(i).and_then(|v| v.as_u64()).unwrap_or(0);
                            let _ = usage_tx.send((n(0), n(1), n(2)));
                        }
                        None
                    }
                    Some("error") => Some(ChildEvent::Error(
                        event["message"].as_str().map(str::to_string),
                    )),
                    _ => None,
                };
                if let Some(event) = forwarded
                    && tx.send(event).is_err()
                {
                    break;
                }
            }
        });
        (handle, rx)
    });
    if let Some((handle, rx)) = reader {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(ChildEvent::Delta(t)) => {
                    append_bounded(&mut text, &t, MAX_OUTPUT_BYTES);
                    if last_beat.elapsed() >= std::time::Duration::from_secs(2) {
                        last_beat = std::time::Instant::now();
                        send_line(progress, format!("{} · {}", def.name, kchars(text.len())));
                    }
                }
                Ok(ChildEvent::Done(t)) => done_text = t,
                Ok(ChildEvent::Error(m)) => {
                    error = m;
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if crate::core::http::interrupted() {
                        interrupted = true;
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        timed_out = true;
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // the reader thread blocks on the child's stdout, so a child that is
        // still running (timeout, interrupt, error) must die before the join
        // below — otherwise the join waits for it to finish on its own and
        // the deadline never actually stops anything
        if timed_out || error.is_some() || interrupted {
            let _ = guard.0.kill();
        }
        drop(rx);
        let _ = handle.join();
        while let Ok(u) = in_rx.try_recv() {
            child_tokens.0 += u.0;
            child_tokens.1 += u.1;
            child_tokens.2 += u.2;
        }
    }
    let status = guard.0.wait();
    if interrupted {
        send_line(progress, format!("{} · interrupted", def.name));
        return ToolOutput::err("sub-agent interrupted by user".to_string());
    }
    if timed_out {
        send_line(progress, format!("{} · timed out", def.name));
        return ToolOutput::err(format!(
            "sub-agent timed out after {timeout}s:\n{}",
            done_text.filter(|t| !t.is_empty()).unwrap_or(text)
        ));
    }
    if let Some(e) = error {
        send_line(progress, format!("{} · failed", def.name));
        return ToolOutput::err(format!("sub-agent failed: {e}"));
    }
    let mut final_text = done_text.filter(|t| !t.is_empty()).unwrap_or(text);
    // the done event rides outside the bounded delta stream: cap it too
    if final_text.len() > MAX_OUTPUT_BYTES {
        let end = crate::core::text::floor_boundary(&final_text, MAX_OUTPUT_BYTES);
        final_text = format!("{}\n[output truncated]", &final_text[..end]);
    }
    send_line(
        progress,
        format!(
            "{} · done ({} · ↑{} ↓{})",
            def.name,
            kchars(final_text.len()),
            crate::core::render::humanize_tokens(child_tokens.0),
            crate::core::render::humanize_tokens(child_tokens.1)
        ),
    );
    let stderr_text = stderr_handle
        .take()
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    match status {
        Ok(s) if s.success() => {
            if final_text.trim().is_empty() {
                return ToolOutput::err("sub-agent produced no output");
            }
            let Some(schema) = schema else {
                return ToolOutput::ok(final_text);
            };
            match extract_json(&final_text) {
                None => ToolOutput::err(format!(
                    "sub-agent output contains no JSON value:\n{final_text}"
                )),
                Some(value) => {
                    let errors = validate_json_schema(&value, schema);
                    if errors.is_empty() {
                        ToolOutput::ok(serde_json::to_string(&value).unwrap_or(final_text))
                    } else {
                        ToolOutput::err(format!(
                            "sub-agent output failed its schema ({}):\n{}",
                            errors.join("; "),
                            serde_json::to_string(&value).unwrap_or_default()
                        ))
                    }
                }
            }
        }
        Ok(s) => {
            let mut msg = format!("sub-agent exited with {s}:\n{final_text}");
            if !stderr_text.trim().is_empty() {
                msg.push_str(&format!("\nstderr:\n{}", stderr_text.trim_end()));
            }
            ToolOutput::err(msg)
        }
        Err(e) => ToolOutput::err(format!("sub-agent wait failed: {e}")),
    }
}

/// Locate the JSON value in a child's final text: a direct parse, a fenced
/// ```json block, or the first balanced object/array region, in that order.
fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    for fence in ["```json", "```"] {
        if let Some(start) = trimmed.find(fence)
            && let Some(end) = trimmed[start + fence.len()..].find("```")
            && let Ok(value) = serde_json::from_str::<Value>(
                trimmed[start + fence.len()..start + fence.len() + end].trim(),
            )
        {
            return Some(value);
        }
    }
    for open in ['{', '['] {
        if let Some(start) = trimmed.find(open)
            && let Some(region) = crate::core::text::balanced_json_region(&trimmed[start..])
            && let Ok(value) = serde_json::from_str::<Value>(region)
        {
            return Some(value);
        }
    }
    None
}

/// Validate a value against a pragmatic JSON Schema subset: type, enum,
/// properties, required, items (nested). Empty vec = valid. Hand-rolled —
/// the dependency budget has no room for a schema crate.
fn validate_json_schema(value: &Value, schema: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    validate_at(value, schema, "", &mut errors);
    errors
}

fn validate_at(value: &Value, schema: &Value, path: &str, errors: &mut Vec<String>) {
    let label = || if path.is_empty() { "value" } else { path };
    if let Some(expected) = schema["type"].as_str() {
        let ok = match (expected, value) {
            ("object", Value::Object(_))
            | ("array", Value::Array(_))
            | ("string", Value::String(_))
            | ("boolean", Value::Bool(_))
            | ("null", Value::Null)
            | ("number", Value::Number(_)) => true,
            ("integer", Value::Number(n)) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        };
        if !ok {
            errors.push(format!("{}: expected {expected}", label()));
        }
    }
    if let Some(allowed) = schema["enum"].as_array()
        && !allowed.contains(value)
    {
        errors.push(format!("{}: not one of the allowed values", label()));
    }
    if let (Some(props), Value::Object(map)) = (schema["properties"].as_object(), value) {
        for (key, sub) in props {
            if let Some(item) = map.get(key) {
                validate_at(item, sub, &format!("{path}.{key}"), errors);
            }
        }
    }
    if let (Some(required), Value::Object(map)) = (schema["required"].as_array(), value) {
        for key in required.iter().filter_map(|k| k.as_str()) {
            if !map.contains_key(key) {
                errors.push(format!("{}: missing required property '{key}'", label()));
            }
        }
    }
    if let (Some(items), Value::Array(entries)) = (schema.get("items"), value) {
        for (i, entry) in entries.iter().enumerate() {
            validate_at(entry, items, &format!("{path}[{i}]"), errors);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nname: researcher\ndescription: finds things\nmodel: mock/m1\ntools: read, grep\n---\nYou are a researcher.\nUse tools well.\n";

    #[test]
    fn parses_output_schema_frontmatter() {
        let md = "---\nname: s\ndescription: d\noutput_schema: {\"type\":\"object\",\"required\":[\"ok\"]}\n---\nbody";
        let def = parse_agent_md(md, "f").unwrap();
        assert_eq!(
            def.output_schema,
            Some(json!({"type": "object", "required": ["ok"]}))
        );
        // invalid single-line JSON degrades to none (with a warning)
        let bad = "---\nname: s\ndescription: d\noutput_schema: {oops\n---\nbody";
        assert!(parse_agent_md(bad, "f").unwrap().output_schema.is_none());
        // the sample has none
        assert!(parse_agent_md(SAMPLE, "f").unwrap().output_schema.is_none());
    }

    #[test]
    fn extracts_json_direct_fence_and_bare() {
        assert_eq!(extract_json("{\"a\":1}"), Some(json!({"a": 1})));
        assert_eq!(
            extract_json("result:\n```json\n{\"a\": [1,2]}\n```\ndone"),
            Some(json!({"a": [1, 2]}))
        );
        assert_eq!(
            extract_json("The answer is {\"nested\": {\"deep\": true}} period."),
            Some(json!({"nested": {"deep": true}}))
        );
        assert_eq!(extract_json("no json here"), None);
    }

    #[test]
    fn balanced_region_honors_strings() {
        let text = r#"{"s": "a \" quote ] here", "t": 1} trailing"#;
        let region = crate::core::text::balanced_json_region(text).unwrap();
        let parsed: Value = serde_json::from_str(region).unwrap();
        assert_eq!(parsed["t"], json!(1));
    }

    #[test]
    fn schema_validation_catches_type_required_enum() {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "status": {"enum": ["ok", "bad"]}
            }
        });
        assert!(
            validate_json_schema(
                &json!({"name": "x", "tags": ["a"], "status": "ok"}),
                &schema
            )
            .is_empty()
        );
        let errs = validate_json_schema(&json!({"tags": ["a", 2], "status": "meh"}), &schema);
        let joined = errs.join("; ");
        assert!(
            joined.contains("missing required property 'name'"),
            "{joined}"
        );
        assert!(joined.contains(".tags[1]: expected string"), "{joined}");
        assert!(joined.contains(".status: not one of"), "{joined}");
        assert!(
            validate_json_schema(&json!({"name": 3}), &schema)[0]
                .contains(".name: expected string")
        );
        assert!(validate_json_schema(&json!("x"), &schema)[0].contains("expected object"));
        // integer accepts whole floats, rejects fractions
        assert!(validate_json_schema(&json!(2.0), &json!({"type": "integer"})).is_empty());
        assert!(!validate_json_schema(&json!(2.5), &json!({"type": "integer"})).is_empty());
    }

    #[test]
    fn call_schema_overrides_definition() {
        let def = AgentDef {
            name: "n".into(),
            description: String::new(),
            model: None,
            tools: Vec::new(),
            output_schema: Some(json!({"type": "object"})),
            body: "b".into(),
        };
        let call = json!({"outputSchema": {"type": "array"}});
        assert_eq!(schema_for(&def, &call), Some(json!({"type": "array"})));
        assert_eq!(
            schema_for(&def, &json!({})),
            Some(json!({"type": "object"}))
        );
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let def = parse_agent_md(SAMPLE, "fallback").unwrap();
        assert_eq!(def.name, "researcher");
        assert_eq!(def.description, "finds things");
        assert_eq!(def.model.as_deref(), Some("mock/m1"));
        assert_eq!(def.tools, vec!["read", "grep"]);
        assert_eq!(def.body, "You are a researcher.\nUse tools well.\n");
    }

    #[test]
    fn falls_back_to_filename_and_tolerates_missing_fields() {
        let def = parse_agent_md("---\ndescription: d\n---\nbody", "myfile").unwrap();
        assert_eq!(def.name, "myfile");
        assert!(def.model.is_none());
        assert!(def.tools.is_empty());
        assert_eq!(def.body, "body");
    }

    #[test]
    fn no_frontmatter_is_rejected() {
        assert!(parse_agent_md("just a body", "x").is_none());
    }

    #[test]
    fn project_overrides_user_by_name() {
        let tmp = std::env::temp_dir().join(format!("llm-agents-{}", std::process::id()));
        let user = tmp.join("user");
        let project = tmp.join("proj");
        std::fs::create_dir_all(user.join("agents")).unwrap();
        std::fs::create_dir_all(project.join(".llm/agents")).unwrap();
        std::fs::write(
            user.join("agents/shared.md"),
            "---\nname: shared\ndescription: user\n---\nuser body",
        )
        .unwrap();
        std::fs::write(
            user.join("agents/onlyuser.md"),
            "---\nname: onlyuser\ndescription: u\n---\nub",
        )
        .unwrap();
        std::fs::write(
            project.join(".llm/agents/shared.md"),
            "---\nname: shared\ndescription: project\n---\nproject body",
        )
        .unwrap();

        let defs = discover(&user, &project);
        assert_eq!(defs.len(), 2);
        let shared = defs.iter().find(|d| d.name == "shared").unwrap();
        assert_eq!(shared.body, "project body");
        assert!(defs.iter().any(|d| d.name == "onlyuser"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn previous_templating() {
        assert_eq!(
            apply_previous("summarize {previous} now", "OUT"),
            "summarize OUT now"
        );
        assert_eq!(apply_previous("no token", "OUT"), "no token");
        assert_eq!(
            apply_previous("twice {previous} {previous}", "X"),
            "twice X X"
        );
    }

    #[test]
    fn kchars_formats_sizes() {
        assert_eq!(kchars(512), "512 chars");
        assert_eq!(kchars(4321), "4.3k chars");
        assert_eq!(kchars(1000), "1.0k chars");
    }

    #[test]
    fn run_jobs_streams_progress_before_results() {
        let mut seen: Vec<String> = Vec::new();
        let outs = run_jobs(
            vec![
                |tx: &std::sync::mpsc::Sender<String>| {
                    tx.send("a · start".into()).unwrap();
                    ToolOutput::ok("one")
                },
                |tx: &std::sync::mpsc::Sender<String>| {
                    tx.send("b · start".into()).unwrap();
                    ToolOutput::ok("two")
                },
            ],
            &mut |line: &str| seen.push(line.to_string()),
        );
        assert_eq!(outs[0].content, "one");
        assert_eq!(outs[1].content, "two");
        assert!(seen.contains(&"a · start".to_string()));
        assert!(seen.contains(&"b · start".to_string()));
    }

    #[test]
    fn bounded_append_caps_output() {
        let mut buf = String::new();
        assert!(!append_bounded(&mut buf, "hello", 10));
        assert!(append_bounded(&mut buf, " world and more", 10));
        assert!(buf.contains("[output truncated]"));
        assert!(buf.len() < 40);
    }
}
