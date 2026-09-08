//! llm — a minimal terminal coding harness in Rust (the reference-shaped).

mod agent;
mod b64;
mod commands;
mod core;
mod gitignore;
mod jsonfmt;
mod platform;
mod providers;
mod read;
mod term;
mod yaml;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const ABOUT: &str = "\
Access Large Language Models from the command-line

Usage:
  llm [flags] [PROMPT]


Bare `llm` opens an interactive agent session; `llm \"task\"` runs the
agent once with tools.

Flags:
  -h, --help      Show this message and exit
  -v, --version   Show the version number
";

fn main() {
    crate::platform::init_console();
    restore_sigpipe_default();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = dispatch(&argv);
    crate::platform::restore_console();
    std::process::exit(code);
}

fn dispatch(argv: &[String]) -> i32 {
    match argv.first().map(String::as_str) {
        Some("--version" | "-v") => {
            println!("llm, version {VERSION}");
            0
        }
        Some("--help" | "-h" | "help") => {
            print!("{ABOUT}");
            0
        }
        Some("install") | Some("remove") | Some("uninstall") | Some("list") => {
            commands::pkg::run(argv)
        }
        // anything else (flags or plain text) is the agent: bare `llm` on a
        // terminal is the interactive REPL, text and pipes are one-shot tasks
        _ => commands::agent::run(argv),
    }
}

/// Restore SIG_DFL for SIGPIPE so `llm --help | head` exits cleanly instead
/// of panicking on a broken pipe. Links the libc symbol directly — no libc
/// crate.
#[cfg(unix)]
fn restore_sigpipe_default() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe_default() {}
