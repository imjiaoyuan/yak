//! The shared kernel: infrastructure and services used by two or more
//! commands or domains. Single-domain code lives in its own top-level
//! module (agent/, providers/, term/) instead. One deliberate exception:
//! `render_md` is core's presentation edge: it turns terminal markdown into
//! ANSI without knowing the terminal, so it stays here. The live-streaming
//! `Renderer`/`TaskView` lives in `term::render` where the terminal width and
//! spinner ticker already live — everything else in core stays terminal-agnostic.

pub mod args;
pub mod attachments;
pub mod commands_md;
pub mod config;
pub mod db;
pub mod http;
pub mod paths;
pub mod render_md;
pub mod templates;
pub mod text;
pub mod threads;
