//! JSON schema resolution: the `--schema` input forms (inline JSON, the
//! compact DSL, or a file path) and the DSL parser, mirroring utils.py's
//! resolve_schema_input / schema_dsl.

use serde_json::{Value, json};

/// Resolve a --schema / --schema-multi input to a JSON schema object.
pub fn resolve_schema(input: &str) -> Result<Value, String> {
    let trimmed = input.trim();
    // inline JSON
    if trimmed.starts_with('{')
        && let Ok(value) = serde_json::from_str::<Value>(trimmed)
    {
        return Ok(value);
    }
    // fall through on parse failure
    // DSL: anything with a space or comma
    if trimmed.contains(' ') || trimmed.contains(',') {
        return schema_dsl(trimmed);
    }
    // existing file path
    let path = std::path::Path::new(trimmed);
    if path.exists() {
        let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        return serde_json::from_str(&raw)
            .map_err(|_| "Schema file contained invalid JSON".to_string());
    }
    Err("Invalid schema".to_string())
}

/// Wrap a schema so the model returns {"items": [ ...schema... ]}.
pub fn multi_schema(schema: &Value) -> Value {
    json!({
        "type": "object",
        "properties": {"items": {"type": "array", "items": schema}},
        "required": ["items"],
    })
}

/// Compact DSL: comma/newline-separated `name [int|float|bool|str] [: description]`,
/// every field required.
pub fn schema_dsl(spec: &str) -> Result<Value, String> {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<String> = Vec::new();
    for piece in spec.split([',', '\n']) {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let (field, description) = match piece.split_once(':') {
            Some((f, d)) => (f.trim(), Some(d.trim().to_string())),
            None => (piece, None),
        };
        let tokens: Vec<&str> = field.split_whitespace().collect();
        let (name, type_) = match tokens.split_last() {
            Some((last, head)) if ["int", "float", "bool", "str"].contains(last) => (
                head.join(" "),
                match *last {
                    "int" => "integer",
                    "float" => "number",
                    "bool" => "boolean",
                    _ => "string",
                },
            ),
            _ => (field.to_string(), "string"),
        };
        if name.is_empty() {
            return Err(format!("Invalid schema DSL item: {piece}"));
        }
        let mut prop = json!({"type": type_});
        if let Some(description) = description {
            prop["description"] = json!(description);
        }
        properties.insert(name.clone(), prop);
        required.push(name);
    }
    if properties.is_empty() {
        return Err("Invalid schema DSL: no fields".to_string());
    }
    Ok(json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsl_basic() {
        let s = schema_dsl("name, age int: years old").unwrap();
        assert_eq!(s["properties"]["name"]["type"], "string");
        assert_eq!(s["properties"]["age"]["type"], "integer");
        assert_eq!(s["properties"]["age"]["description"], "years old");
        assert_eq!(s["required"], json!(["name", "age"]));
    }

    #[test]
    fn dsl_multiword_names() {
        let s = schema_dsl("first name, is active bool").unwrap();
        assert_eq!(s["properties"]["first name"]["type"], "string");
        assert_eq!(s["properties"]["is active"]["type"], "boolean");
    }

    #[test]
    fn multi_wrapper() {
        let inner = json!({"type": "object", "properties": {}, "required": []});
        let wrapped = multi_schema(&inner);
        assert_eq!(wrapped["properties"]["items"]["items"], inner);
        assert_eq!(wrapped["required"], json!(["items"]));
    }

    #[test]
    fn inline_json_preferred() {
        let s = resolve_schema("{\"type\": \"object\"}").unwrap();
        assert_eq!(s["type"], "object");
    }
}
