//! Narrow vendor compatibility rules for outgoing tool schemas. Walk only
//! schema-bearing keywords: examples, defaults and property names are data.

use serde_json::{Value, json};

use super::ResolvedModel;

pub(super) fn normalize_tools(model: &ResolvedModel, body: &mut Value) {
    let deepseek = super::is_host(&model.base_url, "api.deepseek.com");
    let kimi = [
        "api.moonshot.ai",
        "api.moonshot.cn",
        "api.kimi.com",
        "api.kimi.ai",
    ]
    .iter()
    .any(|host| super::is_host(&model.base_url, host));
    if !deepseek && !kimi {
        return;
    }
    if let Some(tools) = body["tools"].as_array_mut() {
        for tool in tools {
            for pointer in ["/function/parameters", "/parameters", "/input_schema"] {
                if let Some(schema) = tool.pointer_mut(pointer) {
                    walk(schema, deepseek, kimi);
                }
            }
        }
    }
}

fn walk(schema: &mut Value, deepseek: bool, kimi: bool) {
    let Some(node) = schema.as_object_mut() else {
        return;
    };
    if deepseek && let Some(Value::String(pattern)) = node.get_mut("pattern") {
        *pattern = unicode_null_escape(pattern);
    }
    if kimi && !node.contains_key("type") {
        let values = node
            .get("enum")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .or_else(|| node.get("const").map(std::slice::from_ref));
        if let Some(typ) = values.and_then(enum_type) {
            node.insert("type".into(), typ);
        }
    }
    for (key, child) in node {
        match key.as_str() {
            "properties" | "$defs" | "definitions" | "dependentSchemas" | "patternProperties" => {
                if let Some(map) = child.as_object_mut() {
                    for schema in map.values_mut() {
                        walk(schema, deepseek, kimi);
                    }
                }
            }
            "items"
            | "additionalProperties"
            | "contains"
            | "propertyNames"
            | "not"
            | "if"
            | "then"
            | "else"
            | "allOf"
            | "anyOf"
            | "oneOf"
            | "prefixItems"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "contentSchema" => {
                if let Some(schemas) = child.as_array_mut() {
                    for schema in schemas {
                        walk(schema, deepseek, kimi);
                    }
                } else {
                    walk(child, deepseek, kimi);
                }
            }
            _ => {}
        }
    }
}

fn enum_type(values: &[Value]) -> Option<Value> {
    let mut typ = None;
    let mut nullable = false;
    for value in values {
        let current = match value {
            Value::Null => {
                nullable = true;
                continue;
            }
            Value::String(_) => "string",
            Value::Bool(_) => "boolean",
            Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
            Value::Number(_) => "number",
            _ => return None,
        };
        typ = Some(match typ {
            None => current,
            Some(previous) if previous == current => current,
            Some("integer" | "number") if matches!(current, "integer" | "number") => "number",
            _ => return None,
        });
    }
    let typ = typ?;
    Some(if nullable {
        json!([typ, "null"])
    } else {
        json!(typ)
    })
}

fn unicode_null_escape(pattern: &str) -> String {
    let mut chars = pattern.chars().peekable();
    let mut out = String::with_capacity(pattern.len());
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(next) = chars.next()
        {
            if next == '0' && !chars.peek().is_some_and(char::is_ascii_digit) {
                out.push_str("\\u0000");
            } else {
                out.push(ch);
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_enum_types_preserve_mixed_and_explicit_types() {
        let mut schema = json!({"type":"object", "properties": {
            "kind": {"enum":["text", "icon", null]},
            "count": {"enum":[1, 2]},
            "number": {"enum":[1, 2.5]},
            "enabled": {"const":true},
            "mixed": {"enum":[1, "one"]},
            "explicit": {"type":"number", "enum":[1, 2]},
            "null": {"const":null}
        }});
        walk(&mut schema, false, true);
        let p = &schema["properties"];
        assert_eq!(p["kind"]["type"], json!(["string", "null"]));
        assert_eq!(p["count"]["type"], "integer");
        assert_eq!(p["number"]["type"], "number");
        assert_eq!(p["enabled"]["type"], "boolean");
        assert_eq!(p["explicit"]["type"], "number");
        assert!(p["mixed"].get("type").is_none());
        assert!(p["null"].get("type").is_none());
    }

    #[test]
    fn schema_walk_visits_nested_schemas_without_rewriting_data() {
        let mut schema = json!({"properties": {
            "enum": {"items":{"anyOf":[{"const":"中文"}]}},
            "pattern": {"default":{"enum":["data"]}}
        }, "$defs":{"x":{"const":false}}, "default":{"pattern":"\\0"}});
        walk(&mut schema, true, true);
        assert_eq!(
            schema["properties"]["enum"]["items"]["anyOf"][0]["type"],
            "string"
        );
        assert_eq!(schema["$defs"]["x"]["type"], "boolean");
        assert_eq!(schema["default"]["pattern"], "\\0");
        assert!(
            schema["properties"]["pattern"]["default"]
                .get("type")
                .is_none()
        );
    }

    #[test]
    fn deepseek_patterns_preserve_literal_backslashes_and_octal_escapes() {
        assert_eq!(
            unicode_null_escape(r"中文\0|\\0|\012|\09|tail\"),
            r"中文\u0000|\\0|\012|\09|tail\"
        );
    }

    #[test]
    fn normalization_is_vendor_scoped_and_does_not_mutate_tools() {
        let mut model = super::super::testutil::model("openai-compat");
        let mut tools = vec![super::super::testutil::tool_def()];
        tools[0].parameters = json!({"type":"object", "properties":{"x":{"enum":["a"]}}});
        let original = tools[0].parameters.clone();
        let input = super::super::testutil::input(&[], &tools);
        model.base_url = "https://api.moonshot.ai/v1".into();
        let body = super::super::openai_compat::build_body(&model, &input, true).unwrap();
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["x"]["type"],
            "string"
        );
        model.base_url = "https://api.moonshot.ai.evil.test/v1".into();
        let body = super::super::openai_compat::build_body(&model, &input, true).unwrap();
        assert!(
            body["tools"][0]["function"]["parameters"]["properties"]["x"]
                .get("type")
                .is_none()
        );
        assert_eq!(tools[0].parameters, original);
    }

    #[test]
    fn schema_compatibility_applies_to_each_wire_shape_after_options() {
        let mut model = super::super::testutil::model("anthropic");
        model.base_url = "https://api.deepseek.com".into();
        let mut tools = vec![super::super::testutil::tool_def()];
        tools[0].parameters =
            json!({"type":"object", "properties":{"x":{"type":"string", "pattern":r"\0"}}});
        let input = super::super::testutil::input(&[], &tools);
        let body = super::super::anthropic::build_body(&model, &input, true).unwrap();
        assert_eq!(
            body["tools"][0]["input_schema"]["properties"]["x"]["pattern"],
            r"\u0000"
        );
        let body = super::super::responses::build_body(&model, &input, true).unwrap();
        assert_eq!(
            body["tools"][0]["parameters"]["properties"]["x"]["pattern"],
            r"\u0000"
        );
        model.options = vec![(
            "tools".into(),
            json!([{"function":{"parameters":{"pattern":r"\0"}}}]).to_string(),
        )];
        let body = super::super::openai_compat::build_body(&model, &input, true).unwrap();
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["pattern"],
            r"\u0000"
        );
    }
}
