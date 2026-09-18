use super::edit::change_hunks;
use super::fetch::{extract_title, html_to_text};
use super::recall::RECALL_CHARS;
use super::recall::recall_page;
use super::write::write_atomic;
use super::*;

#[test]
fn recall_pages_by_char_offset_and_marks_the_end() {
    let text = "中".repeat(RECALL_CHARS + 5);
    let (start, next, eof, chunk) = recall_page(&text, 0);
    assert_eq!((start, eof), (0, false));
    assert_eq!(next, RECALL_CHARS);
    assert_eq!(chunk.chars().count(), RECALL_CHARS, "one full page");
    // continuing from next_offset returns the remainder and stops
    let (start, next, eof, chunk) = recall_page(&text, next);
    assert_eq!((start, eof), (RECALL_CHARS, true));
    assert_eq!(next, RECALL_CHARS + 5);
    assert_eq!(chunk.chars().count(), 5);
    // a past-the-end offset is clamped, not a panic
    let (start, next, eof, chunk) = recall_page("abc", 99);
    assert_eq!((start, next, eof, chunk.as_str()), (3, 3, true, ""));
}

#[test]
fn the_registry_exposes_recall_as_a_read_tier_tool() {
    let tools = builtin_tools();
    let recall = tools
        .iter()
        .find(|t| t.name() == "recall")
        .expect("mounted");
    assert_eq!(
        recall.tier(),
        Tier::Read,
        "reading a local archive is not exec"
    );
    // the id is required, the offset optional
    assert_eq!(recall.parameters()["required"][0], "id");
    assert!(validate(&recall.parameters(), &json!({"id": "01jz"})).is_ok());
    assert!(validate(&recall.parameters(), &json!({})).is_err());
}

#[test]
fn then_run_is_declared_on_both_mutating_tools() {
    let tools = builtin_tools();
    for name in ["write", "edit"] {
        let tool = tools.iter().find(|t| t.name() == name).unwrap();
        let props = &tool.parameters()["properties"];
        assert_eq!(props["then_run"]["type"], "string", "{name}");
        assert!(
            props["then_run"]["description"]
                .as_str()
                .unwrap()
                .contains("same tool call"),
            "{name} must tell the model this is fused, not a second call"
        );
        // the fused field passes validation alongside the tool's own args
        let args = if name == "write" {
            json!({"path": "x", "content": "y", "then_run": "cargo test"})
        } else {
            json!({"path": "x", "edits": [], "then_run": "cargo test"})
        };
        assert!(validate(&tool.parameters(), &args).is_ok(), "{name}");
    }
}

#[test]
fn atomic_write_replaces_content_and_leaves_no_temp_behind() {
    let dir = crate::core::testutil::scratch_dir("atomic");
    let path = dir.join("doc.md");
    std::fs::write(&path, "old").unwrap();

    write_atomic(&path, b"new content").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.unwrap().file_name().into_string().ok())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");

    // a fresh file lands too (no permissions to carry over)
    let fresh = dir.join("new.txt");
    write_atomic(&fresh, b"hi").unwrap();
    assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "hi");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn atomic_write_keeps_the_existing_mode_over_the_rename() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::core::testutil::scratch_dir("atomic-mode");
    let path = dir.join("run.sh");
    std::fs::write(&path, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

    write_atomic(&path, b"#!/bin/sh\necho ok\n").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "the executable bit must survive the rewrite");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_change_hunks_single_edit_with_context() {
    let original = "one\ntwo\nthree\nfour\nfive\n";
    let start = original.find("three").unwrap();
    let spans = [(start, start + 5, "THREE")];
    let out = change_hunks(original, &spans, 1, 30);
    assert_eq!(out, "@@ line 2\n  two\n- three\n+ THREE\n  four");
}

#[test]
fn test_change_hunks_distant_edits_open_two_hunks() {
    let original = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
    let s1 = original.find("b").unwrap();
    let s2 = original.find("i").unwrap();
    let spans = [(s1, s1 + 1, "B"), (s2, s2 + 1, "I")];
    let out = change_hunks(original, &spans, 1, 30);
    assert_eq!(
        out,
        "@@ line 1\n  a\n- b\n+ B\n  c\n@@ line 8\n  h\n- i\n+ I\n  j"
    );
}

#[test]
fn test_change_hunks_deletion_has_no_empty_addition() {
    let original = "x\ny\nz\n";
    let start = original.find("y").unwrap();
    let spans = [(start, start + 2, "")]; // "y\n" removed
    let out = change_hunks(original, &spans, 0, 30);
    assert_eq!(out, "@@ line 2\n- y");
}

#[test]
fn test_change_hunks_caps_output() {
    let original: String = (0..50).map(|i| format!("line{i}\n")).collect();
    let spans = [(0, original.len(), "new content")];
    let out = change_hunks(&original, &spans, 2, 5);
    assert_eq!(out.lines().count(), 6); // 5 rows + the marker
    assert!(out.ends_with("· more lines not shown"));
}

#[test]
fn test_change_hunks_multiline_replacement() {
    let original = "fn a() {}\nfn b() {}\n";
    let start = original.find("fn b() {}").unwrap();
    let spans = [(start, start + 10, "fn b() {\n    todo!();\n}")];
    let out = change_hunks(original, &spans, 1, 30);
    assert_eq!(
        out,
        "@@ line 1\n  fn a() {}\n- fn b() {}\n+ fn b() {\n+     todo!();\n+ }"
    );
}

#[test]
fn test_change_hunks_trailing_newline_no_artifact() {
    let original = "first line\nhello world\nlast line\n";
    let start = original.find("hello world").unwrap();
    let spans = [(start, start + 11, "hello diff preview")];
    let out = change_hunks(original, &spans, 2, 30);
    println!("{out}");
    assert_eq!(
        out,
        "@@ line 1\n  first line\n- hello world\n+ hello diff preview\n  last line"
    );
}

#[test]
fn truncate_tail_caps_lines_and_bytes() {
    let text = (1..=3000)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let (tail, truncated) = truncate_tail(&text, MAX_LINES, MAX_BYTES);
    assert!(truncated);
    assert!(tail.lines().next().unwrap().parse::<usize>().unwrap() > 1000);
    let wide = vec!["x".repeat(300); 3000].join("\n");
    let (tail, truncated) = truncate_tail(&wide, MAX_LINES, MAX_BYTES);
    assert!(truncated);
    assert!(tail.len() <= MAX_BYTES + 300);
}

/// A fetched page reads top-down, so its cut keeps the head — the opposite
/// of a command's output. Same three caps either way, so no tool can hand the
/// model a result the others are capped below.
#[test]
fn the_head_cut_keeps_the_beginning_and_holds_every_cap() {
    // line cap: the first lines survive, the tail is dropped
    let text = (1..=3000)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let head = truncate_head_marked(&text);
    assert!(head.starts_with("1\n2\n3\n"), "{}…", &head[..20]);
    assert!(head.ends_with("[output truncated]\n"));
    assert_eq!(
        head.lines().count(),
        MAX_LINES + 1,
        "2000 lines + the marker"
    );

    // byte cap: one huge line, cut on a char boundary
    let wide = "x".repeat(MAX_BYTES * 2);
    let head = truncate_head_marked(&wide);
    assert!(head.len() < MAX_BYTES + 64, "{} bytes", head.len());

    // token cap: the byte cap is not a token cap, and CJK costs ~1 token per
    // char, so without this the same byte count would cost 3-4x more
    let cjk = "中".repeat(MAX_BYTES);
    let head = truncate_head_marked(&cjk);
    assert!(
        crate::agent::compact::text_tokens(&head) <= MAX_TOKENS + 64,
        "CJK must be cut by the token estimate too"
    );
    // a char boundary: the cut must never split a codepoint
    assert!(!head.trim_end_matches("[output truncated]\n").is_empty());
    let body = head.trim_end_matches("\n[output truncated]\n");
    assert!(body.chars().all(|c| c == '中'));

    // under the caps: returned untouched, with no marker
    let small = "hello";
    assert_eq!(truncate_head_marked(small), "hello");
}

#[test]
fn validation_bounces_bad_args() {
    let schema = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}, "n": {"type": "integer"}},
        "required": ["path"]
    });
    assert!(validate(&schema, &json!({"path": "x"})).is_ok());
    assert!(validate(&schema, &json!({})).is_err());
    assert!(validate(&schema, &json!({"path": 3})).is_err());
    assert!(validate(&schema, &json!({"path": "x", "n": 5})).is_ok());
    assert!(validate(&schema, &json!({"path": "x", "n": "five"})).is_err());
    assert!(validate(&schema, &json!("not an object")).is_err());
}

#[test]
fn html_to_text_strips_markup_and_scripts() {
    let html = "<html><head><style>p { color: red }</style></head><body>\
                <h1>Hello</h1><p>Some <b>bold</b> text.</p>\
                <script>var leak = \"secret <hidden>\";</script>\
                <p>After &amp; before.</p></body></html>";
    let text = html_to_text(html);
    assert!(text.contains("Hello"));
    assert!(text.contains("Some bold text."));
    assert!(text.contains("After & before."));
    assert!(!text.contains("secret"));
    assert!(!text.contains("color: red"));
}

#[test]
fn webfetch_refuses_non_http_schemes() {
    let tool = FetchTool;
    let out = tool.execute(
        &json!({"url": "file:///etc/passwd"}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(out.is_error);
    let out = tool.execute(
        &json!({"url": "ftp://example.com/x"}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(out.is_error);
    let out = tool.execute(&json!({}), Path::new("."), &mut |_| {});
    assert!(out.is_error);
}

#[test]
fn extract_title_collapses_and_decodes() {
    assert_eq!(
        extract_title("<html><head><title>  My &amp; page\n</title></head></html>"),
        Some("My & page".to_string())
    );
    // tags inside the title are stripped
    assert_eq!(
        extract_title("<title><b>bold</b> title</title>"),
        Some("<b>bold</b> title".to_string())
    );
    // no title
    assert_eq!(extract_title("<html><body>no title</body></html>"), None);
    // empty title is None
    assert_eq!(extract_title("<title>   </title>"), None);
}

#[test]
fn edit_fuzzy_matching_rescues_whitespace_and_crlf_mismatches() {
    let dir = crate::core::testutil::scratch_dir("editfz");
    // trailing spaces on both lines, CRLF endings: the model's oldText
    // (clean, LF-only) still applies
    let file = dir.join("a.txt");
    std::fs::write(&file, "let x = 1;   \r\nfn a() {}  \r\n// end\r\n").unwrap();
    let out = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "let x = 1;\nfn a() {}", "newText": "let x = 2;\nfn a() {}"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error, "{}", out.content);
    // the matched lines' trailing junk stays outside the replaced span
    // (minimal diff: only the matched content is replaced)
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "let x = 2;\nfn a() {}  \r\n// end\r\n"
    );
    // trailing whitespace on the oldText itself is trimmed on the needle
    // side too
    std::fs::write(&file, "fn a() {}\nfn b() {}\n").unwrap();
    let out = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "fn b() {}  ", "newText": "fn b() { todo!() }"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn a() {}\nfn b() { todo!() }\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_fuzzy_matching_folds_smart_punctuation() {
    let dir = crate::core::testutil::scratch_dir("editfq");
    let file = dir.join("q.txt");
    std::fs::write(&file, "msg = “hello” — ok\n").unwrap();
    // the model retypes the line with ASCII quotes and a hyphen
    let out = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "msg = \"hello\" - ok", "newText": "msg = 'hi'"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "msg = 'hi'\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_fuzzy_duplicate_matches_still_error() {
    let dir = crate::core::testutil::scratch_dir("editfd");
    let file = dir.join("d.txt");
    // "x \ny" exists twice after normalization, and never exactly
    std::fs::write(&file, "x \ny\nz\nx \ny\n").unwrap();
    let out = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "x\ny", "newText": "w"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(out.is_error);
    assert!(out.content.contains("2 times"), "{}", out.content);
    assert!(
        out.content.contains("whitespace-flexible"),
        "{}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_requires_unique_matches_and_no_overlap() {
    let dir = crate::core::testutil::scratch_dir("edit");
    let file = dir.join("a.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

    let dup = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [{"oldText": "o", "newText": "0"}]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(dup.is_error, "'o' appears twice and must be rejected");

    let missing = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [{"oldText": "nope", "newText": "x"}]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(missing.is_error);

    let overlap = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "two", "newText": "2"},
            {"oldText": "one\ntwo", "newText": "A"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(overlap.is_error, "overlapping spans must be rejected");

    let ok = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "one", "newText": "1"},
            {"oldText": "three", "newText": "3"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!ok.is_error, "{}", ok.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "1\ntwo\n3\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grep_literal_and_ignore_case() {
    let dir = crate::core::testutil::scratch_dir("grep");
    std::fs::write(dir.join("one.txt"), "Hello world\nbye\n").unwrap();
    std::fs::write(dir.join("two.txt"), "nope\n").unwrap();

    let out = GrepTool.execute(
        &json!({"pattern": "hello", "path": dir.display().to_string(), "ignore_case": true}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error);
    assert!(
        out.content.contains("one.txt:1: Hello world"),
        "{}",
        out.content
    );
    assert!(!out.content.contains("two.txt"));

    let exact = GrepTool.execute(
        &json!({"pattern": "hello", "path": dir.display().to_string()}),
        Path::new("."),
        &mut |_| {},
    );
    assert_eq!(exact.content, "no matches\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grep_regex_delegates_to_ripgrep() {
    // regex mode needs ripgrep; environments without it keep the literal path
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let dir = crate::core::testutil::scratch_dir("grep-re");
    std::fs::write(dir.join("a.rs"), "fn main() {}\nlet x = 42;\n").unwrap();
    std::fs::write(dir.join("b.txt"), "nothing\n").unwrap();

    let out = GrepTool.execute(
        &json!({"pattern": "fn\\s+main", "regex": true, "path": dir.display().to_string()}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error, "{}", out.content);
    // rg prints `path:line:text` (the literal path adds a space after the
    // line number); both are the same shape for the model to read
    assert!(
        out.content.contains("a.rs:1:fn main() {}"),
        "{}",
        out.content
    );
    assert!(!out.content.contains("b.txt"));

    // a bad pattern is reported as an error, never a crash
    let bad = GrepTool.execute(
        &json!({"pattern": "(", "regex": true, "path": dir.display().to_string()}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(bad.is_error, "{}", bad.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bash_output_beyond_pipe_buffer_does_not_deadlock() {
    // ~108KB of output used to fill the 64KiB pipe and hang until timeout
    let out = BashTool.execute(
        &json!({"command": "seq 1 20000", "timeout": 30}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error, "{}", out.content);
    // truncated to the last MAX_LINES lines (plus the truncation marker)
    assert!(out.content.lines().count() < 20000);
    assert!(out.content.contains("20000"));
}

/// The deadline and the output are independent facts: a command killed
/// at its limit keeps the partial output it already printed, so the
/// model can read what explains the timeout instead of a bare verdict.
#[cfg(unix)]
#[test]
fn a_timed_out_command_keeps_its_partial_output() {
    let out = BashTool.execute(
        &json!({"command": "echo partial-before-deadline; sleep 30", "timeout": 1}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(out.is_error);
    assert!(
        out.content.contains("partial-before-deadline"),
        "partial output must survive the kill: {}",
        out.content
    );
    assert!(
        out.content.contains("timed out after 1s"),
        "{}",
        out.content
    );
}

#[test]
fn glob_finds_matching_files() {
    let dir = crate::core::testutil::scratch_dir("glob");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "").unwrap();
    std::fs::write(dir.join("b.txt"), "").unwrap();
    let out = GlobTool.execute(
        &json!({"pattern": "**/*.rs", "path": dir.display().to_string()}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(out.content.contains("a.rs"), "{}", out.content);
    assert!(!out.content.contains("b.txt"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grep_context_dedups_overlapping_matches() {
    let dir = crate::core::testutil::scratch_dir("grepctx");
    // hits on adjacent lines with context 1: every line appears exactly
    // once, shared context included
    std::fs::write(
        dir.join("app.txt"),
        "one\nhit alpha\nhit beta\nhit gamma\nfour\n",
    )
    .unwrap();
    let out = GrepTool.execute(
        &json!({"pattern": "hit", "path": dir.display().to_string(), "context": 1}),
        Path::new("."),
        &mut |_| {},
    );
    let line_numbers: Vec<usize> = out
        .content
        .lines()
        .filter(|l| l.contains("app.txt"))
        .map(|l| {
            // `path:line: text`, where the path may carry a drive-letter
            // colon (C:/...) and the text may too — so peel from the right
            let (head, _) = l.rsplit_once(": ").expect("num: text tail");
            let (_, num) = head.rsplit_once(':').expect("path:num");
            num.parse().unwrap()
        })
        .collect();
    assert_eq!(line_numbers, vec![1, 2, 3, 4, 5], "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grep_survives_a_directory_symlink_cycle() {
    let dir = crate::core::testutil::scratch_dir("greplink");
    std::fs::write(dir.join("needle.txt"), "find me\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&dir, dir.join("loop")).unwrap();
    // the walk must treat `loop` as a file (not follow it into a cycle
    // and blow the stack)
    let out = GrepTool.execute(
        &json!({"pattern": "find me", "path": dir.display().to_string()}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(out.content.contains("needle.txt:1"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

fn read_execute(dir: &std::path::Path, args: Value) -> ToolOutput {
    ReadTool.execute(&args, dir, &mut |_| {})
}

#[test]
fn read_tool_windows_with_meta_header_and_note() {
    let dir = crate::core::testutil::scratch_dir("read");
    let file = dir.join("notes.txt");
    let body: Vec<String> = (1..=10).map(|i| format!("line-{i}")).collect();
    std::fs::write(&file, body.join("\n")).unwrap();

    let out = read_execute(&dir, json!({"path": "notes.txt", "offset": 3, "limit": 4}));
    assert!(!out.is_error);
    assert!(
        out.content.starts_with("[notes.txt · text · ≥7 lines · "),
        "{}",
        out.content
    );
    assert!(out.content.contains("3: line-3"));
    assert!(out.content.contains("6: line-6"));
    assert!(!out.content.contains("7: line-7"));
    assert!(
        out.content
            .contains("[Showing lines 3-6 of ≥7. Use offset=7 to continue.]"),
        "{}",
        out.content
    );

    // a window that reaches EOF reports exact totals and no note
    let out = read_execute(&dir, json!({"path": "notes.txt", "offset": 8, "limit": 5}));
    assert!(out.content.contains("· 10 lines ·"), "{}", out.content);
    assert!(!out.content.contains("Use offset="), "{}", out.content);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_paths_batches_several_files_in_one_call() {
    let dir = crate::core::testutil::scratch_dir("readmulti");
    std::fs::write(dir.join("a.rs"), "fn a() {}").unwrap();
    std::fs::write(dir.join("b.md"), "# b\nbody").unwrap();
    let out = read_execute(&dir, json!({"paths": ["a.rs", "b.md", "nope.txt"]}));
    assert!(!out.is_error, "two of three files read fine");
    assert!(out.content.contains("a.rs ·"), "{}", out.content);
    assert!(out.content.contains("1: fn a() {}"), "{}", out.content);
    assert!(out.content.contains("b.md ·"), "{}", out.content);
    // the missing file errors inline without failing the whole call
    assert!(out.content.contains("nope.txt"), "{}", out.content);
    // neither field → a clear usage error
    let none = read_execute(&dir, json!({}));
    assert!(none.is_error);
    assert!(none.content.contains("`paths`"));
    // over the batch cap → refused up front
    let six: Vec<String> = (0..6).map(|i| format!("f{i}.txt")).collect();
    let over = read_execute(&dir, json!({"paths": six}));
    assert!(over.is_error);
    assert!(over.content.contains("at most 5"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_tool_caps_at_read_max_lines() {
    let dir = crate::core::testutil::scratch_dir("readcap");
    let body: Vec<String> = (1..=2600).map(|i| format!("row-{i}")).collect();
    std::fs::write(dir.join("big.txt"), body.join("\n")).unwrap();
    let out = read_execute(&dir, json!({"path": "big.txt"}));
    assert!(out.content.contains("1: row-1"), "{}", out.content);
    assert!(out.content.contains("2000: row-2000"), "{}", out.content);
    assert!(!out.content.contains("2001: row-2001"));
    assert!(out.content.contains("Use offset=2001"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_tool_byte_cut_note_points_at_unseen_lines() {
    // 600 wide lines ≈ 184KB: the 500-line window cuts at line 500 and
    // the 50KB byte cap cuts around line ~170 — the note must resume at
    // the first line the model has not actually seen, not at 501
    let dir = crate::core::testutil::scratch_dir("readcut");
    let body: Vec<String> = (0..600).map(|_| "x".repeat(300)).collect();
    std::fs::write(dir.join("wide.txt"), body.join("\n")).unwrap();
    let out = read_execute(&dir, json!({"path": "wide.txt"}));
    assert!(!out.is_error);
    assert!(out.content.contains("150: "), "{}", out.content);
    assert!(!out.content.contains("250: "), "{}", out.content);
    assert!(!out.content.contains("Use offset=501"), "{}", out.content);
    let resume = out
        .content
        .split("Use offset=")
        .nth(1)
        .and_then(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<usize>().ok()
        })
        .expect("note present");
    assert!(resume > 150 && resume < 250, "resume={resume}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_tool_refuses_binary_with_hint() {
    let dir = crate::core::testutil::scratch_dir("readbin");
    std::fs::write(dir.join("aln.bam"), b"BAM\x01data").unwrap();
    std::fs::write(dir.join("paper.pdf"), b"%PDF-1.4 fake").unwrap();
    std::fs::write(dir.join("blob.dat"), b"xx\0yy").unwrap();

    let out = read_execute(&dir, json!({"path": "aln.bam"}));
    assert!(out.is_error);
    assert!(out.content.contains("binary format"), "{}", out.content);
    assert!(out.content.contains("samtools"), "{}", out.content);
    assert!(out.content.contains("use bash"), "{}", out.content);

    let out = read_execute(&dir, json!({"path": "paper.pdf"}));
    assert!(out.is_error);
    assert!(out.content.contains("pdftotext"), "{}", out.content);

    // unknown binary extension gets the generic wording without a hint
    let out = read_execute(&dir, json!({"path": "blob.dat"}));
    assert!(out.is_error);
    assert!(out.content.contains("binary format"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_tool_attaches_image_for_vision_models() {
    let dir = crate::core::testutil::scratch_dir("readimg");
    // a tiny PNG signature; the read tool routes by extension, so the
    // exact body is irrelevant to the test
    std::fs::write(dir.join("pic.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    let out = read_execute(&dir, json!({"path": "pic.png"}));
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("vision attachment"), "{}", out.content);
    assert_eq!(out.attachments.len(), 1);
    assert_eq!(out.attachments[0].mime_type, "image/png");
    assert!(!out.attachments[0].base64_data.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_tool_offset_past_end_and_empty_file() {
    let dir = crate::core::testutil::scratch_dir("readend");
    std::fs::write(dir.join("small.txt"), b"a\nb\n").unwrap();
    std::fs::write(dir.join("empty.txt"), b"").unwrap();

    let out = read_execute(&dir, json!({"path": "small.txt", "offset": 9}));
    assert!(out.is_error);
    assert_eq!(
        out.content,
        "offset 9 is past the end of the file (2 lines)"
    );

    let out = read_execute(&dir, json!({"path": "empty.txt"}));
    assert!(!out.is_error);
    assert_eq!(out.content, "(empty file)");

    let out = read_execute(&dir, json!({"path": "missing.txt"}));
    assert!(out.is_error);
    assert!(out.content.starts_with("cannot read"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_registry_exposes_update_plan_as_a_read_tier_tool() {
    let tools = builtin_tools();
    let plan = tools
        .iter()
        .find(|t| t.name() == "update_plan")
        .expect("mounted");
    assert_eq!(plan.tier(), Tier::Read, "planning touches nothing");
    // the shallow shared validator only needs `plan` to be an array; the
    // tool's own execute does the per-item checks
    assert!(validate(&plan.parameters(), &json!({"plan": []})).is_ok());
    assert!(validate(&plan.parameters(), &json!({})).is_err());
}

#[test]
fn update_plan_renders_the_checklist_and_bounces_bad_plans() {
    let tools = builtin_tools();
    let plan = tools.iter().find(|t| t.name() == "update_plan").unwrap();
    let run =
        |args: serde_json::Value| plan.execute(&args, std::path::Path::new("."), &mut |_: &str| {});
    let ok = run(json!({"plan": [
        {"step": "read the parser", "status": "completed"},
        {"step": "fix the offset bug", "status": "in_progress"},
        {"step": "add a test", "status": "pending"}
    ]}));
    assert!(!ok.is_error, "{}", ok.content);
    assert_eq!(
        ok.content,
        "[x] read the parser\n[>] fix the offset bug\n[ ] add a test"
    );
    // the chrome preview is one line: counts plus the active step
    assert_eq!(
        plan.preview(&json!({"plan": [{"step": "fix the offset bug", "status": "in_progress"}]})),
        "1 step · 0 done · now: fix the offset bug"
    );
    // two in-progress steps violate the one-at-a-time rule
    let err = run(json!({"plan": [
        {"step": "a", "status": "in_progress"},
        {"step": "b", "status": "in_progress"}
    ]}));
    assert!(err.is_error);
    assert!(err.content.contains("at most one"), "{}", err.content);
    // an empty plan and an unknown status are refused too
    assert!(run(json!({"plan": []})).is_error);
    assert!(run(json!({"plan": [{"step": "a", "status": "done"}]})).is_error);
}

/// The tool definitions ride the head of every request, so they are the
/// clearest fixed cost in the loop: a slack sentence here is paid on every
/// round of every session. They sit inside the cached prefix, which makes it
/// easy to let them grow unnoticed (the first round pays, the rest are cache
/// reads), so the budget is asserted rather than left to review.
#[test]
fn tool_defs_stay_under_the_wire_budget() {
    let mut total = 0;
    for t in builtin_tools() {
        let one = serde_json::json!({
            "name": t.name(), "description": t.description(), "parameters": t.parameters()
        });
        let n = one.to_string().len();
        assert!(
            n < 900,
            "`{}` serializes to {n} bytes; trim its description or schema",
            t.name()
        );
        total += n;
    }
    assert!(
        total < 4_800,
        "tool definitions total {total} bytes (budget 4800) — trim before adding"
    );
}
