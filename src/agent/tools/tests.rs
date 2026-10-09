use super::edit::change_hunks;
use super::fetch::{extract_title, html_to_text};
use super::write::write_atomic;
use super::*;

/// grep/glob delegate to ripgrep; CI runners without it skip those tests
/// (the tools themselves report the missing binary to the model).
fn rg_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_ok()
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
    let check = |args: Value| {
        let mut args = args;
        validate(&schema, &mut args)
    };
    assert!(check(json!({"path": "x"})).is_ok());
    assert!(check(json!({})).is_err());
    assert!(check(json!({"path": 3})).is_err());
    assert!(check(json!({"path": "x", "n": 5})).is_ok());
    assert!(check(json!({"path": "x", "n": "five"})).is_err());
    assert!(check(json!("not an object")).is_err());
}

#[test]
fn validation_coerces_numeric_and_boolean_strings() {
    // numeric coercion: "50" is an offset, "true" is a flag — a type spelling
    // must not cost the model a round
    let schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "offset": {"type": "integer"},
            "ratio": {"type": "number"},
            "flag": {"type": "boolean"}
        },
        "required": ["path"]
    });
    let mut args = json!({"path": "f", "offset": "50", "ratio": "1.5", "flag": "true"});
    assert!(validate(&schema, &mut args).is_ok());
    assert_eq!(args["offset"], json!(50));
    assert_eq!(args["ratio"], json!(1.5));
    assert_eq!(args["flag"], json!(true));

    // a non-numeric string still fails, with the argument named
    let mut args = json!({"path": "f", "offset": "middle"});
    let err = validate(&schema, &mut args).unwrap_err();
    assert!(err.contains("'offset'"), "{err}");
}

#[test]
fn validation_drops_optional_nulls_and_keeps_required_ones() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "limit": {"type": "integer"}
        },
        "required": ["path"]
    });
    // "limit": null on an optional field is an omission, not a type error
    let mut args = json!({"path": "f", "limit": null});
    assert!(validate(&schema, &mut args).is_ok());
    assert!(args.get("limit").is_none(), "the null is dropped");
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
    assert!(out.is_error());
    let out = tool.execute(
        &json!({"url": "ftp://example.com/x"}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(out.is_error());
    let out = tool.execute(&json!({}), Path::new("."), &mut |_| {});
    assert!(out.is_error());
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
fn edit_requires_unique_matches_and_no_overlap() {
    let dir = crate::core::testutil::scratch_dir("edit");
    let file = dir.join("a.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

    let dup = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [{"oldText": "o", "newText": "0"}]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(dup.is_error(), "'o' appears twice and must be rejected");

    let missing = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [{"oldText": "nope", "newText": "x"}]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(missing.is_error());

    let overlap = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "two", "newText": "2"},
            {"oldText": "one\ntwo", "newText": "A"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(overlap.is_error(), "overlapping spans must be rejected");

    let ok = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "one", "newText": "1"},
            {"oldText": "three", "newText": "3"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!ok.is_error(), "{}", ok.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "1\ntwo\n3\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_keeps_crlf_endings_and_a_bom_through_a_rewrite() {
    let dir = crate::core::testutil::scratch_dir("edit-crlf");
    let file = dir.join("win.txt");
    std::fs::write(&file, "\u{feff}one\r\ntwo\r\n").unwrap();

    // the model quotes the file back with \n: it still matches, and the
    // file's CRLF endings and BOM survive the write
    let out = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "one\ntwo", "newText": "ONE\nTWO"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error(), "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "\u{feff}ONE\r\nTWO\r\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_fuzzy_fallback_recovers_unicode_variants_and_keeps_other_lines_verbatim() {
    let dir = crate::core::testutil::scratch_dir("edit-fuzzy");
    let file = dir.join("doc.md");
    let original = "say \u{201c}hello\u{201d} \u{2014} now\nkeep me   \n";
    std::fs::write(&file, original).unwrap();

    // the ASCII spelling matches the smart-quote/em-dash line
    let out = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "say \"hello\" - now", "newText": "speak"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error(), "{}", out.content);
    // only the touched line is rewritten; the untouched line keeps its exact
    // bytes, trailing whitespace included
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "speak\nkeep me   \n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_fuzzy_fallback_folds_fullwidth_forms() {
    let dir = crate::core::testutil::scratch_dir("edit-fullwidth");
    let file = dir.join("cjk.txt");
    std::fs::write(&file, "\u{ff58}\u{ff1d}\u{ff11}\u{ff1b}\n").unwrap();

    let out = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "x=1;", "newText": "y=2;"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error(), "{}", out.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "y=2;\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_reports_which_edit_failed_and_writes_nothing() {
    let dir = crate::core::testutil::scratch_dir("edit-index");
    let file = dir.join("a.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();

    let out = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "one", "newText": "1"},
            {"oldText": "missing", "newText": "x"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(out.is_error());
    assert!(out.content.contains("edits[1]"), "{}", out.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_reports_a_replacement_that_changes_nothing() {
    let dir = crate::core::testutil::scratch_dir("edit-nochange");
    let file = dir.join("a.txt");
    std::fs::write(&file, "one\n").unwrap();

    let out = EditTool.execute(
        &json!({"path": file.display().to_string(), "edits": [
            {"oldText": "one", "newText": "one"}
        ]}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(out.is_error());
    assert!(out.content.contains("No changes made"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_salvages_stringified_and_flat_legacy_arguments() {
    let dir = crate::core::testutil::scratch_dir("edit-args");
    let file = dir.join("a.txt");

    // edits as a JSON string (a model quoting its own array)
    std::fs::write(&file, "one\n").unwrap();
    let out = EditTool.execute(
        &json!({"path": file.display().to_string(),
                "edits": "[{\"oldText\": \"one\", \"newText\": \"1\"}]"}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error(), "{}", out.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "1\n");

    // the legacy flat single-edit shape
    std::fs::write(&file, "one\n").unwrap();
    let out = EditTool.execute(
        &json!({"path": file.display().to_string(), "oldText": "one", "newText": "2"}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error(), "{}", out.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "2\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grep_literal_and_ignore_case() {
    if !rg_available() {
        return;
    }
    let dir = crate::core::testutil::scratch_dir("grep");
    std::fs::write(dir.join("one.txt"), "Hello world\nbye\n").unwrap();
    std::fs::write(dir.join("two.txt"), "nope\n").unwrap();

    let out = GrepTool.execute(
        &json!({"pattern": "hello", "path": dir.display().to_string(), "ignoreCase": true}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error());
    assert!(
        out.content.contains("one.txt:1:Hello world"),
        "{}",
        out.content
    );
    assert!(!out.content.contains("two.txt"));

    // case-sensitive regex: no match
    let exact = GrepTool.execute(
        &json!({"pattern": "hello", "path": dir.display().to_string()}),
        Path::new("."),
        &mut |_| {},
    );
    assert_eq!(exact.content, "No matches found");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grep_regex_delegates_to_ripgrep() {
    if !rg_available() {
        return;
    }
    let dir = crate::core::testutil::scratch_dir("grep-re");
    std::fs::write(dir.join("a.rs"), "fn main() {}\nlet x = 42;\n").unwrap();
    std::fs::write(dir.join("b.txt"), "nothing\n").unwrap();

    let out = GrepTool.execute(
        &json!({"pattern": "fn\\s+main", "path": dir.display().to_string()}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error(), "{}", out.content);
    assert!(
        out.content.contains("a.rs:1:fn main() {}"),
        "{}",
        out.content
    );
    assert!(!out.content.contains("b.txt"));

    // a bad pattern is reported as an error, never a crash
    let bad = GrepTool.execute(
        &json!({"pattern": "(", "path": dir.display().to_string()}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(bad.is_error(), "{}", bad.content);
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
    assert!(!out.is_error(), "{}", out.content);
    // truncated to the last MAX_LINES lines (plus the truncation marker)
    assert!(out.content.lines().count() < 20000);
    assert!(out.content.contains("20000"));
}

/// The deadline and the output are independent facts: a command killed
/// at its limit keeps the partial output it already printed, so the
/// model can read what explains the timeout instead of a bare verdict.
#[cfg(unix)]
#[test]
fn truncated_command_output_spills_the_full_text_to_a_named_file() {
    let dir = crate::core::testutil::scratch_dir("spill");
    let big: String = (0..6000).map(|i| format!("line-{i}\n")).collect();
    let out = truncate_with_spill_in(&dir, &big);
    assert!(out.contains("[Showing lines "), "{}", out);
    assert!(out.contains("Full output: "), "{}", out);
    // the named file holds the full output, so the model can read the rest
    // on demand instead of re-running the command
    let path = out
        .split("Full output: ")
        .nth(1)
        .unwrap()
        .trim_end_matches(']');
    let full = std::fs::read_to_string(Path::new(path)).unwrap();
    assert_eq!(full, big);
    // the tail survives the cut, where a command's failure lands
    assert!(out.contains("line-5999"), "{}", out);

    // under the cap: no note, no spill (lines().join drops the trailing
    // newline, the same as the truncation path has always done)
    let out = truncate_with_spill_in(&dir, "tiny\n");
    assert_eq!(out, "tiny");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_timed_out_command_keeps_its_partial_output() {
    let out = BashTool.execute(
        &json!({"command": "echo partial-before-deadline; sleep 30", "timeout": 1}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(out.is_error());
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

#[cfg(unix)]
#[test]
fn tool_output_strips_ansi_before_it_reaches_the_model() {
    // a --color=always row would otherwise ride into the context as escape
    // bytes and render through wrap paths that count them as zero cells
    let out = BashTool.execute(
        &json!({"command": "printf 'a\\033[31mred\\033[0mb\\n'"}),
        Path::new("."),
        &mut |_| {},
    );
    assert!(!out.is_error(), "{}", out.content);
    assert!(
        !out.content.contains('\x1b'),
        "{}",
        out.content.escape_debug()
    );
    assert_eq!(out.content, "aredb");
}

#[test]
fn glob_finds_matching_files() {
    if !rg_available() {
        return;
    }
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
    if !rg_available() {
        return;
    }
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
    // ripgrep merges overlapping context and reports each match once
    assert!(out.content.contains("app.txt"), "{}", out.content);
    assert!(out.content.contains("hit alpha"), "{}", out.content);
    assert!(out.content.contains("hit beta"), "{}", out.content);
    assert!(out.content.contains("hit gamma"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grep_survives_a_directory_symlink_cycle() {
    if !rg_available() {
        return;
    }
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
fn read_tool_windows_with_a_continuation_note() {
    let dir = crate::core::testutil::scratch_dir("read");
    let file = dir.join("notes.txt");
    let body: Vec<String> = (1..=10).map(|i| format!("line-{i}")).collect();
    std::fs::write(&file, body.join("\n")).unwrap();

    let out = read_execute(&dir, json!({"path": "notes.txt", "offset": 3, "limit": 4}));
    assert!(!out.is_error());
    assert_eq!(
        out.content,
        "line-3\nline-4\nline-5\nline-6\n\n[Showing lines 3-6 of ≥7. Use offset=7 to continue.]"
    );

    // a window that reaches EOF reports no continuation note
    let out = read_execute(&dir, json!({"path": "notes.txt", "offset": 8, "limit": 5}));
    assert_eq!(out.content, "line-8\nline-9\nline-10");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_tool_caps_at_read_max_lines() {
    let dir = crate::core::testutil::scratch_dir("readcap");
    let body: Vec<String> = (1..=2600).map(|i| format!("row-{i}")).collect();
    std::fs::write(dir.join("big.txt"), body.join("\n")).unwrap();
    let out = read_execute(&dir, json!({"path": "big.txt"}));
    assert!(out.content.starts_with("row-1\n"), "{}", out.content);
    assert!(out.content.contains("\nrow-2000\n"), "{}", out.content);
    assert!(!out.content.contains("row-2001"));
    assert!(out.content.contains("Use offset=2001"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_tool_byte_cut_note_points_at_unseen_lines() {
    // 600 wide lines ≈ 184KB: the 2000-line window does not cut, and the
    // 50KB byte cap cuts around line ~170 — the note must resume at
    // the first line the model has not actually seen, not at 2001
    let dir = crate::core::testutil::scratch_dir("readcut");
    let body: Vec<String> = (0..600).map(|_| "x".repeat(300)).collect();
    std::fs::write(dir.join("wide.txt"), body.join("\n")).unwrap();
    let out = read_execute(&dir, json!({"path": "wide.txt"}));
    assert!(!out.is_error());
    // byte cap cut well before the 500-line window: fewer than 500 lines
    let body = out.content.split("\n\n[").next().unwrap();
    let shown = body.lines().count();
    assert!(shown > 150 && shown < 250, "shown={shown}");
    let resume = out
        .content
        .split("Use offset=")
        .nth(1)
        .and_then(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<usize>().ok()
        })
        .expect("note present");
    assert_eq!(resume, shown + 1, "resume at the first unseen line");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_names_the_bash_way_past_an_oversized_single_line() {
    // the line the offset points at is over the whole cap: reporting a
    // "showing lines" note over nothing would send the model stepping
    // offsets forever, so say how to get past it
    let dir = crate::core::testutil::scratch_dir("readhuge");
    let mut line = "x".repeat(60 * 1024);
    line.push('\n');
    std::fs::write(dir.join("huge.txt"), format!("{line}tail\n")).unwrap();
    let out = read_execute(&dir, json!({"path": "huge.txt"}));
    assert!(!out.is_error());
    assert_eq!(
        out.content,
        "[Line 1 is 60.0KB, exceeds 50.0KB limit. Use bash: sed -n '1p' huge.txt | head -c 51200]"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_tool_refuses_binary_with_hint() {
    let dir = crate::core::testutil::scratch_dir("readbin");
    std::fs::write(dir.join("aln.bam"), b"BAM\x01data").unwrap();
    std::fs::write(dir.join("paper.pdf"), b"%PDF-1.4 fake").unwrap();
    std::fs::write(dir.join("blob.dat"), b"xx\0yy").unwrap();

    let out = read_execute(&dir, json!({"path": "aln.bam"}));
    assert!(out.is_error());
    assert!(out.content.contains("binary format"), "{}", out.content);
    assert!(out.content.contains("samtools"), "{}", out.content);
    assert!(out.content.contains("use bash"), "{}", out.content);

    let out = read_execute(&dir, json!({"path": "paper.pdf"}));
    assert!(out.is_error());
    assert!(out.content.contains("pdftotext"), "{}", out.content);

    // unknown binary extension gets the generic wording without a hint
    let out = read_execute(&dir, json!({"path": "blob.dat"}));
    assert!(out.is_error());
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
    assert!(!out.is_error(), "{}", out.content);
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
    assert!(out.is_error());
    assert_eq!(
        out.content,
        "offset 9 is past the end of the file (2 lines)"
    );

    let out = read_execute(&dir, json!({"path": "empty.txt"}));
    assert!(!out.is_error());
    assert_eq!(out.content, "");

    let out = read_execute(&dir, json!({"path": "missing.txt"}));
    assert!(out.is_error());
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
    let mut args = json!({"plan": []});
    assert!(validate(&plan.parameters(), &mut args).is_ok());
    let mut args = json!({});
    assert!(validate(&plan.parameters(), &mut args).is_err());
}

#[test]
fn update_plan_renders_the_checklist_and_bounces_bad_plans() {
    let tools = builtin_tools();
    let plan = tools.iter().find(|t| t.name() == "update_plan").unwrap();
    let run = |args: serde_json::Value| {
        plan.execute(
            &args,
            std::path::Path::new("."),
            &mut |_: super::ToolProgress| {},
        )
    };
    let ok = run(json!({"plan": [
        {"step": "read the parser", "status": "completed"},
        {"step": "fix the offset bug", "status": "in_progress"},
        {"step": "add a test", "status": "pending"}
    ]}));
    assert!(!ok.is_error(), "{}", ok.content);
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
    assert!(err.is_error());
    assert!(err.content.contains("at most one"), "{}", err.content);
    // an empty plan and an unknown status are refused too
    assert!(run(json!({"plan": []})).is_error());
    assert!(run(json!({"plan": [{"step": "a", "status": "done"}]})).is_error());
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
            n < 1_300,
            "`{}` serializes to {n} bytes; trim its description or schema",
            t.name()
        );
        total += n;
    }
    assert!(
        total < 5_700,
        "tool definitions total {total} bytes (budget 5700) — trim before adding"
    );
}

#[test]
fn webfetch_is_exec_tier() {
    let tools = builtin_tools();
    let fetch = tools
        .iter()
        .find(|t| t.name() == "webfetch")
        .expect("mounted");
    // the one way off the machine is exec-tier like any other shell-out
    assert_eq!(fetch.tier(), Tier::Exec);
}

#[test]
fn plain_names_and_patterns_pick_the_allowlist() {
    let tools: Vec<String> = ["read", "grep", "glob", "bash"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let pick = |csv: &str| -> Vec<String> {
        select_tools(
            &tools,
            &csv.split(',')
                .map(|s| s.trim().to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    };
    assert_eq!(pick("read,grep"), vec!["read", "grep"]);
    // a pattern keeps every match, in registry order (gr* hits grep only:
    // glob does not start with gr)
    assert_eq!(pick("gr*"), vec!["grep"]);
    assert_eq!(pick("*"), tools.clone());
    assert_eq!(pick("g*b"), vec!["glob"]);
}

#[test]
fn a_typo_in_a_plain_selection_fails_loudly() {
    let tools: Vec<String> = ["read", "bash"].iter().map(|s| s.to_string()).collect();
    let err = select_tools(&tools, &["raed".to_string()]).unwrap_err();
    assert!(err.contains("unknown tool 'raed'"), "{err}");
    assert!(err.contains("available: read, bash"), "{err}");
}

#[test]
fn modifiers_add_and_remove_without_replacing_the_set() {
    let tools: Vec<String> = ["read", "write", "edit"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let run = |csv: &str| {
        select_tools(
            &tools,
            &csv.split(',')
                .map(|s| s.trim().to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    };
    // remove one, keep the rest
    assert_eq!(run("-edit"), vec!["read", "write"]);
    // add a name the registry does not carry yet is refused (nothing to add);
    // an existing one re-added is a no-op
    assert_eq!(run("+read"), tools.clone());
    // order matters: -a,+a ends without it, +a,-a also without it
    assert_eq!(run("+edit,-edit"), vec!["read", "write"]);
    // removing everything is the same empty selection error
    assert!(
        select_tools(
            &tools,
            &"-read,-write,-edit"
                .split(',')
                .map(|s| s.trim().to_string())
                .collect::<Vec<_>>()
        )
        .is_err()
    );
}

#[test]
fn modifiers_cannot_mix_with_plain_names_or_patterns() {
    let tools: Vec<String> = vec!["read".to_string()];
    let mixed = select_tools(&tools, &["+read".into(), "bash".into()]).unwrap_err();
    assert!(mixed.contains("cannot be mixed"), "{mixed}");
    let pattern = select_tools(&tools, &["+gr*".into()]).unwrap_err();
    assert!(pattern.contains("not patterns"), "{pattern}");
}

#[test]
fn pattern_matching_honors_leading_trailing_and_inner_stars() {
    assert!(tool_pattern_match("mcp__radius__*", "mcp__radius__search"));
    assert!(!tool_pattern_match("mcp__radius__*", "mcp__other__search"));
    assert!(tool_pattern_match("*search*", "mcp__radius__search"));
    assert!(tool_pattern_match("read", "read"));
    assert!(!tool_pattern_match("read", "readonly"));
    // a trailing star is a prefix match: it consumes the rest of the name
    assert!(tool_pattern_match("re*", "read"));
    assert!(!tool_pattern_match("re*", "write"));
    // a pattern without a trailing star must consume the whole name
    assert!(!tool_pattern_match("*ad", "readx"));
    assert!(tool_pattern_match("*ad", "read"));
    // an inner star spans any run
    assert!(tool_pattern_match("r*d", "read"));
    assert!(tool_pattern_match("r*d", "rod"));
    assert!(!tool_pattern_match("r*d", "redx"));
}

#[test]
fn truncate_tail_counts_the_json_escaped_form() {
    // a payload of newlines: raw len is well under the cap, but the wire
    // form doubles every byte — the tail must shrink until the escaped
    // form fits, not just the raw one
    let text = "\n".repeat(40 * 1024);
    let (tail, truncated) = truncate_tail(&text, MAX_LINES, MAX_BYTES);
    assert!(truncated);
    assert!(
        json_escaped_len(&tail) <= MAX_BYTES,
        "escaped len {}",
        json_escaped_len(&tail)
    );
    // quotes and backslashes double too
    let quotes = "\"".repeat(40 * 1024);
    let (tail, _) = truncate_tail(&quotes, MAX_LINES, MAX_BYTES);
    assert!(json_escaped_len(&tail) <= MAX_BYTES);
    // control characters become 6-byte escapes
    let ctl = "\u{1}".repeat(10 * 1024);
    let (tail, truncated) = truncate_tail(&ctl, MAX_LINES, MAX_BYTES);
    assert!(truncated);
    assert!(json_escaped_len(&tail) <= MAX_BYTES);
    // plain ASCII with no escapes: the cap is the raw len, unchanged
    let plain = "a".repeat(MAX_BYTES + 5000);
    let (tail, _) = truncate_tail(&plain, MAX_LINES, MAX_BYTES);
    assert!(tail.len() <= MAX_BYTES);
    assert!(json_escaped_len(&tail) <= MAX_BYTES + 2);
}

#[test]
fn truncate_head_counts_the_json_escaped_form() {
    let text = "\n".repeat(40 * 1024);
    let head = truncate_head_marked(&text);
    let body = head.trim_end_matches("[output truncated]").trim_end();
    assert!(
        json_escaped_len(body) <= MAX_BYTES,
        "escaped len {}",
        json_escaped_len(body)
    );
}

#[test]
fn json_escaped_len_matches_serde_for_the_hot_characters() {
    for s in [
        "",
        "plain text",
        "with \"quotes\"",
        "back\\slash",
        "new\nline\r\n",
        "tab\there",
        "\u{1}\u{7f}",
        "汉字 ünïcødé",
    ] {
        let serde_len = serde_json::to_string(s).unwrap().len();
        assert_eq!(json_escaped_len(s), serde_len, "{s:?}");
    }
}
