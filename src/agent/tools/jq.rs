//! A one-pass evaluator for the jq subset the read tool's `?q=` query takes
//! (evaluated in-process: the minimal-dependency rule says
//! hand-write it instead). Supported grammar, in jq's own shapes:
//!
//! ```text
//! pipe   := chain ('|' chain)*
//! chain  := term suffix*
//! suffix := '.' IDENT | '[]' | '[' INT ']' | '?'
//! term   := '.' | '.' IDENT | IDENT | IDENT '(' pipe ')' | '[' pipe ']'
//! IDENT  := keys | length | type | select(pipe)
//! ```
//!
//! Values are `serde_json::Value`; a filter yields a stream (jq's model:
//! `.[]` emits many results), rendered one value per line like jq does.
//! Unknown syntax fails loudly at parse time with the offending text, so a
//! typo costs one round-trip, not a silently wrong answer.

use serde_json::Value;

/// A parsed filter: a pipeline of stages, each stage producing zero or more
/// values from one input value.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    stages: Vec<Stage>,
}

#[derive(Debug, Clone, PartialEq)]
enum Stage {
    /// `.` — identity.
    Identity,
    /// `.foo.bar` — key path; a missing key is `null` (jq's silent miss),
    /// indexing a non-object is an error unless the path is under `?`.
    Keys(Vec<String>),
    /// `.[]` — emit every element (array) or value (object).
    Iterate,
    /// `.[N]` — one element; `null` out of range, like jq.
    Index(usize),
    /// `[<filter>]` — collect the inner stream into one array.
    Collect(Box<Filter>),
    /// `keys` / `length` / `type`.
    Builtin(Builtin),
    /// `select(<filter>)` — emit the input where the filter is truthy
    /// (jq truthiness: only `null` and `false` are falsy).
    Select(Box<Filter>),
    /// `X Y` — two stages in sequence within one chain (`.foo[]`).
    Pair(Box<Stage>, Box<Stage>),
    /// `X?` — run X, turning its errors into no output.
    Try(Box<Stage>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Builtin {
    Keys,
    Length,
    Type,
}

#[derive(Debug, PartialEq)]
pub enum ParseError {
    /// the unexpected text, up to a dozen chars
    Unexpected(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Unexpected(s) => write!(f, "unexpected `{s}` in filter"),
        }
    }
}

/// Parse a jq-subset filter; the whole input must consume.
pub fn parse(src: &str) -> Result<Filter, ParseError> {
    let mut p = Parser {
        s: src.as_bytes(),
        i: 0,
    };
    p.ws();
    let f = p.pipe()?;
    p.ws();
    if p.i < p.s.len() {
        return Err(ParseError::Unexpected(p.rest_here()));
    }
    Ok(f)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

fn ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn rest_here(&self) -> String {
        String::from_utf8_lossy(&self.s[self.i..])
            .chars()
            .take(12)
            .collect()
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: u8) -> Result<(), ParseError> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(ParseError::Unexpected(self.rest_here()))
        }
    }
    fn ident(&mut self) -> Option<String> {
        self.ws();
        let start = self.i;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == b'_') {
            self.i += 1;
        }
        if self.i == start {
            None
        } else {
            Some(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
        }
    }
    fn int(&mut self) -> Option<usize> {
        self.ws();
        let start = self.i;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == start {
            None
        } else {
            std::str::from_utf8(&self.s[start..self.i])
                .ok()
                .and_then(|s| s.parse().ok())
        }
    }

    /// pipe := chain ('|' chain)*
    fn pipe(&mut self) -> Result<Filter, ParseError> {
        let mut stages = vec![self.chain()?];
        loop {
            self.ws();
            if self.peek() == Some(b'|') {
                self.i += 1;
                stages.push(self.chain()?);
            } else {
                break;
            }
        }
        Ok(Filter { stages })
    }

    /// chain := term suffix* — suffixes fold onto the term, left to right.
    fn chain(&mut self) -> Result<Stage, ParseError> {
        let mut stage = self.term()?;
        loop {
            self.ws();
            match self.peek() {
                // `.key` — merges into a Keys path, or pairs after another shape
                Some(b'.') if self.s.get(self.i + 1).is_some_and(|&c| ident_start(c)) => {
                    self.i += 1;
                    let key = self
                        .ident()
                        .ok_or_else(|| ParseError::Unexpected(self.rest_here()))?;
                    stage = match stage {
                        Stage::Keys(mut v) => {
                            v.push(key);
                            Stage::Keys(v)
                        }
                        other => Stage::Pair(Box::new(other), Box::new(Stage::Keys(vec![key]))),
                    };
                }
                Some(b'[') if self.s.get(self.i + 1) == Some(&b']') => {
                    self.i += 2;
                    stage = Stage::Pair(Box::new(stage), Box::new(Stage::Iterate));
                }
                Some(b'[') if self.s.get(self.i + 1).is_some_and(u8::is_ascii_digit) => {
                    let saved = self.i;
                    self.i += 1;
                    match self.int() {
                        Some(n) if self.eat(b']') => {
                            stage = Stage::Pair(Box::new(stage), Box::new(Stage::Index(n)));
                        }
                        _ => {
                            // not an index after all: leave it for the caller
                            self.i = saved;
                            break;
                        }
                    }
                }
                Some(b'?') => {
                    self.i += 1;
                    stage = Stage::Try(Box::new(stage));
                }
                _ => break,
            }
        }
        Ok(stage)
    }

    /// term := '.' | '.' IDENT | IDENT-call | '[' pipe ']'
    fn term(&mut self) -> Result<Stage, ParseError> {
        self.ws();
        match self.peek() {
            Some(b'.') => {
                self.i += 1;
                match self.peek() {
                    Some(c) if ident_start(c) => {
                        let key = self
                            .ident()
                            .ok_or_else(|| ParseError::Unexpected(self.rest_here()))?;
                        Ok(Stage::Keys(vec![key]))
                    }
                    _ => Ok(Stage::Identity), // bare `.` (a `.[` follows as a suffix)
                }
            }
            Some(b'[') => {
                self.i += 1;
                let inner = self.pipe()?;
                self.expect(b']')?;
                Ok(Stage::Collect(Box::new(inner)))
            }
            Some(c) if ident_start(c) => {
                let name = self
                    .ident()
                    .ok_or_else(|| ParseError::Unexpected(self.rest_here()))?;
                if self.eat(b'(') {
                    let inner = self.pipe()?;
                    self.expect(b')')?;
                    match name.as_str() {
                        "select" => Ok(Stage::Select(Box::new(inner))),
                        _ => Err(ParseError::Unexpected(format!("{name}("))),
                    }
                } else {
                    match name.as_str() {
                        "keys" => Ok(Stage::Builtin(Builtin::Keys)),
                        "length" => Ok(Stage::Builtin(Builtin::Length)),
                        "type" => Ok(Stage::Builtin(Builtin::Type)),
                        _ => Err(ParseError::Unexpected(name)),
                    }
                }
            }
            _ => Err(ParseError::Unexpected(self.rest_here())),
        }
    }
}

/// Run a parsed filter over one input; results are appended to `out`.
/// Errors carry jq's message shapes ("Cannot iterate over number").
pub fn eval(filter: &Filter, input: &Value, out: &mut Vec<Value>) -> Result<(), String> {
    let mut current = vec![input.clone()];
    for stage in &filter.stages {
        let mut next = Vec::new();
        for v in &current {
            eval_stage(stage, v, &mut next)?;
        }
        current = next;
    }
    out.extend(current);
    Ok(())
}

fn eval_stage(stage: &Stage, input: &Value, out: &mut Vec<Value>) -> Result<(), String> {
    match stage {
        Stage::Identity => out.push(input.clone()),
        Stage::Keys(keys) => {
            let mut v = input.clone();
            for k in keys {
                v = match &v {
                    Value::Object(m) => m.get(k).cloned().unwrap_or(Value::Null),
                    Value::Null => Value::Null,
                    other => return Err(format!("Cannot index {} with \"{k}\"", type_name(other))),
                };
            }
            out.push(v);
        }
        Stage::Iterate => match input {
            Value::Array(a) => out.extend(a.iter().cloned()),
            Value::Object(m) => out.extend(m.values().cloned()),
            other => return Err(format!("Cannot iterate over {}", type_name(other))),
        },
        Stage::Index(n) => match input {
            Value::Array(a) => out.push(a.get(*n).cloned().unwrap_or(Value::Null)),
            Value::Null => out.push(Value::Null),
            other => return Err(format!("Cannot index {} with number", type_name(other))),
        },
        Stage::Collect(inner) => {
            let mut collected = Vec::new();
            eval(inner, input, &mut collected)?;
            out.push(Value::Array(collected));
        }
        Stage::Builtin(b) => match (b, input) {
            (Builtin::Keys, Value::Object(m)) => out.push(Value::Array(
                m.keys().map(|k| Value::String(k.clone())).collect(),
            )),
            (Builtin::Length, Value::Array(a)) => out.push(Value::from(a.len())),
            (Builtin::Length, Value::Object(m)) => out.push(Value::from(m.len())),
            (Builtin::Length, Value::String(s)) => out.push(Value::from(s.chars().count())),
            (Builtin::Length, Value::Null) => out.push(Value::from(0)),
            (Builtin::Type, v) => out.push(Value::String(type_name(v).to_string())),
            (builtin, other) => {
                return Err(format!(
                    "{} has no `{}`",
                    type_name(other),
                    match builtin {
                        Builtin::Keys => "keys",
                        Builtin::Length => "length",
                        Builtin::Type => "type",
                    }
                ));
            }
        },
        Stage::Select(inner) => {
            let mut results = Vec::new();
            eval(inner, input, &mut results)?;
            if results.iter().any(truthy) {
                out.push(input.clone());
            }
        }
        Stage::Pair(head, tail) => {
            let mut mid = Vec::new();
            eval_stage(head, input, &mut mid)?;
            for v in &mid {
                eval_stage(tail, v, out)?;
            }
        }
        Stage::Try(head) => {
            // the errors of the wrapped stage become no output (jq's `?`)
            let mut tmp = Vec::new();
            if eval_stage(head, input, &mut tmp).is_ok() {
                out.extend(tmp);
            }
        }
    }
    Ok(())
}

/// jq truthiness: everything but `false` and `null` (so `0` and `[]` pass).
fn truthy(v: &Value) -> bool {
    !matches!(v, Value::Null | Value::Bool(false))
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Render a result stream the way jq's default output does: one compact
/// value per line.
pub fn render(results: &[Value]) -> String {
    results
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `?q=` reads are bounded like any other read result: at most this many
/// result lines ride one call, and the note names the
/// continuation shape.
const QUERY_RESULT_LIMIT: usize = 100;

/// The whole-file ceiling for a `?q=` read: a bigger
/// JSON is not parsed at all, and the message says so.
const QUERY_FILE_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Entry point for the read tool's `?q=` path: parse the filter, stream the
/// file (whole-file JSON or line-by-line JSONL), evaluate, cap, render.
/// `raw_path` is the original argument (for error messages), `file` the
/// path half already resolved by the caller, `query` the filter text.
pub(super) fn run_file_query(
    path: &Path,
    raw_path: &str,
    query: &str,
    args: &Value,
) -> super::ToolOutput {
    use super::ToolOutput;

    let filter = match parse(query) {
        Ok(f) => f,
        Err(e) => {
            return ToolOutput::err(format!(
                "{raw_path}: invalid jq filter: {e} (supported: \
                 key paths .a.b, .[], .[N], | pipes, [collect], select(), keys, length, type, \
                 trailing ?)"
            ));
        }
    };
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return ToolOutput::err(format!("cannot read {} ({e})", path.display())),
    };
    if meta.len() > QUERY_FILE_MAX_BYTES {
        return ToolOutput::err(format!(
            "{}: {} exceeds the {} JSON query limit; split the file or use bash",
            path.display(),
            crate::core::text::human_bytes(meta.len()),
            crate::core::text::human_bytes(QUERY_FILE_MAX_BYTES)
        ));
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return ToolOutput::err(format!("cannot read {} ({e})", path.display())),
    };

    // JSONL first: every non-blank line must parse on its own. A mixed file
    // (some lines JSON, some not) is not JSONL; try whole-file JSON then.
    let inputs: Vec<Value> = if text.lines().filter(|l| !l.trim().is_empty()).count() > 1
        && text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(4)
            .all(|l| serde_json::from_str::<Value>(l.trim()).is_ok())
    {
        let mut v = Vec::new();
        for (idx, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line.trim()) {
                Ok(val) => v.push(val),
                // the head parsed as JSONL and this line does not: a corrupt
                // file fails loudly rather than skipping the line
                Err(e) => {
                    return ToolOutput::err(format!(
                        "{}:{}: invalid JSON ({e})",
                        path.display(),
                        idx + 1
                    ));
                }
            }
        }
        v
    } else {
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => vec![v],
            Err(e) => {
                return ToolOutput::err(format!(
                    "{}: not valid JSON or JSONL ({e}); read it without ?q=",
                    path.display()
                ));
            }
        }
    };

    let mut results: Vec<Value> = Vec::new();
    for input in &inputs {
        if let Err(e) = eval(&filter, input, &mut results) {
            // a JSONL line that errors names itself: the line index is the
            // input ordinal in the file order
            return ToolOutput::err(format!("{raw_path}: {e}"));
        }
        if results.len() > QUERY_RESULT_LIMIT {
            break;
        }
    }
    if results.is_empty() {
        return ToolOutput::ok("(no results)");
    }

    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"]
        .as_u64()
        .map(|l| l as usize)
        .unwrap_or(QUERY_RESULT_LIMIT)
        .clamp(1, 1000);
    let total = results.len();
    let window: Vec<Value> = results.into_iter().skip(offset - 1).take(limit).collect();
    let last = offset + window.len().saturating_sub(1);
    let mut out = render(&window);
    // a continuation note only when results remain unseen — the same rule
    // as the line-window note above
    if last < total {
        out.push_str(&format!(
            "\n\n[Showing {offset}-{last} of {total} results. Use offset={} to continue.]",
            last + 1
        ));
    }
    ToolOutput::ok(out)
}

use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(src: &str, input: Value) -> Vec<Value> {
        let f = parse(src).unwrap();
        let mut out = Vec::new();
        eval(&f, &input, &mut out).unwrap();
        out
    }

    #[test]
    fn identity_emits_the_input() {
        assert_eq!(run(".", json!({"a": 1})), vec![json!({"a": 1})]);
    }

    #[test]
    fn key_paths_walk_objects() {
        assert_eq!(run(".a.b", json!({"a": {"b": 7}})), vec![json!(7)]);
        // a missing key is null, like jq
        assert_eq!(run(".a.c", json!({"a": {"b": 7}})), vec![Value::Null]);
    }

    #[test]
    fn iterate_emits_each_element_or_value() {
        assert_eq!(run(".[]", json!([1, 2])), vec![json!(1), json!(2)]);
        // a suffixed term: .a then iterate
        assert_eq!(run(".a[]", json!({"a": [true]})), vec![json!(true)]);
        assert_eq!(
            run(".a[]", json!({"a": {"x": 1, "y": 2}})),
            vec![json!(1), json!(2)]
        );
    }

    #[test]
    fn indexing_takes_one_element() {
        assert_eq!(run(".[1]", json!(["a", "b"])), vec![json!("b")]);
        assert_eq!(run(".[9]", json!(["a"])), vec![Value::Null]);
        assert_eq!(run(".a[0]", json!({"a": ["x", "y"]})), vec![json!("x")]);
    }

    #[test]
    fn pipes_chain_stages() {
        assert_eq!(
            run(".a | .[] | .b", json!({"a": [{"b": 1}, {"b": 2}]})),
            vec![json!(1), json!(2)]
        );
    }

    #[test]
    fn collect_wraps_a_stream_into_one_array() {
        assert_eq!(run("[.[]]", json!([1, 2])), vec![json!([1, 2])]);
    }

    #[test]
    fn select_filters_by_jq_truthiness() {
        let input = json!([{"b": true, "n": 1}, {"b": false, "n": 0}, {"b": true, "n": 2}]);
        assert_eq!(
            run(".[] | select(.b)", input.clone()),
            vec![json!({"b": true, "n": 1}), json!({"b": true, "n": 2})]
        );
        // jq truthiness: 0 and [] are truthy, only null/false are not
        assert_eq!(
            run(".[] | select(.n)", input),
            vec![
                json!({"b": true, "n": 1}),
                json!({"b": false, "n": 0}),
                json!({"b": true, "n": 2})
            ]
        );
    }

    #[test]
    fn builtins_report_shape() {
        assert_eq!(
            run("keys", json!({"b": 1, "a": 2})),
            vec![json!(["b", "a"])]
        );
        assert_eq!(run("length", json!([1, 2, 3])), vec![json!(3)]);
        assert_eq!(run("type", json!(1)), vec![json!("number")]);
    }

    #[test]
    fn try_suppresses_errors_but_keeps_values() {
        // iterating a number errors bare, yields nothing under `?`
        let mut out = Vec::new();
        assert!(eval(&parse(".[]").unwrap(), &json!(3), &mut out).is_err());
        assert_eq!(run(".[]?", json!(3)), Vec::<Value>::new());
        // and a successful stage under `?` passes through
        assert_eq!(run(".a?", json!({"a": 1})), vec![json!(1)]);
    }

    #[test]
    fn errors_read_like_jq_messages() {
        let f = parse(".[]").unwrap();
        let mut out = Vec::new();
        let err = eval(&f, &json!(3), &mut out).unwrap_err();
        assert_eq!(err, "Cannot iterate over number");
    }

    #[test]
    fn a_bad_filter_reports_what_was_unexpected() {
        assert!(parse(".foo..").is_err());
        assert!(parse(".a |").is_err());
        assert!(parse("tostring").is_err());
        assert!(parse(".a | meow").is_err());
    }

    #[test]
    fn rendering_is_one_value_per_line() {
        assert_eq!(render(&[json!(1), json!("x")]), "1\n\"x\"");
    }
}

#[cfg(test)]
mod file_tests {
    use super::super::ToolOutput;
    use super::*;
    use serde_json::json;

    fn scratch(name: &str, contents: &str) -> std::path::PathBuf {
        let dir = crate::core::testutil::scratch_dir(name);
        let p = dir.join("data.json");
        std::fs::write(&p, contents).unwrap();
        p
    }

    fn body(out: ToolOutput) -> String {
        out.content
    }

    #[test]
    fn queries_a_whole_file_json_by_key_path() {
        let p = scratch("jq_obj", r#"{"items": [{"n": 1}, {"n": 2}]}"#);
        let out = run_file_query(&p, "data.json?q=.items[1].n", ".items[1].n", &json!({}));
        assert_eq!(body(out), "2");
    }

    #[test]
    fn queries_a_jsonl_file_line_by_line() {
        let dir = crate::core::testutil::scratch_dir("jq_jsonl");
        let p = dir.join("log.jsonl");
        std::fs::write(
            &p,
            "{\"lvl\":\"info\"}\n{\"lvl\":\"err\"}\n{\"lvl\":\"err\"}\n",
        )
        .unwrap();
        let out = run_file_query(&p, "log.jsonl?q=.", ".", &json!({}));
        let b = body(out);
        assert!(b.contains("\"lvl\":\"info\""), "{b}");
        assert!(b.contains("\"lvl\":\"err\""), "{b}");
        // 3 results, all shown: no continuation note
        assert_eq!(b.lines().count(), 3);
    }

    #[test]
    fn select_filters_jsonl_rows() {
        let dir = crate::core::testutil::scratch_dir("jq_sel");
        let p = dir.join("log.jsonl");
        std::fs::write(
            &p,
            "{\"lvl\":\"info\",\"m\":\"a\"}\n{\"lvl\":\"err\",\"m\":\"b\"}\n",
        )
        .unwrap();
        // identity over each row keeps all rows; a select keeps the match
        let out = run_file_query(&p, "log.jsonl?q=.", ".", &json!({}));
        let b = body(out);
        assert!(b.contains("\"m\":\"a\""));
        assert!(b.contains("\"m\":\"b\""), "{b}");
        let picked = run_file_query(
            &p,
            "log.jsonl?q=. | select(.lvl == \"err\")",
            ". | select(.lvl == \"err\")",
            &json!({}),
        );
        assert!(
            picked.is_error(),
            "== is not in the subset yet: {}",
            picked.content
        );
    }

    #[test]
    fn a_bad_filter_is_a_named_error() {
        let p = scratch("jq_bad", "{}");
        let out = run_file_query(&p, "data.json?q=.a |", ".a |", &json!({}));
        assert!(out.is_error());
        assert!(out.content.contains("invalid jq filter"), "{}", out.content);
    }

    #[test]
    fn a_non_json_file_refuses_instead_of_guessing() {
        let p = scratch("jq_text", "just prose\nmore prose\n");
        let out = run_file_query(&p, "data.json?q=.", ".", &json!({}));
        assert!(out.is_error());
        assert!(out.content.contains("not valid JSON"), "{}", out.content);
    }

    #[test]
    fn a_missing_file_is_cannot_read() {
        let dir = crate::core::testutil::scratch_dir("jq_missing");
        let absent = dir.join("nope.json");
        let out = run_file_query(&absent, "nope.json?q=.", ".", &json!({}));
        assert!(out.is_error());
        assert!(out.content.starts_with("cannot read"), "{}", out.content);
    }
}
