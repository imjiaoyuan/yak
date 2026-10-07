//! yak — a minimal terminal coding harness in Rust (the reference-shaped).

mod agent;
mod b64;
mod commands;
mod core;
mod jsonfmt;
mod platform;
mod providers;
mod read;
mod term;
mod theme;
mod yaml;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const ABOUT: &str = "\
Access Large Language Models from the command-line

Usage:
  yak [flags] [PROMPT]

Bare `yak` opens an interactive agent session; `yak \"task\"` runs the
agent once with tools.

Available commands:
  export     Export a conversation as markdown (also: /export)
  install    Install a git package (also: remove, list)

Flags:
  -h, --help      Show this message and exit
  -v, --version   Show the version number
";

fn main() {
    crate::platform::init_console();
    // SIGPIPE stays ignored (the std default): a broken pipe or socket — a
    // dead extension child, a provider that hung up, `yak logs | head` —
    // surfaces as a write error instead of killing the process silently.
    // The one user-visible case, output into a closed shell pipe, panics
    // inside print!/eprint!; the hook below exits quietly on it, which is
    // what a SIGPIPE death used to do.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info.to_string().contains("Broken pipe") {
            std::process::exit(0);
        }
        default_hook(info);
    }));
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = dispatch(&argv);
    crate::platform::restore_console();
    std::process::exit(code);
}

fn dispatch(argv: &[String]) -> i32 {
    match argv.first().map(String::as_str) {
        Some("--version" | "-v") => {
            println!("yak, version {VERSION}");
            0
        }
        Some("--help" | "-h" | "help") => {
            print!("{ABOUT}");
            0
        }
        Some("export") => commands::export::run(argv),
        Some("install") | Some("remove") | Some("uninstall") | Some("list") => {
            commands::pkg::run(argv)
        }
        // anything else (flags or plain text) is the agent: bare `yak` on a
        // terminal is the interactive REPL, text and pipes are one-shot tasks
        _ => commands::agent::run(argv),
    }
}
