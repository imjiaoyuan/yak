//! `llm export [PATH]` — write a conversation as one markdown file: the
//! newest thread of the working directory, else the newest anywhere, i.e.
//! whatever `-c` would have continued. The REPL's `/export` shares
//! [`export_thread`], handing it the live session id instead.

use std::path::{Path, PathBuf};

use crate::core::args::{OptSpec, render_help};
use crate::core::threads::Store;
use crate::flag_spec;

const SPECS: &[OptSpec] = &[flag_spec!("help", Some('h'), "Show this message and exit")];

fn help() -> String {
    render_help(
        "llm export",
        "Export a conversation as a markdown file",
        SPECS,
        &[(
            "PATH",
            "File to write (default: llm-<session>.md in the working directory)",
        )],
    )
}

pub fn run(argv: &[String]) -> i32 {
    let rest: Vec<String> = argv.get(1..).unwrap_or_default().to_vec();
    let (args, code) = crate::core::args::parse_with_help(&rest, SPECS, help);
    let Some(args) = args else { return code };
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: cannot read the working directory: {e}");
            return 1;
        }
    };
    match export_latest(&cwd, args.positionals.first().map(String::as_str)) {
        Ok((cid, path)) => {
            println!("exported {cid} to {}", path.display());
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

/// Export this directory's newest conversation (the newest anywhere when it
/// has none), returning the session id and the file written.
pub fn export_latest(cwd: &Path, path: Option<&str>) -> Result<(String, PathBuf), String> {
    let store = Store::open()?;
    let dir = cwd.display().to_string();
    let cid = match store.latest_thread(Some(&dir))? {
        Some(cid) => cid,
        None => store
            .latest_thread(None)?
            .ok_or_else(|| "no conversations to export yet".to_string())?,
    };
    let path = export_thread(&cid, cwd, path)?;
    Ok((cid, path))
}

/// Render thread `cid` to markdown and write it to `path` — a directory
/// takes the default name, a relative one resolves against `cwd`.
pub fn export_thread(cid: &str, cwd: &Path, path: Option<&str>) -> Result<PathBuf, String> {
    let store = Store::open()?;
    let turns = store.read_thread(cid)?;
    let md = crate::core::export::to_markdown(cid, &turns);
    let target = output_path(cwd, path, cid);
    crate::core::fsx::write_atomic(&target, md.as_bytes(), None)
        .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    Ok(target)
}

/// `llm-<id>.md` unless a file name was given; a directory argument gets
/// that name inside it.
fn output_path(cwd: &Path, path: Option<&str>, cid: &str) -> PathBuf {
    let default = || cwd.join(format!("llm-{cid}.md"));
    match path {
        None => default(),
        Some(raw) => {
            let p = Path::new(raw);
            if p.is_dir() {
                p.join(format!("llm-{cid}.md"))
            } else if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_path_defaults_and_resolves() {
        // real dirs under the platform temp dir: a hardcoded `/tmp` is not an
        // absolute path on Windows, and whether it is a directory there is
        // an accident of the drive layout
        let root = crate::core::testutil::scratch_dir("export");
        let cwd = root.join("project");
        let dir = root.join("out");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&dir).unwrap();

        // no argument: the session-named default in cwd
        assert_eq!(output_path(&cwd, None, "01ABC"), cwd.join("llm-01ABC.md"));
        // a relative name resolves against cwd
        assert_eq!(
            output_path(&cwd, Some("notes.md"), "01ABC"),
            cwd.join("notes.md")
        );
        // an existing directory takes the default name inside it
        let as_dir = dir.display().to_string();
        assert_eq!(
            output_path(&cwd, Some(&as_dir), "01ABC"),
            dir.join("llm-01ABC.md")
        );
        // an absolute path to a file is used as given
        let abs = root.join("elsewhere.md");
        let as_abs = abs.display().to_string();
        assert_eq!(output_path(&cwd, Some(&as_abs), "01ABC"), abs);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
