//! Shared span initialization: `init_span!` macro and Langfuse field recording.
//!
//! These helpers are reused across crates (e.g. `visp-core` agent loop and
//! `visp-agent` orchestrator) to create agent-loop spans with a consistent
//! field set and to propagate Langfuse trace-level attributes.

use std::collections::HashMap;

/// Langfuse-related configuration snapshot used by [`record_langfuse_trace_fields`].
///
/// Mirrors the Langfuse subset of the agent configuration so that span
/// initialization can be shared without depending on the full config type.
#[derive(Debug, Clone, Default)]
pub struct LangfuseSpanConfig {
    /// Langfuse 总开关
    pub langfuse_enabled: bool,
    /// Langfuse user.id 字段值（None = 不设置）
    pub langfuse_user_id: Option<String>,
    /// Langfuse tags 字段值（JSON 字符串，None = 不设置）
    pub langfuse_tags: Option<String>,
    /// Langfuse environment 字段
    pub langfuse_environment: Option<String>,
    /// Langfuse release 字段
    pub langfuse_release: Option<String>,
    /// Langfuse version 字段
    pub langfuse_version: Option<String>,
    /// Langfuse public 开关（None = 不设置）
    pub langfuse_public: Option<bool>,
    /// Langfuse metadata（值均为字符串：标量保留，数组/table 转紧凑 JSON）
    pub langfuse_metadata: Option<HashMap<String, String>>,
}

/// 统一的 agent-loop span 初始化宏（tool / run / iteration 三类 span 共用同一展开）。
///
/// - 行为：创建 span，`tracing` 字段语法的立即字段原样透传给 `info_span!`；`langfuse_enabled` 守卫内记录 `langfuse.observation.type="span"` 并调用 `record_langfuse_trace_fields` 进行 langfuse 传播。
/// - 固定字段集（三类并集）：公共具体值 `session_id`/`session.id`（取自 `$sid`）；延后 record 的 Empty 槽 `trace_id`/`parent_span_id`（ParentLinkLayer on_enter）、全部 `langfuse.*`、`level`/`status_message`/`visp.tool.*`。
/// - 约束：调用方立即字段不得与上述集合重名（重名 = 同一 callsite 重复声明）；对未声明字段调用 `record()` 会被静默丢弃。`$cfg` 必须为 `&LangfuseSpanConfig`。
#[macro_export]
macro_rules! init_span {
    ($cfg:expr, $sid:expr, $span_name:literal, $($field:tt)*) => {{
        let __span = tracing::info_span!(
            $span_name,
            session_id = $sid,
            session.id = $sid,
            langfuse.session.id = tracing::field::Empty,
            trace_id = tracing::field::Empty,
            parent_span_id = tracing::field::Empty,
            langfuse.observation.type = tracing::field::Empty,
            langfuse.observation.input = tracing::field::Empty,
            langfuse.observation.output = tracing::field::Empty,
            langfuse.user.id = tracing::field::Empty,
            langfuse.trace.tags = tracing::field::Empty,
            langfuse.trace.name = tracing::field::Empty,
            langfuse.environment = tracing::field::Empty,
            langfuse.trace.public = tracing::field::Empty,
            langfuse.release = tracing::field::Empty,
            langfuse.version = tracing::field::Empty,
            langfuse.trace.metadata = tracing::field::Empty,
            level = tracing::field::Empty,
            status_message = tracing::field::Empty,
            visp.tool.is_error = tracing::field::Empty,
            visp.tool.duration_ms = tracing::field::Empty,
            visp.tool.arguments = tracing::field::Empty,
            $($field)*
        );
        if $cfg.langfuse_enabled {
            __span.record("langfuse.observation.type", "span");
        }
        $crate::span_init::record_langfuse_trace_fields(&__span, $cfg, $sid);
        __span
    }};
}

/// Record langfuse trace-level fields onto a span.
/// The span must have these fields declared in its `info_span!` macro invocation.
/// Safe to call even when langfuse is disabled (returns immediately).
pub fn record_langfuse_trace_fields(
    span: &tracing::Span,
    cfg: &LangfuseSpanConfig,
    session_id: &str,
) {
    if !cfg.langfuse_enabled {
        return;
    }
    span.record("langfuse.session.id", session_id);
    span.record("langfuse.trace.name", "visp.agent.run".to_string());

    if let Some(ref user_id) = cfg.langfuse_user_id {
        span.record("langfuse.user.id", user_id.as_str());
    }
    if let Some(ref tags) = cfg.langfuse_tags {
        span.record("langfuse.trace.tags", tags.as_str());
    }
    let env = cfg.langfuse_environment.as_deref().unwrap_or("default");
    span.record("langfuse.environment", env);
    if let Some(ref release) = cfg.langfuse_release {
        span.record("langfuse.release", release.as_str());
    }
    if let Some(ref version) = cfg.langfuse_version {
        span.record("langfuse.version", version.as_str());
    }
    if let Some(public) = cfg.langfuse_public {
        span.record("langfuse.trace.public", public);
    }
    if let Some(ref metadata) = cfg.langfuse_metadata
        && !metadata.is_empty()
        && let Ok(json) = serde_json::to_string(metadata)
    {
        span.record("langfuse.trace.metadata", json.as_str());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn full_cfg() -> LangfuseSpanConfig {
        let mut metadata = HashMap::new();
        metadata.insert("k".to_string(), "v".to_string());
        LangfuseSpanConfig {
            langfuse_enabled: true,
            langfuse_user_id: Some("user-1".to_string()),
            langfuse_tags: Some("[\"a\"]".to_string()),
            langfuse_environment: Some("prod".to_string()),
            langfuse_release: Some("rel-1".to_string()),
            langfuse_version: Some("1.0.0".to_string()),
            langfuse_public: Some(true),
            langfuse_metadata: Some(metadata),
        }
    }

    #[test]
    fn test_record_langfuse_returns_immediately_when_disabled() {
        let cfg = LangfuseSpanConfig::default();
        let span =
            tracing::info_span!("disabled_test", langfuse.session.id = tracing::field::Empty);
        // Must not panic and must be a no-op.
        record_langfuse_trace_fields(&span, &cfg, "session-1");
    }

    #[test]
    fn test_record_langfuse_all_fields() {
        let cfg = full_cfg();
        let span = tracing::info_span!(
            "enabled_test",
            langfuse.session.id = tracing::field::Empty,
            langfuse.trace.name = tracing::field::Empty,
            langfuse.user.id = tracing::field::Empty,
            langfuse.trace.tags = tracing::field::Empty,
            langfuse.environment = tracing::field::Empty,
            langfuse.release = tracing::field::Empty,
            langfuse.version = tracing::field::Empty,
            langfuse.trace.public = tracing::field::Empty,
            langfuse.trace.metadata = tracing::field::Empty,
        );
        // Recording all declared fields must not panic.
        record_langfuse_trace_fields(&span, &cfg, "session-1");
    }

    #[test]
    fn test_init_span_macro_expands() {
        let cfg = LangfuseSpanConfig::default();
        let span = init_span!(&cfg, "session-1", "macro_test", extra.field = "value");
        let _ = span;
    }

    #[test]
    fn test_init_span_macro_expands_with_enabled_cfg() {
        let cfg = full_cfg();
        let span = init_span!(&cfg, "session-1", "macro_enabled_test",);
        let _ = span;
    }
}
