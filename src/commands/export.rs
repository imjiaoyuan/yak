//! `llm export [PATH]` — write a conversation as one markdown file: the
//! newest thread of the working directory, else the newest anywhere, i.e.
//! whatever `-c` would have continued. The rendering and writing live in
//! `core::export`; the REPL's `/export` shares that path with the live
//! session id.

use crate::core::args::{OptSpec, render_help};
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
    match crate::core::export::export_latest(&cwd, args.positionals.first().map(String::as_str)) {
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
