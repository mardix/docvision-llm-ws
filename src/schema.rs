//! The JSON Schema subset used by structured extraction (`extract_schema`): `type` (one or a
//! list), `properties`, `required`, `items` and `enum`. Other keywords (`description`,
//! `format`, …) are passed to the model as guidance but not checked here.

use serde_json::Value;

/// Largest accepted schema, serialized.
pub const MAX_SCHEMA_BYTES: usize = 16 * 1024;
const MAX_DEPTH: usize = 32;

/// A schema is accepted when it is an object schema of reasonable size and depth.
pub fn check_schema(schema: &Value) -> Result<(), String> {
    if schema.to_string().len() > MAX_SCHEMA_BYTES {
        return Err(format!("extract_schema must be at most {} KiB", MAX_SCHEMA_BYTES / 1024));
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("extract_schema must be a JSON Schema with \"type\": \"object\" at the top".into());
    }
    fn depth(v: &Value) -> usize {
        match v {
            Value::Object(m) => 1 + m.values().map(depth).max().unwrap_or(0),
            Value::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
            _ => 0,
        }
    }
    if depth(schema) > MAX_DEPTH {
        return Err("extract_schema is nested too deeply".into());
    }
    Ok(())
}

fn type_matches(v: &Value, t: &str) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => true,
    }
}

/// Check `v` against `schema`; the error names the first mismatching path (e.g. `$.items[2].price`).
pub fn validate(v: &Value, schema: &Value) -> Result<(), String> {
    validate_at(v, schema, "$")
}

fn validate_at(v: &Value, schema: &Value, path: &str) -> Result<(), String> {
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !types.is_empty() && !types.iter().any(|t| type_matches(v, t)) {
        return Err(format!("{path} should be {}", types.join(" or ")));
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(v)
    {
        return Err(format!("{path} is not one of the allowed values"));
    }
    if let Value::Object(obj) = v {
        for key in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !obj.contains_key(key) {
                return Err(format!("{path}.{key} is required"));
            }
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (key, sub) in props {
                if let Some(child) = obj.get(key) {
                    validate_at(child, sub, &format!("{path}.{key}"))?;
                }
            }
        }
    }
    if let (Value::Array(items), Some(sub)) = (v, schema.get("items")) {
        for (i, item) in items.iter().enumerate() {
            validate_at(item, sub, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_subset() {
        let s = json!({
            "type": "object",
            "required": ["total"],
            "properties": {
                "total": {"type": "number"},
                "date": {"type": ["string", "null"]},
                "currency": {"enum": ["USD", "EUR"]},
                "items": {"type": "array", "items": {"type": "object", "properties": {"qty": {"type": "integer"}}}}
            }
        });
        assert!(check_schema(&s).is_ok());
        assert!(validate(&json!({"total": 4.5, "date": null, "currency": "USD", "items": [{"qty": 2}]}), &s).is_ok());
        assert_eq!(validate(&json!({"date": "x"}), &s).unwrap_err(), "$.total is required");
        assert_eq!(validate(&json!({"total": "4.5"}), &s).unwrap_err(), "$.total should be number");
        assert_eq!(
            validate(&json!({"total": 1, "items": [{"qty": 1}, {"qty": 1.5}]}), &s).unwrap_err(),
            "$.items[1].qty should be integer"
        );
        assert!(validate(&json!({"total": 1, "currency": "GBP"}), &s).is_err());
        assert!(check_schema(&json!({"type": "array"})).is_err());
        assert!(check_schema(&json!({"type": "object", "description": "x".repeat(20_000)})).is_err());
    }
}
