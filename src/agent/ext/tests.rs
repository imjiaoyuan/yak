use super::manifest::{discover_in, parse_tool_manifest};
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

/// `extensions.disabled` must reach a script tool by its declared name,
/// not only by the file's stem: packages ship scripts whose stem says
/// nothing about the tool inside, and two files may declare one name.
#[test]
fn disabled_matches_a_script_tool_by_declared_name_too() {
    let dir = std::env::temp_dir().join(format!("llm-ext-disc-{}", crate::core::db::ulid()));
    std::fs::create_dir_all(&dir).unwrap();
    // one script tool named `search`, stem unrelated; one executable
    // resident (name spelled per platform: Windows recognizes an
    // executable by extension, unix by the exec bit) for the stem path
    std::fs::write(
        dir.join("helper.py"),
        "# --- llm-tool: search\n# description: find things\n",
    )
    .unwrap();
    #[cfg(unix)]
    let resident = dir.join("other");
    #[cfg(windows)]
    let resident = dir.join("other.bat");
    std::fs::write(&resident, b"#!/bin/sh\n:").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&resident, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // nothing disabled: both surface
    let found = discover_in(std::slice::from_ref(&dir), &[]);
    assert_eq!(found.script_tools.len(), 1);
    assert_eq!(found.script_tools[0].name, "search");
    assert_eq!(found.resident.len(), 1, "the executable resident surfaces");

    // by declared tool name: the script tool is gone, the resident stays
    let found = discover_in(std::slice::from_ref(&dir), &["search".to_string()]);
    assert!(found.script_tools.is_empty(), "declared name disables");
    assert_eq!(found.resident.len(), 1);

    // by file stem: the script tool is gone too (the old spelling)
    let found = discover_in(std::slice::from_ref(&dir), &["helper".to_string()]);
    assert!(found.script_tools.is_empty(), "stem still disables");
    assert_eq!(found.resident.len(), 1);

    // and the resident still falls to its own stem
    let found = discover_in(std::slice::from_ref(&dir), &["other".to_string()]);
    assert_eq!(found.script_tools.len(), 1);
    assert!(found.resident.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
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
    let read =
        parse_tool_manifest(&format!("{base}# tier: read\n"), Path::new("/x/t")).expect("manifest");
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
fn the_interrupt_notice_names_the_abandoned_call() {
    let frame = interrupt_frame(41);
    assert!(
        frame.ends_with('\n'),
        "frames are newline-terminated: {frame:?}"
    );
    let msg: Value = serde_json::from_str(frame.trim()).unwrap();
    assert_eq!(msg["type"], "interrupt");
    assert_eq!(msg["cancelled"], 41);
    assert!(
        msg["id"].is_u64(),
        "the frame keeps the protocol shape: {msg}"
    );
}

#[test]
fn an_extension_may_ask_for_a_longer_call_budget() {
    // no request: the host's config default rules
    assert_eq!(parse_tool_timeout(&json!({})), None);
    // zero, negative and non-numeric requests are ignored
    assert_eq!(parse_tool_timeout(&json!({"tool_timeout": 0})), None);
    assert_eq!(parse_tool_timeout(&json!({"tool_timeout": -5})), None);
    assert_eq!(parse_tool_timeout(&json!({"tool_timeout": "900"})), None);
    assert_eq!(
        parse_tool_timeout(&json!({"tool_timeout": 900})),
        Some(Duration::from_secs(900))
    );
    // ...and the ceiling holds, so a wedged call cannot park the turn
    // for a day
    assert_eq!(
        parse_tool_timeout(&json!({"tool_timeout": 86400})),
        Some(Duration::from_secs(3600))
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
    let mut taken: std::collections::HashSet<String> = ["read".to_string()].into_iter().collect();
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
    assert_eq!(
        ext.call_tool("ping", &json!({}), &mut |_| {}).unwrap(),
        "alive"
    );
    // the child exited after answering: the next call must respawn it
    let out = ext.call_tool("ping", &json!({}), &mut |_| {});
    assert_eq!(out.unwrap(), "alive", "respawned, not an error");
    let n: u32 = std::fs::read_to_string(&counter)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(n >= 2, "the script must have run twice, saw {n}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// stderr written while a call is in flight is live progress: the tool
/// log sees it, the model-facing result does not.
#[cfg(unix)]
#[test]
fn a_working_extension_streams_stderr_to_the_tool_log() {
    let dir = std::env::temp_dir().join(format!("llm-ext-progress-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("chatty");
    let body = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *initialize*)
      printf '{"id":%s,"result":{"tools":[{"name":"ping","parameters":{}}]}}\n' "$id"
      ;;
    *call_tool*)
      echo "step one" >&2
      echo "step two" >&2
      sleep 0.3
      printf '{"id":%s,"result":"done"}\n' "$id"
      ;;
  esac
done
"#;
    std::fs::write(&script, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let ext = connect_stub(&script);
    assert!(
        lock(&ext.state).is_ok(),
        "initial handshake must succeed; state says {:?}",
        lock(&ext.state).as_ref().err()
    );
    let mut log: Vec<String> = Vec::new();
    let out = ext
        .call_tool("ping", &json!({}), &mut |line| log.push(line.to_string()))
        .unwrap();
    assert_eq!(out, "done", "stderr is progress, never part of the result");
    assert!(
        log.iter().any(|l| l.contains("step one")) && log.iter().any(|l| l.contains("step two")),
        "stderr written during the call must reach the tool log: {log:?}"
    );
    // and the diagnostics tail still carries them for /status
    assert!(
        lock(&ext.tail).iter().any(|l| l.contains("step two")),
        "the tail must keep stderr too: {:?}",
        *lock(&ext.tail)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A line that is not valid UTF-8 is decoded lossily, not treated as the
/// end of the stream: an extension under a legacy codepage still answers
/// and still reports progress.
#[cfg(unix)]
#[test]
fn a_non_utf8_line_does_not_end_the_stream() {
    let dir = std::env::temp_dir().join(format!("llm-ext-lossy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("codepage");
    let body = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *initialize*)
      printf '{"id":%s,"result":{"tools":[{"name":"ping","parameters":{}}]}}\n' "$id"
      ;;
    *call_tool*)
      printf '\377\376 cp1252 bytes\n' >&2
      echo "still here" >&2
      printf 'garbage \377\376 line\n'
      sleep 0.2
      printf '{"id":%s,"result":"done"}\n' "$id"
      ;;
  esac
done
"#;
    std::fs::write(&script, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let ext = connect_stub(&script);
    let mut log: Vec<String> = Vec::new();
    let out = ext.call_tool("ping", &json!({}), &mut |line| log.push(line.to_string()));
    assert_eq!(
        out.unwrap(),
        "done",
        "the reply after the garbled stdout line must still arrive"
    );
    assert!(
        log.iter().any(|l| l.contains("still here")),
        "stderr must keep flowing after an invalid byte: {log:?}"
    );
    assert!(
        log.iter().any(|l| l.contains("cp1252")),
        "the invalid line is kept, decoded lossily: {log:?}"
    );
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
    let untouched = json!({"tool": "bash", "summary": "s", "is_error": false, "content": "plain"});
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
