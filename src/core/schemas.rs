//! JSON schema resolution: the `--schema` input forms (inline JSON or a
//! file path).

use serde_json::Value;

/// Resolve a --schema input to a JSON schema object: inline JSON first,
/// then an existing file path.
pub fn resolve_schema(input: &str) -> Result<Value, String> {
    let trimmed = input.trim();
    if trimmed.starts_with('{')
        && let Ok(value) = serde_json::from_str::<Value>(trimmed)
    {
        return Ok(value);
    }
    // fall through on parse failure
    let path = std::path::Path::new(trimmed);
    if path.exists() {
        let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        return serde_json::from_str(&raw)
            .map_err(|_| "Schema file contained invalid JSON".to_string());
    }
    Err("Invalid schema (inline JSON or a file path)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_json_preferred() {
        let s = resolve_schema("{\"type\": \"object\"}").unwrap();
        assert_eq!(s["type"], "object");
    }
}
