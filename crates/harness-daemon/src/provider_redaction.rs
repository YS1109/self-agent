//! Bounded semantic handling for JSON carried inside text fields. This is not
//! arbitrary decoding: base64, URLs and other encodings are not interpreted.
use serde_json::Value;
const MAX_JSON_TEXT: usize = 1024 * 1024;
const MAX_JSON_LAYERS: usize = 4;
const WITHHELD: &str = "[嵌套 JSON 文本超过安全解析限额，已隐藏]";
fn looks_json(text: &str) -> bool {
    matches!(
        text.trim_start().as_bytes().first(),
        Some(b'{' | b'[' | b'"')
    )
}
pub(crate) struct SecretMatcher {
    patterns: std::sync::Arc<Vec<String>>,
    minimum_bytes: usize,
}
impl SecretMatcher {
    pub(crate) fn new(secrets: &[String]) -> Self {
        let patterns = super::provider_stream::Redactor::compile_patterns(secrets.to_vec());
        let minimum_bytes = patterns.iter().map(String::len).min().unwrap_or(usize::MAX);
        Self {
            patterns,
            minimum_bytes,
        }
    }
    fn plain(&self, input: &str) -> String {
        if input.len() < self.minimum_bytes
            || !self
                .patterns
                .iter()
                .any(|secret| input.contains(secret.as_str()))
        {
            return input.into();
        }
        let mut redactor = super::provider_stream::Redactor::with_patterns(self.patterns.clone());
        redactor.push(input) + &redactor.finish()
    }
    fn text(&self, input: &str, depth: usize) -> String {
        if !self.patterns.is_empty() && looks_json(input) {
            if input.len() > MAX_JSON_TEXT || depth >= MAX_JSON_LAYERS {
                return WITHHELD.into();
            }
            if let Ok(original) = serde_json::from_str::<Value>(input) {
                let safe = self.value(&original, depth + 1);
                if safe != original {
                    return safe.to_string();
                }
            }
        }
        self.plain(input)
    }
    fn value(&self, input: &Value, depth: usize) -> Value {
        match input {
            Value::String(input) => Value::String(self.text(input, depth)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.value(item, depth)).collect())
            }
            Value::Object(items) => Value::Object(
                items
                    .iter()
                    .map(|(key, item)| (self.text(key, depth), self.value(item, depth)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    pub(crate) fn contains_text(&self, input: &str, depth: usize) -> bool {
        if input.len() >= self.minimum_bytes
            && self
                .patterns
                .iter()
                .any(|secret| input.contains(secret.as_str()))
        {
            return true;
        }
        if !self.patterns.is_empty() && looks_json(input) {
            if input.len() > MAX_JSON_TEXT || depth >= MAX_JSON_LAYERS {
                return true;
            }
            if let Ok(value) = serde_json::from_str::<Value>(input) {
                return self.contains(&value, depth + 1);
            }
        }
        false
    }
    fn contains(&self, input: &Value, depth: usize) -> bool {
        match input {
            Value::String(input) => self.contains_text(input, depth),
            Value::Array(items) => items.iter().any(|item| self.contains(item, depth)),
            Value::Object(items) => items
                .iter()
                .any(|(key, item)| self.contains_text(key, depth) || self.contains(item, depth)),
            _ => false,
        }
    }
}
pub(super) fn redact(input: &Value, secrets: &[String]) -> Value {
    SecretMatcher::new(secrets).value(input, 0)
}
pub(super) fn has_secret(input: &Value, secrets: &[String]) -> bool {
    SecretMatcher::new(secrets).contains(input, 0)
}
pub(super) fn redact_text(input: &str, secrets: &[String]) -> String {
    SecretMatcher::new(secrets).text(input, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn semantic_json_text_keys_values_and_nested_strings_do_not_reveal_known_secret() {
        let key = "fixture-\"私有\\key\n尾";
        let secrets = vec![key.into()];
        let encoded = json!({key:key,"nested":json!({"token":key}).to_string()}).to_string();
        assert!(has_secret(&json!({"text":encoded}), &secrets));
        let safe = redact_text(&encoded, &secrets);
        let parsed: Value = serde_json::from_str(&safe).unwrap();
        assert!(!has_secret(&parsed, &secrets));
        assert!(!parsed.as_object().unwrap().contains_key(key));
        let nested: Value = serde_json::from_str(parsed["nested"].as_str().unwrap()).unwrap();
        assert_ne!(nested["token"], key);
        // Unicode escapes are interpreted semantically, not assumed to match
        // one serializer's choice of ASCII/UTF-8 or hex case.
        let escaped = "{\"token\":\"fixture-\\\"\\u79c1\\u6709\\\\key\\n\\u5c3e\"}";
        assert!(has_secret(&json!(escaped), &secrets));
        assert!(!has_secret(
            &json!(redact_text(escaped, &secrets)),
            &secrets
        ));
    }
    #[test]
    fn many_short_fields_compile_the_secret_patterns_only_once() {
        let secrets: Vec<String> = (0..64)
            .map(|index| format!("key-{index:02}-{}", "x".repeat(2041)))
            .collect();
        let data = json!(vec!["x"; 200_000]);
        let encoded = data.to_string();
        assert!(encoded.len() < MAX_JSON_TEXT);
        let started = std::time::Instant::now();
        assert!(!has_secret(&json!(encoded), &secrets));
        assert_eq!(redact_text(&encoded, &secrets), encoded);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "bounded JSON short-field processing took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn json_text_parse_budget_withholds_instead_of_forwarding_unknown_layers() {
        let secrets = vec!["secret".into()];
        let mut value = json!({"token":"secret"});
        for _ in 0..8 {
            value = json!({"wrapped":value.to_string()});
        }
        let safe = redact(&value, &secrets);
        assert!(safe.to_string().contains("安全解析限额"));
        assert_eq!(redact_text("ordinary text", &secrets), "ordinary text");
        assert_eq!(redact_text("{not json}", &secrets), "{not json}");
    }
}
