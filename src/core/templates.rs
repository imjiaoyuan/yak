//! Prompt templates: YAML files under the user dir, original-llm style —
//! loading, `$var` substitution and application. Internal engine behind
//! custom commands; there is no `llm templates` CLI.

use std::collections::BTreeMap;

#[derive(Debug, Default, Clone)]
pub struct Template {
    pub prompt: Option<String>,
    pub system: Option<String>,
}

/// Collect `$var` / `${var}` names used in a template body.
fn template_vars(text: &str) -> Vec<String> {
    let mut vars = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            continue;
        }
        match chars.peek() {
            Some('{') => {
                chars.next();
                let mut name = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    name.push(c);
                }
                if !name.is_empty() && is_ident(&name) && !vars.contains(&name) {
                    vars.push(name);
                }
            }
            Some(&c2) if is_ident_start(c2) => {
                let mut name = String::new();
                name.push(c2);
                chars.next();
                while let Some(&c2) = chars.peek() {
                    if is_ident_char(c2) {
                        name.push(c2);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if !vars.contains(&name) {
                    vars.push(name);
                }
            }
            _ => {}
        }
    }
    vars
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_ident_char)
}

/// A placeholder name starts with a letter or underscore (Python's
/// string.Template rule): `$5` and `$100` are literal dollar amounts, not
/// variables, so a command body containing them still runs.
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// string.Template-style substitution of `$name` / `${name}`.
pub fn substitute(text: &str, params: &BTreeMap<String, String>) -> Result<String, String> {
    let vars = template_vars(text);
    let missing: Vec<String> = vars
        .iter()
        .filter(|v| !params.contains_key(*v))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(format!("Missing variables: {}", missing.join(", ")));
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                chars.next();
                let mut name = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    name.push(c);
                }
                if is_ident(&name) {
                    out.push_str(params.get(&name).map(|s| s.as_str()).unwrap_or(""));
                } else {
                    // an invalid braced name stays literal text
                    out.push_str(&format!("${{{name}}}"));
                }
            }
            Some(&c2) if is_ident_start(c2) => {
                let mut name = String::new();
                name.push(c2);
                chars.next();
                while let Some(&c2) = chars.peek() {
                    if is_ident_char(c2) {
                        name.push(c2);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(params.get(&name).map(|s| s.as_str()).unwrap_or(""));
            }
            _ => out.push('$'),
        }
    }
    Ok(out)
}

/// Evaluate a template against user input and params (original `_apply_template`).
pub fn apply(
    t: &Template,
    input: &str,
    params: &BTreeMap<String, String>,
) -> Result<(Option<String>, Option<String>), String> {
    let mut all: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in params {
        all.insert(k.clone(), v.clone());
    }
    all.insert("input".to_string(), input.to_string());
    let prompt = match &t.prompt {
        Some(p) => {
            let evaluated = substitute(p, &all)?;
            if template_vars(p).contains(&"input".to_string()) || input.is_empty() {
                Some(evaluated)
            } else {
                Some(format!("{evaluated}\n{input}"))
            }
        }
        None => None,
    };
    let system = match &t.system {
        Some(s) => Some(substitute(s, &all)?),
        None => None,
    };
    Ok((prompt, system))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dollar_amounts_stay_literal() {
        // Python's string.Template idstart rule: a leading digit is not a
        // variable, so a command body may quote prices unpunished
        let params = BTreeMap::new();
        assert_eq!(
            substitute("costs $5 or $100", &params).unwrap(),
            "costs $5 or $100"
        );
        assert_eq!(
            substitute("regex groups: $1 and $2", &params).unwrap(),
            "regex groups: $1 and $2"
        );
    }

    #[test]
    fn invalid_braced_names_stay_literal() {
        let params = BTreeMap::new();
        assert_eq!(
            substitute("shell ${HOME-ish}", &params).unwrap(),
            "shell ${HOME-ish}"
        );
        // valid names still substitute
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), "x".to_string());
        assert_eq!(substitute("hi ${name}!", &p).unwrap(), "hi x!");
    }
}
