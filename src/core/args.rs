//! Hand-rolled argument parser — zero-dependency replacement for clap.
//!
//! Supports: long/short flags, `k=v` tuple options, multi-value options,
//! `--` terminator, and positional collection. Subcommands are dispatched
//! by name before option parsing (matching click's group semantics).

use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct ParsedArgs {
    /// positional arguments in order
    pub positionals: Vec<String>,
    /// flag presence (e.g. "-n", "--no-log")
    pub flags: Vec<String>,
    /// single-value options (e.g. "-m value")
    pub options: BTreeMap<String, String>,
    /// multi-value options (e.g. "-o k=v" repeated)
    pub multi: BTreeMap<String, Vec<String>>,
}

impl ParsedArgs {
    pub fn flag(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.flags.iter().any(|f| f == n))
    }

    pub fn opt(&self, names: &[&str]) -> Option<&str> {
        names
            .iter()
            .find_map(|n| self.options.get(*n).map(|s| s.as_str()))
    }

    pub fn multi(&self, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .flat_map(|n| self.multi.get(*n).cloned().unwrap_or_default())
            .collect()
    }

    pub fn first_positional(&self) -> Option<&str> {
        self.positionals.first().map(|s| s.as_str())
    }
}

/// Spec for one option, used for parsing and `--help` generation.
#[derive(Clone, Copy)]
pub struct OptSpec {
    /// long name without dashes, e.g. "model"
    pub long: &'static str,
    /// short name without dash, e.g. 'm' (optional)
    pub short: Option<char>,
    /// number of values consumed (0 = flag)
    pub takes_value: usize,
    /// may be repeated?
    pub multiple: bool,
    pub help: &'static str,
    /// value shown in help, e.g. "TEXT" or "KEY=VALUE"
    pub value_name: &'static str,
}

/// flag spec:  spec!("name", Some('n'), "help text")
#[macro_export]
macro_rules! flag_spec {
    ($long:expr, $short:expr, $help:expr) => {
        $crate::core::args::OptSpec {
            long: $long,
            short: $short,
            takes_value: 0,
            multiple: false,
            help: $help,
            value_name: "",
        }
    };
}

/// value spec:  value_spec!("name", Some('n'), "help text", "TEXT")
#[macro_export]
macro_rules! value_spec {
    ($long:expr, $short:expr, $help:expr, $value:expr) => {
        $crate::core::args::OptSpec {
            long: $long,
            short: $short,
            takes_value: 1,
            multiple: false,
            help: $help,
            value_name: $value,
        }
    };
}

/// repeated value spec:  multi_spec!("option", Some('o'), "help", "KEY=VALUE")
#[macro_export]
macro_rules! multi_spec {
    ($long:expr, $short:expr, $help:expr, $value:expr) => {
        $crate::core::args::OptSpec {
            long: $long,
            short: $short,
            takes_value: 1,
            multiple: true,
            help: $help,
            value_name: $value,
        }
    };
}

/// two-value spec: `--at <PATH> <MIMETYPE>` (original click tuple options)
#[macro_export]
macro_rules! two_value_spec {
    ($long:expr, $help:expr, $value:expr) => {
        $crate::core::args::OptSpec {
            long: $long,
            short: None,
            takes_value: 2,
            multiple: true,
            help: $help,
            value_name: $value,
        }
    };
}

/// Parse `argv` (without program name) against a spec table.
///
/// Both `--long value` / `--long=value` and `-s value` / `-svalue` forms are
/// accepted. A short option that takes a value can also be bundled with it
/// directly (e.g. `-mdeepseek-chat`). Flags may be combined (`-nx`).
pub fn parse(argv: &[String], specs: &[OptSpec]) -> Result<ParsedArgs, String> {
    let mut out = ParsedArgs::default();
    let find = |name: &str| -> Option<&OptSpec> {
        specs
            .iter()
            .find(|s| name == s.long || name.len() == 1 && s.short == name.chars().next())
    };

    let mut i = 0;
    let mut positional_only = false;
    while i < argv.len() {
        let arg = &argv[i];
        if positional_only {
            out.positionals.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            positional_only = true;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let Some(spec) = find(name) else {
                return Err(format!("Error: No such option: --{name}"));
            };
            if spec.takes_value == 0 {
                out.flags.push(spec.long.to_string());
            } else if spec.takes_value == 2 {
                // two-value options: --at path mime (no inline form)
                if inline.is_some() {
                    return Err(format!(
                        "Error: Option '--{}' takes two separate values",
                        spec.long
                    ));
                }
                let mut values = Vec::new();
                for _ in 0..2 {
                    i += 1;
                    values.push(
                        argv.get(i)
                            .ok_or_else(|| {
                                format!("Error: Option '--{}' requires two values", spec.long)
                            })?
                            .clone(),
                    );
                }
                let entry = out.multi.entry(spec.long.to_string()).or_default();
                entry.extend(values);
            } else {
                let value = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        argv.get(i)
                            .ok_or_else(|| {
                                format!("Error: Option '--{}' requires a value", spec.long)
                            })?
                            .clone()
                    }
                };
                if spec.multiple {
                    out.multi
                        .entry(spec.long.to_string())
                        .or_default()
                        .push(value);
                } else {
                    out.options.insert(spec.long.to_string(), value);
                }
            }
        } else if arg.starts_with('-') && arg.len() > 1 {
            // short option cluster, e.g. -nx, -mmodel, -m model
            let chars: Vec<char> = arg.chars().skip(1).collect();
            let mut ci = 0;
            while ci < chars.len() {
                let c = chars[ci];
                let name = c.to_string();
                let Some(spec) = find(&name) else {
                    return Err(format!("Error: No such option: -{c}"));
                };
                if spec.takes_value == 0 {
                    out.flags.push(spec.long.to_string());
                    ci += 1;
                } else if spec.takes_value == 2 {
                    let mut values: Vec<String> = chars[ci + 1..]
                        .iter()
                        .collect::<String>()
                        .split_whitespace()
                        .map(String::from)
                        .collect();
                    if values.len() < 2 {
                        for _ in values.len()..2 {
                            i += 1;
                            values.push(
                                argv.get(i)
                                    .ok_or_else(|| {
                                        format!("Error: Option '-{c}' requires two values")
                                    })?
                                    .clone(),
                            );
                        }
                    }
                    let entry = out.multi.entry(spec.long.to_string()).or_default();
                    entry.extend(values);
                    ci = chars.len();
                } else {
                    // rest of cluster is the value, else next argv
                    let rest: String = chars[ci + 1..].iter().collect();
                    let value = if !rest.is_empty() {
                        rest
                    } else {
                        i += 1;
                        argv.get(i)
                            .ok_or_else(|| format!("Error: Option '-{c}' requires a value"))?
                            .clone()
                    };
                    if spec.multiple {
                        out.multi
                            .entry(spec.long.to_string())
                            .or_default()
                            .push(value);
                    } else {
                        out.options.insert(spec.long.to_string(), value);
                    }
                    ci = chars.len();
                }
            }
        } else {
            out.positionals.push(arg.clone());
        }
        i += 1;
    }
    Ok(out)
}

/// Render a click-style help block from specs.
pub fn render_help(
    usage: &str,
    about: &str,
    specs: &[OptSpec],
    args_help: &[(&str, &str)],
) -> String {
    let mut s = String::new();
    s.push_str(about);
    s.push_str("\n\nUsage: ");
    s.push_str(usage);
    s.push_str(" [OPTIONS] ");
    s.push_str(
        &args_help
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<&str>>()
            .join(" "),
    );
    s.push_str("\n\nOptions:\n");
    // specs whose help is "(alias of --target)" fold into the target's row
    // instead of printing a row of their own
    let mut aliases: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for spec in specs {
        if let Some(target) = spec
            .help
            .strip_prefix("(alias of --")
            .and_then(|t| t.strip_suffix(')'))
        {
            aliases.entry(target).or_default().push(spec.long);
        }
    }
    let mut rows: Vec<(String, &str)> = Vec::new();
    for spec in specs {
        if spec.help.starts_with("(alias of --") {
            continue;
        }
        let mut left = String::new();
        if let Some(c) = spec.short {
            left.push_str(&format!("-{c}, "));
        } else {
            left.push_str("    ");
        }
        left.push_str(&format!("--{}", spec.long));
        if let Some(names) = aliases.get(spec.long) {
            for name in names {
                left.push_str(&format!(", --{name}"));
            }
        }
        if spec.takes_value > 0 {
            left.push_str(&format!(" {}", spec.value_name));
        }
        rows.push((left, spec.help));
    }
    // specs that already declare their own help flag must not get a second
    if !specs.iter().any(|s| s.long == "help") {
        rows.push(("-h, --help".to_string(), "Show this message and exit"));
    }
    let width = rows
        .iter()
        .map(|(l, _)| l.len())
        .max()
        .unwrap_or(20)
        .max(20);
    for (left, help) in rows {
        s.push_str(&format!("  {:<width$}  {}\n", left, help, width = width));
    }
    s
}

/// Split argv for a click-style DefaultGroup: if the first token starts with
/// `-` it is an option, so the default subcommand applies to the whole argv.
pub fn split_subcommand<'a>(argv: &'a [String], default: &'a str) -> (&'a str, &'a [String]) {
    match argv.first() {
        Some(first) if !first.starts_with('-') => (first.as_str(), &argv[1..]),
        _ => (default, argv),
    }
}

#[cfg(test)]
mod tests {
    use super::{OptSpec, render_help};

    #[test]
    fn alias_specs_fold_into_the_target_row() {
        let specs: &[OptSpec] = &[
            value_spec!("conversation", None, "Continue a conversation", "ID"),
            value_spec!("cid", None, "(alias of --conversation)", "ID"),
        ];
        let help = render_help("llm t", "T", specs, &[]);
        assert!(help.contains("--conversation, --cid ID"));
        assert!(!help.contains("(alias of"));
    }
}
