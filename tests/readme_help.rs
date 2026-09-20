//! README.md embeds every command's `-h` output verbatim. Keeping that true by
//! hand is a chore nobody remembers after a flag change, so the README blocks
//! are diffed against what the binary actually prints: a drifted block fails
//! the suite instead of shipping as documentation.

use std::process::Command;

const README: &str = include_str!("../README.md");

/// The fenced block containing `marker`: the nearest fence above it opens the
/// block, the next one closes it. (Scanning fences in pairs would need the
/// README's fences to be balanced, which inline samples break.)
fn readme_block(marker: &str) -> String {
    let lines: Vec<&str> = README.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains(marker))
        .unwrap_or_else(|| panic!("README never mentions {marker:?}"));
    let open = (0..at)
        .rev()
        .find(|i| lines[*i].starts_with("```"))
        .unwrap_or_else(|| panic!("no fence opens the {marker:?} block"));
    let close = (at + 1..lines.len())
        .find(|i| lines[*i].starts_with("```"))
        .unwrap_or_else(|| panic!("no fence closes the {marker:?} block"));
    lines[open + 1..close].join("\n")
}

/// `llm ARGS` as it would print on a terminal, one-shot and hermetic.
fn rendered(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_llm"))
        .args(args)
        .env("LLM_USER_PATH", env!("CARGO_TARGET_TMPDIR"))
        .output()
        .expect("run llm");
    assert!(out.status.success(), "llm {args:?} failed");
    String::from_utf8(out.stdout).expect("help is UTF-8")
}

/// Trailing newlines are not what the blocks are about.
fn assert_matches_readme(marker: &str, args: &[&str]) {
    let block = readme_block(marker);
    let live = rendered(args);
    assert_eq!(
        live.trim_end(),
        block.trim_end(),
        "README's {marker:?} block is stale — refresh it from `llm {}`",
        args.join(" ")
    );
}

#[test]
fn the_readme_top_level_help_block_matches_the_binary() {
    assert_matches_readme("llm [flags] [PROMPT]", &["--help"]);
}

#[test]
fn the_readme_install_help_block_matches_the_binary() {
    assert_matches_readme("Usage: llm install", &["install", "-h"]);
}

#[test]
fn the_readme_export_help_block_matches_the_binary() {
    assert_matches_readme("Usage: llm export", &["export", "-h"]);
}

#[test]
fn the_readme_remove_help_block_matches_the_binary() {
    assert_matches_readme("Usage: llm remove", &["remove", "-h"]);
}

#[test]
fn the_readme_list_help_block_matches_the_binary() {
    assert_matches_readme("Usage: llm list", &["list", "-h"]);
}
