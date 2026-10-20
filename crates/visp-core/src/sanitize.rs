/// 脱敏和截断工具函数，用于打印安全的 LLM generation input/output capture。
///
/// 脱敏敏感 JSON 字段
///
/// 将已知敏感字段（api_key, authorization, secret, token, password 等）的值
/// 替换为 `[REDACTED]`。同时替换明文 Bearer/sk-/pk- 等常见 secret 模式。
pub fn sanitize_json(value: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(value) {
        Ok(mut v) => {
            redact_value(&mut v);
            // Also apply pattern-based redaction on all string values
            redact_string_values(&mut v);
            serde_json::to_string(&v).unwrap_or_else(|_| simple_redact(value))
        }
        Err(_) => simple_redact(value),
    }
}

/// 对所有 JSON 字符串值应用 simple_redact（捕获 sk-/pk-/Bearer 等模式）
fn redact_string_values(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => {
            let redacted = simple_redact(s);
            if redacted != *s {
                *s = redacted;
            }
        }
        serde_json::Value::Object(map) => {
            for val in map.values_mut() {
                redact_string_values(val);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                redact_string_values(item);
            }
        }
        _ => {}
    }
}

/// JSON 值递归脱敏
fn redact_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if is_sensitive_key(&key)
                    && let Some(val) = map.get_mut(&key)
                    && val.is_string()
                {
                    *val = serde_json::Value::String("[REDACTED]".to_string());
                    continue;
                }
                if let Some(val) = map.get_mut(&key) {
                    redact_value(val);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                redact_value(item);
            }
        }
        _ => {}
    }
}

const SENSITIVE_KEYS: &[&str] = &[
    "api_key",
    "api-key",
    "authorization",
    "secret",
    "token",
    "password",
    "passwd",
    "credential",
    "x-api-key",
    "x-auth-token",
];

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_lowercase();
    SENSITIVE_KEYS
        .iter()
        .any(|k| lower == *k || lower.ends_with(k))
}

/// 简单字符串脱敏：替换 Bearer/sk-/pk- 等模式
fn simple_redact(s: &str) -> String {
    let mut result = s.to_string();

    // Bearer token: "Bearer xxxxx"  where xxxxx ≥ 6 chars
    result = replace_bearer_token(&result);

    // sk- / pk- / sv- prefixed keys (common for API keys), length ≥ 8
    result = replace_prefixed_secret(&result, "sk-");
    result = replace_prefixed_secret(&result, "pk-");
    result = replace_prefixed_secret(&result, "sv-");

    result
}

/// 替换 `Bearer <token>` 模式（token ≥ 6 字符）
fn replace_bearer_token(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.to_lowercase().find("bearer ") {
        // Keep everything before "bearer "
        result.push_str(&rest[..pos + 7]); // "bearer " = 7 chars
        rest = &rest[pos + 7..];

        // Find the end of the token (whitespace, quote, comma, closing paren/bracket/brace, or end)
        let token_len = rest
            .chars()
            .take_while(|&c| {
                !c.is_whitespace() && c != '"' && c != ',' && c != ')' && c != ']' && c != '}'
            })
            .count();
        if token_len >= 6 {
            result.push_str("[REDACTED]");
            rest = &rest[token_len..];
        } else {
            // Short token — might not be a real secret, keep as-is
            // (we already included the prefix, so just continue)
        }
    }
    result.push_str(rest);
    result
}

/// 替换 `prefix<token>` 模式（token ≥ 8 字符，且后跟非字母数字或结尾）
fn replace_prefixed_secret(s: &str, prefix: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(prefix) {
        result.push_str(&rest[..pos]);
        rest = &rest[pos + prefix.len()..];

        // Check that prefix is at a word boundary (preceded by non-alphanumeric or start)
        if pos > 0 {
            let prev = result.chars().last().unwrap();
            if prev.is_alphanumeric() || prev == '_' || prev == '-' {
                // Not a real prefix boundary, keep the prefix
                result.push_str(prefix);
                continue;
            }
        }

        // Count token chars (alphanumeric, underscore, hyphen, dot)
        let token_len = rest
            .chars()
            .take_while(|&c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
            .count();
        if token_len >= 8 {
            result.push_str("[REDACTED]");
            rest = &rest[token_len..];
        } else {
            result.push_str(prefix);
        }
    }
    result.push_str(rest);
    result
}

/// UTF-8 安全截断。
///
/// 最多保留 `max_chars` 个字符。如果被截断，追加 `…[truncated]` 标记。
pub fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{}…[truncated]", truncated)
}

/// 组合：先脱敏，再截断。
pub fn sanitize_and_truncate(s: &str, max_chars: usize, redact: bool) -> String {
    let cleaned = if redact {
        sanitize_json(s)
    } else {
        s.to_string()
    };
    truncate(&cleaned, max_chars)
}

/// 构建 Langfuse observation span 的 input 字段值。
///
/// 输入是完整的请求 body JSON。输出经过 sanitize 处理。
pub fn format_langfuse_input(body: &serde_json::Value, max_chars: usize, redact: bool) -> String {
    let json_str = serde_json::to_string(body).unwrap_or_default();
    sanitize_and_truncate(&json_str, max_chars, redact)
}

/// 构建 Langfuse observation span 的 output 字段值。
///
/// 输入是最终 assistant 文本。输出经过 sanitize 处理。
pub fn format_langfuse_output(text: &str, max_chars: usize, redact: bool) -> String {
    sanitize_and_truncate(text, max_chars, redact)
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
