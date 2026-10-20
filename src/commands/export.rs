//! `yak export [PATH]` — write a conversation as one markdown file: the
//! newest thread of the working directory, else the newest anywhere, i.e.
//! whatever `-c` would have continued. The rendering and writing live in
//! `core::export`; the REPL's `/export` shares that path with the live
//! session id.

use crate::core::args::{OptSpec, render_help};
use crate::flag_spec;
use crate::value_spec;

const SPECS: &[OptSpec] = &[
    flag_spec!("help", Some('h'), "Show this message and exit"),
    value_spec!("thread", None, "Thread id to export", "ID"),
    flag_spec!(
        "html",
        None,
        "Write one self-contained HTML file instead of markdown"
    ),
];

fn help() -> String {
    render_help(
        "yak export",
        "Export a conversation as markdown or self-contained HTML",
        SPECS,
        &[(
            "PATH",
            "File to write (default: yak-<session>.md in the working directory)",
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
    let path = args.positionals.first().map(String::as_str);
    let html = args.flag(&["html"]);
    let result = match args.opt(&["thread"]) {
        Some(cid) => {
            if html {
                crate::core::export::export_html_thread(cid, &cwd, path)
                    .map(|path| (cid.to_string(), path))
            } else {
                crate::core::export::export_thread(cid, &cwd, path)
                    .map(|path| (cid.to_string(), path))
            }
        }
        None => {
            // no thread named: what -c would continue, in either format
            let cid = match crate::core::export::latest_thread_id(&cwd) {
                Ok(cid) => cid,
                Err(e) => {
                    eprintln!("Error: {e}");
                    return 1;
                }
            };
            if html {
                crate::core::export::export_html_thread(&cid, &cwd, path).map(|path| (cid, path))
            } else {
                crate::core::export::export_thread(&cid, &cwd, path).map(|path| (cid, path))
            }
        }
    };
    match result {
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
