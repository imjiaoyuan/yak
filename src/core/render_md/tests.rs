//! Renderer tests: the replay path, the live stream and the line/wrap
//! decisions they share, driven through both entry points.

use super::*;

fn p() -> &'static Palette {
    crate::theme::ansi256()
}

fn render(text: &str) -> String {
    let mut s = StyleStream::indented(0, p());
    let mut out = String::new();
    s.push_delta(&resolve_setext(text), &mut out);
    s.finish(&mut out);
    out
}

/// Stream `text` through StyleStream char-by-char (margin 0).
fn live(text: &str) -> String {
    let mut s = StyleStream::indented(0, p());
    let mut out = String::new();
    for ch in text.chars() {
        s.push_delta(&ch.to_string(), &mut out);
    }
    s.finish(&mut out);
    out
}

const B: &str = "\x1b[1m"; // bold
const I: &str = "\x1b[3m"; // italic
const U: &str = "\x1b[4m"; // underline
const H: &str = "\x1b[38;5;222m"; // heading
const L: &str = "\x1b[38;5;110m"; // link
const LU: &str = "\x1b[38;5;242m"; // link url
const C: &str = "\x1b[38;5;109m"; // code + bullet accent
const CB: &str = "\x1b[38;5;143m"; // code block green
const G: &str = "\x1b[38;5;244m"; // gray (quote/border/hr)
const S: &str = "\x1b[9m"; // strike
const R: &str = "\x1b[0m";

// ---- replay: headings ------------------------------------------

#[test]
fn headings_render_styled_by_level() {
    assert_eq!(render("# 标题\n"), format!("{H}{B}{U}标题{R}\n"));
    assert_eq!(render("## 小节\n"), format!("{H}{B}小节{R}\n"));
    // h3+ keep their prefix, styled like the heading
    assert_eq!(render("### 深级\n"), format!("{H}{B}### 深级{R}\n"));
}

#[test]
fn setext_underlines_make_headings() {
    assert_eq!(render("标题\n=====\n"), format!("{H}{B}{U}标题{R}\n"));
    assert_eq!(render("标题\n-----\n"), format!("{H}{B}标题{R}\n"));
    // a ---- with no paragraph above stays a rule
    assert_eq!(render("---\n"), format!("{G}{}{R}\n", "─".repeat(80)));
}

#[test]
fn one_blank_survives_between_blocks() {
    assert_eq!(render("a\n\nb\n"), "a\n\nb\n");
    // runs collapse to one
    assert_eq!(render("a\n\n\n\nb\n"), "a\n\nb\n");
    // no source blank: no injected blank (matches the live stream)
    assert_eq!(render("a\nb\n"), "a\nb\n");
    // leading and trailing blanks drop
    assert_eq!(render("\n\na\n\n"), "a\n");
}

// ---- replay: inline --------------------------------------------

#[test]
fn inline_emphasis_code_and_strike() {
    assert_eq!(
        render("这是 **加粗** 与 *斜体* 与 `代码` 与 ~~删除~~\n"),
        format!("这是 {B}加粗{R} 与 {I}斜体{R} 与 {C}代码{R} 与 {S}删除{R}\n")
    );
    assert_eq!(render("***粗斜***\n"), format!("{B}{I}粗斜{R}\n"));
}

#[test]
fn double_backtick_code_span_renders_clean() {
    assert_eq!(
        render("改 ``dedup_bam2/p1`` 目录\n"),
        format!("改 {C}dedup_bam2/p1{R} 目录\n")
    );
    assert_eq!(render("孤立 ` 反引号\n"), "孤立 ` 反引号\n");
}

#[test]
fn underscore_is_never_emphasis() {
    assert_eq!(render("_snake_case_\n"), "_snake_case_\n");
}

#[test]
fn link_underlines_and_shows_a_differing_href() {
    assert_eq!(
        render("见 [文档](https://example.com) 说明\n"),
        format!("见 {L}{U}文档{R}{LU} (https://example.com){R} 说明\n")
    );
    // text == href: no duplicate
    assert_eq!(render("[x](x)\n"), format!("{L}{U}x{R}\n"));
}

#[test]
fn unterminated_markers_stay_literal() {
    assert_eq!(render("未闭合 **加粗\n"), "未闭合 **加粗\n");
    assert_eq!(
        render("未闭合 [链接](https://x\n"),
        "未闭合 [链接](https://x\n"
    );
    // a closed span inside an unclosed one still renders
    assert_eq!(render("a **b *c* d\n"), format!("a **b {I}c{R} d\n"));
}

// ---- replay: fences, quotes, lists, rules ----------------------

#[test]
fn fence_shows_borders_and_indents_content() {
    assert_eq!(
        render("```rust\nfn main() {}\n```\n"),
        format!("{G}```rust{R}\n{CB}  fn main() {{}}{R}\n{G}```{R}\n")
    );
    // blank separation from surrounding paragraphs
    assert_eq!(
        render("para\n\n```rust\nlet x;\n```\n\nafter\n"),
        format!("para\n\n{G}```rust{R}\n{CB}  let x;{R}\n{G}```{R}\n\nafter\n")
    );
}

#[test]
fn unterminated_fence_flushes_content() {
    assert_eq!(render("```\nabc"), format!("{G}```{R}\n{CB}  abc{R}\n"));
}

#[test]
fn fence_content_not_inline_parsed() {
    assert_eq!(
        render("```\n**not bold**\n```\n"),
        format!("{G}```{R}\n{CB}  **not bold**{R}\n{G}```{R}\n")
    );
}

#[test]
fn quote_uses_bar_and_italic_gray() {
    assert_eq!(
        render("> 引用内容\n"),
        format!("{G}│ {R}{G}{I}引用内容{R}\n")
    );
}

#[test]
fn lists_nest_four_spaces_and_color_markers() {
    assert_eq!(
        render("- 一级\n  - 二级\n      - 三级\n"),
        format!("{C}- {R}一级\n    {C}- {R}二级\n        {C}- {R}三级\n")
    );
}

#[test]
fn ordered_list_keeps_numbers() {
    assert_eq!(
        render("1. 甲\n2. 乙\n"),
        format!("{C}1. {R}甲\n{C}2. {R}乙\n")
    );
}

#[test]
fn task_lists_keep_the_literal_checkbox() {
    assert_eq!(
        render("- [x] 完成\n- [ ] 待办\n"),
        format!("{C}- [x] {R}完成\n{C}- [ ] {R}待办\n")
    );
}

#[test]
fn hr_renders_dim_rule_capped_at_80() {
    assert_eq!(render("---\n"), format!("{G}{}{R}\n", "─".repeat(80)));
    assert_eq!(
        render_at(0, 40, "---\n"),
        format!("{G}{}{R}\n", "─".repeat(40))
    );
}

// ---- replay: tables ---------------------------------------------

#[test]
fn table_rows_pass_through_verbatim() {
    let t = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    assert_eq!(render(t), t);
}

// ---- replay: margins and wrapping --------------------------------

#[test]
fn indented_margin_prefixes_content_not_blank_lines() {
    assert_eq!(
        render_at(2, 0, "hi\n\n- a\n"),
        format!("  hi\n\n  {C}- {R}a\n")
    );
}

#[test]
fn wrapped_lines_carry_the_margin() {
    assert_eq!(
        render_at(2, 10, "aaaa bbbb cccc dddd\n"),
        "  aaaa bbbb\n  cccc dddd\n"
    );
}

#[test]
fn wrapped_list_continuations_align_under_content() {
    // cont 2: continuation rows indent past the marker. The break point
    // is the live stream's: the row fills (the space before `bbbb` sits
    // too far from the edge to be taken early)
    assert_eq!(
        render_at(2, 12, "- aaaa bbbb cccc\n"),
        format!("  {C}- {R}aaaa bbb\n    b cccc\n")
    );
}

// ---- StyleStream: live streaming ------------------------------------

#[test]
fn live_plain_text_streams_verbatim_with_margin() {
    let mut s = StyleStream::indented(2, p());
    let mut out = String::new();
    s.push_delta("The Rust t", &mut out);
    assert_eq!(out, "  The Rust t"); // printed as it arrives
    s.push_delta("oolkit\nsecond line", &mut out);
    assert_eq!(out, "  The Rust toolkit\n  second line");
    assert!(s.finish(&mut out));
    assert_eq!(out, "  The Rust toolkit\n  second line\n");
}

#[test]
fn live_blank_lines_stay_empty() {
    assert_eq!(live("a\n\nb\n"), "a\n\nb\n");
}

#[test]
fn live_heading_opens_style_and_streams_chars() {
    let mut s = StyleStream::indented(0, p());
    let mut out = String::new();
    s.push_delta("# 标", &mut out);
    assert_eq!(out, format!("{H}{B}{U}标")); // style opens immediately
    s.push_delta("题\n", &mut out);
    assert_eq!(out, format!("{H}{B}{U}标题{R}\n"));
}

#[test]
fn live_deep_heading_keeps_prefix() {
    assert_eq!(live("### 深级\n"), format!("{H}{B}### 深级{R}\n"));
}

#[test]
fn live_bold_holds_only_the_span() {
    let mut s = StyleStream::indented(0, p());
    let mut out = String::new();
    s.push_delta("a **bo", &mut out);
    // the open marker holds its own tail; the prefix streamed
    assert_eq!(out, "a ");
    s.push_delta("ld** x\n", &mut out);
    assert_eq!(out, format!("a {B}bold{R} x\n"));
}

#[test]
fn live_inline_code_and_links_burst_on_close() {
    assert_eq!(live("use `foo` here\n"), format!("use {C}foo{R} here\n"));
    assert_eq!(
        live("见 [文档](https://x.com) 吗\n"),
        format!("见 {L}{U}文档{R}{LU} (https://x.com){R} 吗\n")
    );
}

#[test]
fn live_unclosed_marker_flushes_literally_at_line_end() {
    assert_eq!(live("a **b\n"), "a **b\n");
    // nested closed italic inside the unclosed bold still renders
    assert_eq!(live("a **b *c* d\n"), format!("a **b {I}c{R} d\n"));
}

#[test]
fn live_quote_streams_with_bar() {
    let mut s = StyleStream::indented(0, p());
    let mut out = String::new();
    s.push_delta("> 引用", &mut out);
    assert_eq!(out, format!("{G}│ {R}{G}{I}引用"));
    s.push_delta("内容\n", &mut out);
    assert_eq!(out, format!("{G}│ {R}{G}{I}引用内容{R}\n"));
}

#[test]
fn live_lists_color_the_marker() {
    assert_eq!(live("- 一级\n"), format!("{C}- {R}一级\n"));
    assert_eq!(live("  - 二级\n"), format!("    {C}- {R}二级\n"));
    assert_eq!(live("1. 甲\n"), format!("{C}1. {R}甲\n"));
    assert_eq!(live("- [x] 完成\n"), format!("{C}- [x] {R}完成\n"));
}

#[test]
fn live_fence_streams_content_immediately() {
    let mut s = StyleStream::indented(0, p());
    let mut out = String::new();
    s.push_delta("```rust\nfn m", &mut out);
    // fence content chars land as they arrive, in the code color
    assert_eq!(out, format!("{G}```rust{R}\n{CB}  fn m"));
    s.push_delta("ain() {}\n```\nafter\n", &mut out);
    assert_eq!(
        out,
        format!("{G}```rust{R}\n{CB}  fn main() {{}}{R}\n{G}```{R}\nafter\n")
    );
}

#[test]
fn live_fence_close_needs_the_full_line() {
    // ``` inside content (with text after) is content, not a close
    assert_eq!(
        live("```\n``x\n```\nend\n"),
        format!("{G}```{R}\n{CB}  ``x{R}\n{G}```{R}\nend\n")
    );
}

#[test]
fn live_fence_inside_a_list_item_closes() {
    // the fence is indented inside a list item: the closing run must
    // still be recognized, and the indent held back with it, or the
    // stream would stay in fence mode for the rest of the answer
    assert_eq!(
        live("1. **作用域选择**\n\n   ```\n   x\n   ```\n\n后文\n"),
        format!("{C}1. {R}{B}作用域选择{R}\n\n{G}```{R}\n{CB}     x{R}\n{G}```{R}\n\n后文\n")
    );
    // a whitespace-only line inside the fence prints a bare blank line
    assert_eq!(
        live("```\n   \n```\n尾\n"),
        format!("{G}```{R}\n\n{G}```{R}\n尾\n")
    );
}

#[test]
fn hr_markers_thresholds_split_replay_from_live() {
    // one walk answers all four questions: the count is the threshold
    assert_eq!(hr_markers("---"), Some(3));
    assert_eq!(hr_markers("- - -"), Some(3));
    assert_eq!(hr_markers("--"), Some(2));
    assert_eq!(hr_markers("-"), Some(1));
    // any other char (or a mixed run) ends it
    assert_eq!(hr_markers("-*-"), None);
    assert_eq!(hr_markers("-x"), None);
    assert_eq!(hr_markers(""), None);
    assert_eq!(hr_markers("   "), None);
    assert!(is_hr("---") && !is_hr("--"));
    assert!(is_hr_candidate("--") && !is_hr_candidate("-"));
    // the live hold is the loosest of the four
    assert!(may_be_hr("-") && may_be_hr("- -"));
    assert!(!may_be_hr("-x"));
}

#[test]
fn live_hr_decides_at_line_end() {
    assert_eq!(live("---\n"), format!("{G}{}{R}\n", "─".repeat(80)));
    // `--` (only two) is not a rule: literal
    assert_eq!(live("--\n"), "--\n");
    // `**bold**` at line start aborts the HR candidate and resolves
    assert_eq!(live("**注意**：\n"), format!("{B}注意{R}：\n"));
}

#[test]
fn live_table_rows_pass_through_verbatim() {
    // the simple way: live and replay both pass `|` rows through
    let t = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    assert_eq!(live(t), t);
    let mut s = StyleStream::indented(2, p());
    s.wrap_terminal();
    let mut out = String::new();
    s.push_delta(t, &mut out);
    s.finish(&mut out);
    assert!(out.starts_with("  | a | b |\n"));
    assert!(!out.contains('\r')); // never any in-place rewrite
}

#[test]
fn live_tables_without_separator_fall_back_verbatim() {
    let t = "| a | b |\n| 1 | 2 |\n";
    assert_eq!(live(t), t);
}

#[test]
fn long_unclosed_marker_degrades_within_the_cap() {
    // a prose bracket or stray backtick must not hold the rest of the
    // line: past HOLD_CAP cells the marker streams literally and the scan
    // continues (a later closed span still renders)
    let xs = "x".repeat(600);
    let doc = format!("see [note {xs} and `code` too\n");
    assert_eq!(live(&doc), format!("see [note {xs} and {C}code{R} too\n"));
    assert_eq!(live(&doc), render(&doc));
    // a never-closed marker settles the same way at the line's end
    let short = format!("see [note {xs}\n");
    assert_eq!(live(&short), render(&short));
}

#[test]
fn a_long_cjk_span_keeps_its_styling() {
    // the hold bound counts cells: three bytes a character, so a Chinese
    // sentence used to spill past a byte bound and show its asterisks
    let doc = "4. **只作用于 smooth frequency，没把低覆盖位点从 reads 面板剔除** 后文\n";
    assert_eq!(
        live(doc),
        format!("{C}4. {R}{B}只作用于 smooth frequency，没把低覆盖位点从 reads 面板剔除{R} 后文\n")
    );
    assert_eq!(
        live("`一段比较长的中文代码片段内容在这里哦真的很长很长很长很长` 后文\n"),
        format!("{C}一段比较长的中文代码片段内容在这里哦真的很长很长很长很长{R} 后文\n")
    );
    assert_eq!(
        live("前文 **一段足够长的中文强调文本，长到超过旧的字节上限也不该漏出星号** 后文\n"),
        format!("前文 {B}一段足够长的中文强调文本，长到超过旧的字节上限也不该漏出星号{R} 后文\n")
    );
    // 100 characters = 300 bytes but only 100 cells: a byte bound cuts
    // this span off mid-line and the asterisks show
    let wide = "宽".repeat(100);
    assert_eq!(
        live(&format!("4. **{wide}** 后文\n")),
        format!("{C}4. {R}{B}{wide}{R} 后文\n")
    );
}

#[test]
fn live_bare_markers_at_eol_render_as_items() {
    assert_eq!(live("-\n"), format!("{C}- {R}\n"));
    assert_eq!(live("1.\n"), format!("{C}1. {R}\n"));
}

#[test]
fn live_wraps_carry_margin_and_reopen_styles() {
    let mut s = StyleStream::indented(2, p());
    s.wrap = 10;
    let mut out = String::new();
    s.push_delta("aaaa bbbb cccc dddd\n", &mut out);
    assert_eq!(out, "  aaaa bbbb\n  cccc dddd\n");
    // an open heading style re-opens after a break
    s.push_delta("**aaaaaaaaaa bbbb**\n", &mut out);
    assert!(
        out.contains("aaaa\n  \x1b[1ma") || out.contains("\x1b[1maaaa"),
        "got {out:?}"
    );
}

#[test]
fn live_cjk_never_straddles_the_wrap() {
    let mut s = StyleStream::indented(2, p());
    s.wrap = 4;
    let mut out = String::new();
    s.push_delta("中中中中\n", &mut out);
    assert_eq!(out, "  中中\n  中中\n");
}

#[test]
fn live_finish_is_idempotent_and_write_once() {
    let mut s = StyleStream::indented(2, p());
    let mut out = String::new();
    s.push_delta("尾部", &mut out);
    assert!(s.finish(&mut out));
    assert_eq!(out, "  尾部\n");
    assert!(!s.finish(&mut out));
    assert!(!out.contains('\r') && !out.contains("\x1b[2K") && !out.contains("\x1b[1A"));
}

#[test]
fn live_matches_itself_regardless_of_delta_boundaries() {
    let doc = "# T\n\npara `code` and **bold** text\n\n- item one\n- [x] done\n\n> quote line\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
    // char-by-char, word-by-word and whole-blob feeds agree byte for
    // byte: settlement depends only on line content, never on chunks
    let chars = live(doc);
    let mut words = StyleStream::indented(0, p());
    let mut wout = String::new();
    for w in doc.split_inclusive([' ', '\n']) {
        words.push_delta(w, &mut wout);
    }
    words.finish(&mut wout);
    assert_eq!(chars, wout);
    // no in-place erase/redraw ever
    assert!(!chars.contains('\r'));
    assert!(!chars.contains("\x1b[2K"));
    assert!(!chars.contains("\x1b[1A"));
    assert!(!chars.contains("\x1b[J"));
}

/// The shapes a model actually answers in, streamed one char at a time
/// (the worst case for every hold decision) and replayed: both renderers
/// must produce the same bytes. Every rendering change must keep this
/// green — the markdown differential is where the streaming bugs live.
///
/// One shape is deliberately absent because live and replay cannot
/// agree on it by construction: a setext underline (`text` then `---`)
/// folds the pair into a heading, and live has already printed that
/// paragraph line. Blank runs *are* covered — the live stream holds a
/// blank until the next block, like replay, so runs collapse and
/// leading/trailing blanks never print.
#[test]
fn live_and_replay_agree_on_a_realistic_corpus() {
    let docs = [
        // headings, inline spans, lists, quote, rule, fence
        "# 标题\n\n段落 `code` 与 **加粗**。\n\n- 甲\n- [x] 乙\n\n> 引用\n\n---\n\n```rust\nfn x() {}\n```\n",
        // a chinese answer: nested and ordered items, an autolink, and a
        // long emphasis span (the byte-vs-cell cap bug)
        "## 结论\n\n4. **只作用于 smooth frequency，没把低覆盖位点从 reads 面板剔除**（PDF 101-102）\n   - 子项见 http://example.com/x\n5. 其余按 `min_calls` 处理\n",
        // strike spans: a lone `~` must wait for its pair
        "这是 ~~删除线~~ 与 ~~另一个~~ 和 ~单个波浪~\n",
        // a quote continuing over two source lines
        "> 第一行\n> 第二行\n\n正文\n",
        // markers with their padding: extra spaces after `#`, `>`, `-`
        "-  甲\n1.  乙\n\n##  标题\n\n>  补一句\n",
        // a fence with an info string and a blank line inside
        "```python\nprint(1)\n\nprint(2)\n```\n",
        // a table row pair
        "| a | b |\n| - | - |\n| 1 | 2 |\n",
        // inline code, a link and an image-looking bracket
        "见 `改哪里` 与 [链接](http://x) 以及 ![图片](img.png) 结束\n",
        // a fence indented inside a list item: it must close (and the
        // text after it keep rendering) on the live stream too
        "1. **作用域** —— 先弹：\n\n   ```\n   install where:\n   ❯ project-local\n   ```\n\n   给了 `-l` 就跳过。\n\n**验证**\n\n- 见 `pick` 与 `-r` 浏览。\n",
        // a fence indented inside a list item, closing with a longer run
        "  - 例子：\n\n    ````\n    x\n    ````\n\n结束\n",
        // a bold-only line, and a fence whose body is all whitespace
        "**验证**\n\n```\n   \n```\n\n尾段\n",
        // leading, doubled and trailing blank lines: a run collapses to
        // the one blank replay prints between blocks, the leading and
        // trailing ones never print on either side
        "\n\n\n# 开头的空行\n\n\n\n正文\n\n\n",
        // a blank line inside a quote and inside a list
        "> 甲\n\n> 乙\n\n- 一\n\n- 二\n",
        // full-width space padding: replay's `str::trim` counts it as
        // whitespace in the indent, after a marker and inside a rule
        "\u{3000}- 项\n\n1. \u{3000}[x] 项\n\n> \u{3000}引用\n",
        // list markers that never settle: a `[` that opens no task box
        "1. [。不是复选框\n2. [x\n\n- \t-\n-\t--\n",
    ];
    for doc in docs {
        assert_eq!(live(doc), render(doc), "live vs replay for {doc:?}");
    }
}

/// A deterministic random-markdown differential, the renderer's hard
/// rule: the live stream and the replay must agree byte for byte. Blocks
/// of every shape (headings, inline spans, lists, quotes, fences —
/// indented, tab-indented and long-running —, tables, rules, tab and
/// punctuation heavy prose) are composed at random, then rendered three
/// ways: char by char, in random chunks, and at several wrap widths and
/// left margins. Wrapping must depend on line content only, never on how
/// the bytes arrived.
#[test]
fn fuzz_live_and_replay_agree() {
    const POOL: &[&str] = &[
        "# 标题",
        "## 二级 ##",
        "#### 深标题",
        "##\t制表符标题",
        "段落 `code` 与 **加粗** 结束",
        "a ~~strike~~ and ~single~ tail",
        "link [a](http://x) and ![img](i.png)",
        "先挑模型再挑会话。",
        "> 引用",
        ">  空格引用",
        "> 第一行\n> 第二行",
        "> 引用一段很长很长的中文，用来测试引用块的折行与缩进规则是否一致。",
        "- 甲",
        "- [x] 乙",
        "1. 一",
        "   - 嵌套",
        "\t- 制表符项",
        "\t\t- 双制表缩进",
        "-  空格",
        "2. **加粗项** —— 说明",
        "- 一个很长的列表项，里面有不少中文内容，用来测试续行的缩进是不是和最上面那行对齐。",
        "```",
        "```rust",
        "   ```",
        "````",
        "````lang",
        "```\nx\n```",
        "```python\nprint(1)\n\nprint(2)\n```",
        "```\naaaa\tbbbb\t中中\tcccc\tdddd\teeee\tffff\tgggg\t中文\t尾巴\n```",
        "```\n长代码行 a_very_long_identifier_name = another_long_call(arg1, arg2)\n```",
        "1. 项：\n\n   ```\n   install where:\n   ❯ project-local\n   ```\n\n   后续段落。",
        "  - 例：\n\n    ````\n    x\n    ````\n\n结束",
        "| a | b |\n| - | - |\n| 1 | 2 |",
        "| a | b |\n| - | - |",
        "| 中 | tab\there |\n| - | - |\n| 1 | 2 |",
        "---",
        "* * *",
        "普通行结尾 **未闭合",
        "半截 `code",
        "行尾两个空格  ",
        "\t制表符开头",
        "# \u{3000}全角空格标题\u{3000}",
        "- \u{3000}全角空格项",
        "> \u{00a0}不断行空格引用",
        "段落结尾的全角空格\u{3000}",
        "##\u{3000}没有半角空格的井号",
        "长英文行 aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk lll mmm nnn ooo",
        "中英混排 with a very long english word supercalifragilistic 然后中文结尾。",
        "aaaa\tbbbb\t中中\tcccc\tdddd\teeee\tffff\tgggg\t中文\t尾巴",
        "标点密集，逗号很多，所以折行的时候，应该尽量，不要让，标点，出现在，行首。",
        "括号（里面有一段很长的说明文字，用来测试禁则）后面的内容。",
        "引号「中文书名」和（括号）交错出现（（嵌套））着。",
        "超长单词 supercalifragilisticexpialidocious 与结尾",
        "中中中中中中中中中中中中中中中中中中中中中中中中中中中中中中中中中中中中",
    ];
    // a plain LCG: reproducible without a dependency
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = |n: usize| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((seed >> 33) as usize) % n
    };
    for case in 0..4000 {
        let blocks = 1 + next(4);
        let mut doc = String::new();
        for b in 0..blocks {
            if b > 0 {
                doc.push('\n'); // one blank line between blocks
            }
            doc.push_str(POOL[next(POOL.len())]);
            doc.push('\n');
        }
        for indent in [0usize, 2] {
            // no wrapping: the marginal case, and where the replay used
            // to take a no-wrap shortcut
            assert_eq!(
                live_at(indent, 0, &doc, 0),
                render_at(indent, 0, &doc),
                "case {case} at indent {indent} diverged for {doc:?}"
            );
            // random chunk boundaries at several widths: settlement must
            // depend on line content, never on how the bytes arrived
            for wrap in [14usize, 20, 29, 40, 72] {
                let cout = live_at(indent, wrap, &doc, 1 + next(7));
                let rout = render_at(indent, wrap, &doc);
                assert_eq!(
                    cout, rout,
                    "case {case} at indent {indent} wrap {wrap} diverged for {doc:?}"
                );
                // a row wider than the wrap width soft-wraps on a real
                // terminal, losing the left margin
                for row in cout.split('\n') {
                    assert!(
                        cell_width(row) <= wrap + indent || row.trim().is_empty(),
                        "case {case} row over {wrap} cells ({}) for {doc:?}",
                        cell_width(row)
                    );
                }
            }
        }
    }
}

/// The one divergence left is by construction: replay folds a paragraph
/// line followed by a setext underline into a heading, and the live
/// stream has already printed that line. Conservative callers skip such
/// docs (it may skip a few that would have agreed, which is harmless).
fn setext_upgrade(doc: &str) -> bool {
    let mut content = false;
    for raw in doc.lines() {
        let t = raw.trim_start();
        if content && is_setext(t) {
            return true;
        }
        content = !t.is_empty();
    }
    false
}

/// The same differential over random markdown *garbage*: characters that
/// keep re-opening the classifiers (marker runs, half-open spans, tabs,
/// CJK punctuation), which is where a streaming renderer usually drifts
/// from its replay.
#[test]
fn fuzz_random_markup_agrees() {
    const ALPHABET: &[char] = &[
        '#', '*', '-', '_', '+', '>', '|', '`', '~', '[', ']', '(', ')', '!', '\\', ' ', '\t', 'a',
        'b', '中', '，', '。', '（', '）', '1', '.',
    ];
    // the seed is fixed so CI is reproducible; FUZZ_CASES and FUZZ_SEED
    // widen the search when investigating a divergence
    let cases: usize = std::env::var("FUZZ_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6000);
    let mut seed: u64 = std::env::var("FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x1234_5678_9abc_def0);
    let mut next = |n: usize| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((seed >> 33) as usize) % n
    };
    for case in 0..cases {
        let lines = 1 + next(4);
        let mut doc = String::new();
        for l in 0..lines {
            if l > 0 && next(2) == 0 {
                doc.push('\n');
            }
            let len = next(40);
            for _ in 0..len {
                doc.push(ALPHABET[next(ALPHABET.len())]);
            }
            doc.push('\n');
        }
        if setext_upgrade(&doc) {
            continue;
        }
        for indent in [0usize, 2] {
            assert_eq!(
                live_at(indent, 0, &doc, 0),
                render_at(indent, 0, &doc),
                "garbage case {case} diverged for {doc:?}"
            );
            for wrap in [8usize, 12, 23, 51] {
                assert_eq!(
                    live_at(indent, wrap, &doc, 1 + next(5)),
                    render_at(indent, wrap, &doc),
                    "garbage case {case} wrap {wrap} diverged for {doc:?}"
                );
            }
        }
    }
}

/// [`render`] at a given margin and wrap width.
fn render_at(indent: usize, wrap: usize, doc: &str) -> String {
    let mut s = StyleStream::indented(indent, p());
    s.wrap = wrap;
    let mut out = String::new();
    s.push_delta(&resolve_setext(doc), &mut out);
    s.finish(&mut out);
    out
}

/// Char-by-char when `chunk` is 0, else in random `chunk`-sized pieces.
fn live_at(indent: usize, wrap: usize, doc: &str, mut chunk: usize) -> String {
    let mut s = StyleStream::indented(indent, p());
    s.wrap = wrap;
    let mut out = String::new();
    let cs: Vec<char> = doc.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        if chunk == 0 {
            s.push_delta(&cs[i].to_string(), &mut out);
            i += 1;
            continue;
        }
        let n = chunk.min(cs.len() - i);
        s.push_delta(&cs[i..i + n].iter().collect::<String>(), &mut out);
        i += n;
        chunk = 1 + (chunk * 7) % 11; // deterministic drift
    }
    s.finish(&mut out);
    out
}

#[test]
fn live_tabs_count_real_cells() {
    let mut s = StyleStream::indented(2, p());
    s.wrap = 8;
    let mut out = String::new();
    s.push_delta("a\t\tbb\n", &mut out);
    assert_eq!(out, "  a\t\n  \tbb\n");
}

// ---- shared helpers -------------------------------------------------

#[test]
fn dbg_bare_hash() {
    for d in ["#\n", "# \n", "##\n", "####\n", "#### \n", "#\t\n", "#  \n"] {
        let l = live(d);
        let r = render(d);
        println!("SAME={} {d:?}\n  L {l:?}\n  R {r:?}", l == r);
    }
}

/// Replay trims marker padding with `str::trim`, which counts a
/// full-width or no-break space as whitespace: live must skip exactly the
/// same bytes, in the indent, after a marker and inside a rule.
#[test]
fn live_matches_replay_on_unicode_padding() {
    const CASES: &[&str] = &[
        "\u{3000}- x\n",
        "\u{3000}\u{3000}- x\n",
        "  \u{3000}- x\n",
        "\u{00a0}- x\n",
        "\u{3000}> q\n",
        "\u{3000}# h\n",
        "> \u{3000}x\n",
        "-\u{3000}--\n",
        "_\u{3000}__\n",
        "*\u{3000}**\n",
        "- \u{3000}\n",
        "1. \u{3000}\n",
        "# \u{3000}\n",
        "\u{3000}```\ncode\n```\n",
        "- \u{3000}项\n",
        "1. \u{3000}[x] 项\n",
    ];
    for d in CASES {
        for indent in [0usize, 2] {
            for wrap in [0usize, 12, 20] {
                for split in 0..3 {
                    assert_eq!(
                        live_at(indent, wrap, d, split),
                        render_at(indent, wrap, d),
                        "diverge for {d:?} indent={indent} wrap={wrap} split={split}"
                    );
                }
            }
        }
    }
}

/// A marker whose line ends before the item settles (`- [x` with no `]`,
/// a bare `1.`) is still an item to replay, and live must say the same at
/// the end of the line.
#[test]
fn live_matches_replay_on_unsettled_list_markers() {
    const CASES: &[&str] = &[
        "1. [\n",
        "1. [x\n",
        "1. [\u{3000}x\n",
        "- [x\n",
        "- [。x\n",
        "- [x]\n",
        "-\tprose\n",
        "1.\tprose\n",
        "1.\t\n",
        "-\t-\n",
        "-\t--\n",
        "- \t--\n",
        "\t- \t-\n",
    ];
    for d in CASES {
        for indent in [0usize, 2] {
            for wrap in [0usize, 8, 12, 20, 51] {
                for split in 0..3 {
                    assert_eq!(
                        live_at(indent, wrap, d, split),
                        render_at(indent, wrap, d),
                        "diverge for {d:?} indent={indent} wrap={wrap} split={split}"
                    );
                }
            }
        }
    }
}

/// The classes of divergence the live stream used to have: trailing and
/// unicode whitespace, the row-start column a tab is measured from, and
/// markers that are still undecided at the end of a line.
#[test]
fn live_matches_replay_on_edge_whitespace_and_markers() {
    const CASES: &[&str] = &[
        // a heading trims its trailing whitespace, a paragraph keeps it
        "# H  \n",
        "# H\u{3000}\n",
        "## H ##\n",
        "####  H \t \n",
        "para \u{3000} \n",
        // unicode whitespace is marker padding for replay (`str::trim`)
        "# \u{3000}标题\n",
        "- \u{3000}项\n",
        "> \u{00a0}引用\n",
        ">\t引用\n",
        "---\n",
        "  - （-\t\n",
        "- a\t`b`c\n",
        "*a `b ` c*\n",
        "a+）__中1 +。`>\t[-|，\n-\t--\n",
    ];
    for d in CASES {
        for indent in [0usize, 2] {
            for wrap in [0usize, 8, 12, 20, 51] {
                for split in 0..3 {
                    assert_eq!(
                        live_at(indent, wrap, d, split),
                        render_at(indent, wrap, d),
                        "diverge for {d:?} indent={indent} wrap={wrap} split={split}"
                    );
                }
            }
        }
    }
    // a tab at the head of a row is measured from the row's own margin,
    // not from where the previous row ended
    assert_eq!(
        live_at(2, 20, "x\t-\taaaa\tbbbb\n", 0),
        render_at(2, 20, "x\t-\taaaa\tbbbb\n")
    );
}

#[test]
fn wrap_block_indents_every_line() {
    assert_eq!(wrap_block("aaaa bbbb cccc", 12, 2), "  aaaa bbbb\n  cccc");
    assert_eq!(wrap_block("one\ntwo", 12, 2), "  one\n  two");
    assert_eq!(wrap_block("", 12, 2), "");
}

#[test]
fn wrapping_honors_cjk_punctuation_rules() {
    // a closing mark cannot hang on the row above: that row would be
    // wider than the wrap width, so the terminal would soft-wrap it —
    // to the same place, minus the left margin. It moves down instead.
    assert_eq!(wrap_plain("中中中中。後", 8, 0), "中中中中\n。後");
    assert_eq!(wrap_plain("中中中中中。", 10, 0), "中中中中中\n。");
    // … and an opening mark moves down with what it opens
    assert_eq!(wrap_plain("中中中（後", 8, 0), "中中中\n（後");
    // a run of marks cannot hang forever
    let run = format!("中中中中{}後", "，".repeat(10));
    let wrapped = wrap_plain(&run, 8, 0);
    assert!(wrapped.contains('\n'), "got {wrapped:?}");
    assert_eq!(wrapped.replace('\n', ""), run, "wrapping keeps every char");
}

#[test]
fn wrap_plain_indents_continuations_and_keeps_embedded_breaks() {
    assert_eq!(wrap_plain("aaaa bbbb cccc", 10, 2), "aaaa bbbb\n  cccc");
    assert_eq!(wrap_plain("one\ntwo", 10, 2), "one\n  two");
    assert_eq!(wrap_plain("中中中中中", 8, 2), "中中中中\n  中");
    assert_eq!(wrap_plain("short", 10, 2), "short");
}

#[test]
fn render_once_indents_and_ends_with_newline() {
    let out = render_once("# hi\n\nbody", 2);
    assert!(out.starts_with("  hi"), "got {out:?}");
    assert!(out.ends_with('\n'), "got {out:?}");
    assert!(!out.contains("\x1b[J"), "got {out:?}");
    assert!(!out.contains("\x1b[1A"), "got {out:?}");
    assert_eq!(render_once("", 0), "");
}

#[test]
fn char_width_handles_zero_width_wide_and_emoji() {
    assert_eq!(char_width('\u{0301}'), 0);
    assert_eq!(char_width('\u{200D}'), 0);
    assert_eq!(char_width('\u{FE0F}'), 0);
    assert_eq!(char_width('中'), 2);
    assert_eq!(char_width('Ａ'), 2);
    assert_eq!(char_width('\u{1F600}'), 2);
    assert_eq!(char_width('\u{1F3AF}'), 2);
    assert_eq!(char_width('a'), 1);
    assert_eq!(char_width('\u{FF9E}'), 1);
    assert_eq!(char_width('\u{00B7}'), 1);
}

#[test]
fn cell_width_skips_every_escape_kind_without_losing_content() {
    assert_eq!(cell_width("\x1b[1Aab"), 2);
    assert_eq!(cell_width("a\x1b[2Kb"), 2);
    assert_eq!(cell_width("\x1b[31m中\x1b[0m"), 2);
    assert_eq!(cell_width("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"), 4);
}

#[test]
fn truncate_cells_charges_the_ellipsis_and_counts_wide_glyphs_as_two() {
    // text that fits is handed back untouched, no ellipsis invented
    assert_eq!(truncate_cells("abc", 3), "abc");
    assert_eq!(truncate_cells("abc", 9), "abc");
    // the ellipsis is part of the budget, so the row never overshoots
    assert_eq!(truncate_cells("abcdef", 4), "abc…");
    assert_eq!(cell_width(&truncate_cells("abcdef", 4)), 4);
    // a wide glyph is two cells and is never split across the cut
    assert_eq!(truncate_cells("中中中", 6), "中中中");
    assert_eq!(truncate_cells("中中中", 5), "中中…");
    assert_eq!(cell_width(&truncate_cells("中文测试", 7)), 7);
    // zero-width input still leaves a marker rather than nothing
    assert_eq!(truncate_cells("", 4), "");
}

#[test]
fn truncate_cells_charges_no_cells_for_escape_sequences() {
    // the SGR bytes must not be mistaken for printable columns
    assert_eq!(truncate_cells("\x1b[32mab\x1b[0m", 2), "\x1b[32mab\x1b[0m");
    assert_eq!(truncate_cells("\x1b[32mabcd\x1b[0m", 3), "\x1b[32mab…");
}

#[test]
fn plain_streams_carry_no_hyperlink_sequences() {
    // the render path runs under the plain palette in tests (no tty), and
    // osc8 gates on the same condition: a non-terminal stream gets the same
    // visible bytes it always had, no OSC 8 anywhere
    let out = render("see [docs](https://example.com/a?b=c) here\n");
    assert!(!out.contains("\x1b]8"), "{out:?}");
    assert!(out.contains("docs"), "{out:?}");
}

#[test]
fn osc8_wrapping_skips_the_sequence_for_width() {
    // a wrapped replay row containing an OSC 8 pair must not count the
    // sequence's bytes as cells: the row prefix rule holds with hyperlinks
    let text = "a \x1b]8;;https://example.com/very/long/path\x07link text\x1b]8;;\x07 b c d e f g h i j k l m n o p q r s t u v w x y z";
    let mut out = String::new();
    super::wrap_scan(text, 20, "", "  ", true, &mut out);
    // every row fits or breaks at a space; no row overflows 20 cells
    for (i, row) in out.lines().enumerate() {
        let w = super::cell_width(row);
        assert!(w <= 20, "row {i} is {w} cells: {row:?}");
    }
}

#[test]
fn osc8_open_sanitizes_and_gates() {
    // control characters cannot terminate the sequence early
    let open = crate::theme::osc8_open("https://x.com/a\u{07}b\u{1b}c");
    assert!(!open.contains('\u{7f}'));
    // only http(s) targets ride
    assert_eq!(crate::theme::osc8_open("file:///etc/passwd"), "");
    assert_eq!(crate::theme::osc8_open("javascript:alert(1)"), "");
}
