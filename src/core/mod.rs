//! The shared kernel: infrastructure and services used by two or more
//! commands or domains. Single-domain code lives in its own top-level
//! module (agent/, providers/, term/) instead.

pub mod args;
pub mod attachments;
pub mod commands_md;
pub mod config;
pub mod db;
pub mod http;
pub mod logstore;
pub mod render;
pub mod render_md;
pub mod schemas;
pub mod templates;
pub mod text;
