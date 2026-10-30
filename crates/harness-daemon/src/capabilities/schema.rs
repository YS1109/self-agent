//! Bounded, deliberately small JSON Schema subset for model-visible MCP tools.
//! Unsupported validation keywords fail closed rather than being ignored.
use anyhow::{bail, Result};
use serde_json::Value;

const MAX_DEPTH: usize = 16;
const MAX_NODES: usize = 512;

pub(crate) fn check_schema(schema: &Value) -> Result<()> {
    if serde_json::to_vec(schema)?.len() > 32 * 1024 {
        bail!("MCP 参数结构超过限制");
    }
    let mut nodes = 0;
    inspect(schema, 0, &mut nodes)?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        bail!("MCP 顶层参数必须声明为对象");
    }
    Ok(())
}

fn inspect(schema: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    *nodes += 1;
    if depth > MAX_DEPTH || *nodes > MAX_NODES {
        bail!("MCP 参数结构复杂度超过限制");
    }
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("MCP 参数结构必须为对象"))?;
    for (key, value) in object {
        match key.as_str() {
            "$schema" => {
                if !matches!(
                    value.as_str(),
                    Some(
                        "https://json-schema.org/draft/2020-12/schema"
                            | "http://json-schema.org/draft-07/schema#"
                    )
                ) {
                    bail!("MCP 参数结构版本不受支持");
                }
            }
            "type" => {
                if !matches!(
                    value.as_str(),
                    Some("object" | "array" | "string" | "number" | "integer" | "boolean" | "null")
                ) {
                    bail!("MCP 参数类型不受支持");
                }
            }
            "properties" => {
                let properties = value
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("MCP 属性结构无效"))?;
                for (name, child) in properties {
                    if name.is_empty() || name.len() > 128 {
                        bail!("MCP 属性名称超过限制");
                    }
                    inspect(child, depth + 1, nodes)?;
                }
            }
            "items" => inspect(value, depth + 1, nodes)?,
            "additionalProperties" => {
                if !value.is_boolean() {
                    inspect(value, depth + 1, nodes)?;
                }
            }
            "required" => {
                let required = value
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("MCP 必填参数结构无效"))?;
                if required.len() > 128
                    || required
                        .iter()
                        .any(|entry| entry.as_str().is_none_or(|s| s.is_empty() || s.len() > 128))
                {
                    bail!("MCP 必填参数超过限制");
                }
            }
            "enum" => {
                if value
                    .as_array()
                    .is_none_or(|items| items.is_empty() || items.len() > 128)
                {
                    bail!("MCP 枚举结构无效");
                }
            }
            "const" | "default" | "examples" => {}
            "description" | "title" => {
                if !value.is_string() {
                    bail!("MCP 参数说明格式无效");
                }
            }
            "minimum" | "maximum" => {
                if !safe_number(value) {
                    bail!("MCP 数值边界必须在精确安全范围内");
                }
            }
            "minLength" | "maxLength" | "minItems" | "maxItems" => {
                if value.as_u64().is_none_or(|n| n > 65536) {
                    bail!("MCP 长度边界无效");
                }
            }
            _ => bail!("MCP 参数结构包含尚未支持的校验关键字"),
        }
    }
    Ok(())
}

pub(crate) fn validate(schema: &Value, value: &Value) -> Result<()> {
    check_schema(schema)?;
    if serde_json::to_vec(value)?.len() > 64 * 1024 {
        bail!("MCP 参数超过 64 KiB 限制");
    }
    let mut nodes = 0;
    validate_inner(schema, value, 0, &mut nodes)
}

fn validate_inner(schema: &Value, value: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    *nodes += 1;
    if depth > MAX_DEPTH || *nodes > 4096 {
        bail!("MCP 参数复杂度超过限制");
    }
    let valid_type = match schema["type"].as_str() {
        None => true,
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("number") => value.is_number(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("boolean") => value.is_boolean(),
        Some("null") => value.is_null(),
        _ => false,
    };
    if !valid_type {
        bail!("MCP 参数类型不符合声明");
    }
    if schema
        .get("const")
        .is_some_and(|expected| expected != value)
        || schema["enum"]
            .as_array()
            .is_some_and(|entries| !entries.contains(value))
    {
        bail!("MCP 参数不符合枚举限制");
    }
    if value.is_number() && !safe_number(value) {
        bail!("MCP 数值参数超出精确安全范围");
    }
    if let Some(number) = value.as_f64() {
        if schema["minimum"].as_f64().is_some_and(|min| number < min)
            || schema["maximum"].as_f64().is_some_and(|max| number > max)
        {
            bail!("MCP 参数超出数值边界");
        }
    }
    if let Some(text) = value.as_str() {
        check_length(schema, text.chars().count(), "minLength", "maxLength")?;
    }
    if let Some(items) = value.as_array() {
        check_length(schema, items.len(), "minItems", "maxItems")?;
        if let Some(child) = schema.get("items") {
            for item in items {
                validate_inner(child, item, depth + 1, nodes)?;
            }
        }
    }
    if let Some(object) = value.as_object() {
        for required in schema["required"].as_array().into_iter().flatten() {
            if !object.contains_key(required.as_str().unwrap_or_default()) {
                bail!("MCP 缺少必填参数");
            }
        }
        for (name, child) in object {
            if let Some(property) = schema["properties"].get(name) {
                validate_inner(property, child, depth + 1, nodes)?;
            } else if schema["additionalProperties"] == false {
                bail!("MCP 存在未声明参数");
            } else if schema["additionalProperties"].is_object() {
                validate_inner(&schema["additionalProperties"], child, depth + 1, nodes)?;
            }
        }
    }
    Ok(())
}
fn safe_number(value: &Value) -> bool {
    const LIMIT: i64 = 9_007_199_254_740_991;
    if let Some(n) = value.as_i64() {
        return (-LIMIT..=LIMIT).contains(&n);
    }
    if let Some(n) = value.as_u64() {
        return n <= LIMIT as u64;
    }
    value
        .as_f64()
        .is_some_and(|n| n.is_finite() && n.abs() <= LIMIT as f64)
}
fn check_length(schema: &Value, len: usize, minimum: &str, maximum: &str) -> Result<()> {
    if schema[minimum]
        .as_u64()
        .is_some_and(|min| len < min as usize)
        || schema[maximum]
            .as_u64()
            .is_some_and(|max| len > max as usize)
    {
        bail!("MCP 参数长度超出边界");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn bounded_subset_is_checked_before_arguments_are_sent() {
        let schema = json!({"type":"object","properties":{"text":{"type":"string","maxLength":3},"count":{"type":"integer","minimum":1}},"required":["text"],"additionalProperties":false});
        validate(&schema, &json!({"text":"你好","count":2})).unwrap();
        assert!(validate(
            &schema,
            &json!({"text":"ok","count":9_007_199_254_740_993i64})
        )
        .is_err());
        assert!(check_schema(&json!({"type":"object","properties":{"n":{"type":"integer","maximum":9_007_199_254_740_992i64}}})).is_err());
        for args in [
            json!({}),
            json!({"text":"four"}),
            json!({"text":"ok","other":1}),
            json!({"text":"ok","count":0}),
        ] {
            assert!(validate(&schema, &args).is_err());
        }
        for schema in [
            json!({"type":"object","$ref":"https://example.com/schema"}),
            json!({"type":"object","patternProperties":{}}),
            json!({"type":"object","properties":{"s":{"type":"string","pattern":".*"}}}),
        ] {
            assert!(check_schema(&schema).is_err());
        }
    }
}
