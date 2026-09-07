//! llm — a unified AI hub for the terminal, in Rust.

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

use std::io::IsTerminal;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const ABOUT: &str = "\
Access Large Language Models from the command-line

Usage:
  llm [flags]
  llm [command]

Bare `llm \"prompt\"` runs the prompt command.

Available commands:
  agent      Run an agentic task with tools
  login      Add a provider
  logs       Show past conversations
  logout     Remove a provider
  models     Pick the default model and its thinking level
  prompt     Execute a prompt

Use \"llm [command] --help\" for more information about a command.

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
    // no args at all + piped stdin → prompt; fully empty tty → help
    let Some(first) = argv.first() else {
        if std::io::stdin().is_terminal() {
            print!("{ABOUT}");
            return 0;
        }
        return commands::prompt::run(argv);
    };

    match first.as_str() {
        "--version" | "-v" | "version" => {
            println!("llm, version {VERSION}");
            0
        }
        "--help" | "-h" | "help" => {
            print!("{ABOUT}");
            0
        }
        "prompt" => commands::prompt::run(&argv[1..]),
        "agent" => commands::agent::run(&argv[1..]),
        "logs" => commands::logs::run(&argv[1..]),
        "models" => commands::models::run(&argv[1..]),
        "login" => commands::login::run_login(&argv[1..]),
        "logout" => commands::login::run_logout(&argv[1..]),
        // anything else (flags or plain text) → default prompt; a word that
        // reads like a mistyped command gets a hint instead of a surprise
        // model call. Custom commands win over the hint: an exact name match
        // (`.llm/commands/log.md`) is not a typo of a builtin
        _ => {
            if let Some(cmd) = core::commands_md::find(first) {
                // commands-dir subcommand: ~/.llm/commands/<name>.md or the
                // nearest .llm/commands/<name>.md (project wins)
                commands::prompt::run_command(&cmd, &argv[1..])
            } else if let Some(hint) = command_hint(first) {
                eprintln!(
                    "'llm {first}' is not a command, closest match is '{hint}'\n\
                     run `llm {hint} ...`, or `llm prompt {first}` to send it to the model;\n\
                     `llm --help` lists every command"
                );
                2
            } else {
                commands::prompt::run(argv)
            }
        }
    }
}

/// Subcommand names and aliases. Keep in sync with the match in `dispatch`
/// above.
const SUBCOMMANDS: &[&str] = &[
    "prompt", "agent", "logs", "models", "login", "logout", "help", "version",
];

/// Suggest a subcommand for a word that is probably a mistyped command: a
/// prefix of at least 3 chars, or a small edit distance scaled to the
/// candidate's length. Returns None when the word reads like a prompt.
fn command_hint(word: &str) -> Option<String> {
    crate::core::text::closest_name(word, SUBCOMMANDS)
}

/// Restore SIG_DFL for SIGPIPE so `llm logs | head` exits cleanly instead of
/// panicking on a broken pipe. Links the libc symbol directly — no libc crate.
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

#[cfg(test)]
mod tests {
    use super::command_hint;

    #[test]
    fn one_edit_off_hits() {
        assert_eq!(command_hint("lgos"), Some("logs".to_string()));
        assert_eq!(command_hint("model"), Some("models".to_string()));
    }

    #[test]
    fn three_char_prefix_hits() {
        assert_eq!(command_hint("mod"), Some("models".to_string()));
        assert_eq!(command_hint("mo"), None);
    }

    #[test]
    fn short_and_flag_words_stay_prompts() {
        assert_eq!(command_hint("go"), None);
        assert_eq!(command_hint("-m"), None);
    }

    #[test]
    fn distant_words_stay_prompts() {
        assert_eq!(command_hint("hello"), None);
        assert_eq!(command_hint("翻译"), None);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe_default() {}
