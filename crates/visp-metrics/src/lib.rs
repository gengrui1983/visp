//! MetricsLayer — per-session metric aggregation and summary event emission.
//!
//! Accumulates token usage, API cost, and tool-call durations keyed by
//! `session.id`.  Emits a single `metrics.session.summary` event when the
//! primary (depth=0) `visp.agent.run` span closes.
//!
//! # Session ID strategy
//!
//! Neither `visp.tool.execute` nor `gen_ai.client.operation` carry
//! `session.id` as a direct field.  The layer propagates the session id
//! **down** the span tree during `on_new_span`: every new span checks its
//! parent chain for a `SessionId` extension and copies it if found.  This
//! avoids walking the parent chain at close time (when parents may already
//! have been dropped).
//!
//! Refs: design §6.2, §7.3 MetricsLayer; plan §Step 4b.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-session accumulated metrics.
///
/// Values are monotonically increasing within a session.  Reset on each new
/// session; never shared across sessions.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SessionMetrics {
    /// Sum of `gen_ai.usage.input_tokens` across all LLM calls.
    pub total_tokens_input: u64,
    /// Sum of `gen_ai.usage.output_tokens` across all LLM calls.
    pub total_tokens_output: u64,
    /// Sum of `gen_ai.usage.cache_read_input_tokens`.
    pub cache_read_input: u64,
    /// Sum of `gen_ai.usage.cache_creation_input_tokens`.
    pub cache_creation_input: u64,
    /// Sum of `visp.tool.duration_ms` across all tool calls.
    pub tool_duration_ms: u64,
    /// Number of tool calls (`visp.tool.execute` spans closed).
    pub tool_calls: u32,
    /// Number of LLM calls (`gen_ai.client.operation` spans closed).
    pub llm_calls: u32,
    /// LLM provider name (e.g. "anthropic", "openai"), captured from
    /// `gen_ai.provider.name` on the first `gen_ai.client.operation` span.
    pub provider_name: Option<String>,
    /// Wall-clock instant when this session was first seen.
    #[allow(dead_code)]
    pub started_at: Instant,
    /// Set when the summary event has been emitted for this session.
    pub ended_at: Option<Instant>,
}

impl Default for SessionMetrics {
    fn default() -> Self {
        Self {
            total_tokens_input: 0,
            total_tokens_output: 0,
            cache_read_input: 0,
            cache_creation_input: 0,
            tool_duration_ms: 0,
            tool_calls: 0,
            llm_calls: 0,
            provider_name: None,
            started_at: Instant::now(),
            ended_at: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers (stored on span extensions)
// ---------------------------------------------------------------------------

/// Fields collected from a span (initial + recorded), stored as a string map
/// in the span extension so they can be read in `on_close`.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
struct CollectedFields {
    map: BTreeMap<String, String>,
}

impl CollectedFields {
    #[allow(dead_code)]
    fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(|s| s.as_str())
    }

    #[allow(dead_code)]
    fn insert(&mut self, key: String, value: String) {
        self.map.insert(key, value);
    }
}

/// Marker type for a propagated `session.id`.  Stored on every span that is a
/// descendant of a `visp.agent.run` span so that `on_close` can determine the
/// session without walking the parent chain.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct SessionId(String);

// ---------------------------------------------------------------------------
// Field visitor (collects all field values as strings)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Default)]
struct FieldCollector {
    fields: BTreeMap<String, String>,
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
}

// ---------------------------------------------------------------------------
// MetricsLayer
// ---------------------------------------------------------------------------

/// A tracing [`Layer`] that accumulates per-session metrics.
///
/// **Aggregation** — On `on_close` of a `visp.tool.execute` span, the
/// duration is added to the corresponding session bucket; on `on_close` of a
/// `gen_ai.client.operation` span, token and cost fields are accumulated.
///
/// **Summary emission** — On `on_close` of a *primary* (`visp.agent.depth =
/// 0`) `visp.agent.run` span, a single `metrics.session.summary` event is
/// emitted via `tracing::info!`.  Re-entrant emission is prevented by an
/// `ended_at` marker.
///
/// **Session isolation** — Each session gets its own [`SessionMetrics`]
/// bucket inside a `DashMap`.  Sessions are independent and concurrent-safe.
// Not wired into the subscriber until Step 5-sub; `#[allow(dead_code)]`
// prevents warnings from the daemon binary target.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct MetricsLayer {
    sessions: Arc<DashMap<String, SessionMetrics>>,
}

#[allow(dead_code)]
impl MetricsLayer {
    /// Create a new `MetricsLayer` with no sessions tracked.
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
        }
    }

    /// Return the current accumulated metrics for `session_id`, if any.
    pub fn session_metrics(&self, session_id: &str) -> Option<SessionMetrics> {
        self.sessions.get(session_id).map(|r| r.clone())
    }

    /// Return all sessions and their accumulated metrics.
    pub fn all_sessions(&self) -> Vec<(String, SessionMetrics)> {
        self.sessions
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }
}

impl Default for MetricsLayer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Layer<S> implementation
// ---------------------------------------------------------------------------

impl<S> Layer<S> for MetricsLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    // ── on_new_span: collect fields + propagate session.id ─────────────────

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        // 1. Collect initial field values via the Visit trait.
        let mut collector = FieldCollector::default();
        attrs.record(&mut collector);
        let fields = CollectedFields {
            map: collector.fields,
        };

        // 2. Store collected fields so we can read them in on_close.
        let span_ref = match ctx.span(id) {
            Some(s) => s,
            None => return,
        };
        span_ref.extensions_mut().insert(fields.clone());

        // 3. Propagate session.id from parent (or from own fields).
        let has_own_session_id = fields.get("session.id").is_some();
        let propagated = if has_own_session_id {
            // This span explicitly declares session.id – store as marker.
            fields.get("session.id").map(|s| SessionId(s.to_string()))
        } else if let Some(parent) = span_ref.parent() {
            // Walk one level up — the parent should have SessionId if it is
            // under a visp.agent.run span (the propagation chain is built
            // incrementally).
            parent.extensions().get::<SessionId>().cloned()
        } else {
            None
        };

        if let Some(sid) = propagated {
            span_ref.extensions_mut().insert(sid);
        }
    }

    // ── on_record: update collected fields when span.record() is called ────

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let span_ref = match ctx.span(id) {
            Some(s) => s,
            None => return,
        };

        let mut collector = FieldCollector::default();
        values.record(&mut collector);
        if collector.fields.is_empty() {
            return;
        }

        if let Some(fields) = span_ref.extensions_mut().get_mut::<CollectedFields>() {
            for (k, v) in collector.fields {
                fields.insert(k, v);
            }
        }
    }

    // ── on_close: aggregate metrics or emit summary ───────────────────────

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let span_ref = match ctx.span(&id) {
            Some(s) => s,
            None => return,
        };

        let name = span_ref.metadata().name().to_string();
        let fields = match span_ref.extensions().get::<CollectedFields>() {
            Some(f) => f.clone(),
            None => return,
        };

        let session_id = match span_ref.extensions().get::<SessionId>() {
            Some(sid) => sid.0.clone(),
            None => return,
        };

        match name.as_str() {
            "visp.tool.execute" => {
                let duration_ms = fields
                    .get("visp.tool.duration_ms")
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0) as u64;

                let mut entry = self.sessions.entry(session_id).or_default();
                entry.tool_duration_ms += duration_ms;
                entry.tool_calls += 1;
            }

            "gen_ai.client.operation" => {
                let input_tokens = fields
                    .get("gen_ai.usage.input_tokens")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                let output_tokens = fields
                    .get("gen_ai.usage.output_tokens")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                let cache_read = fields
                    .get("gen_ai.usage.cache_read_input_tokens")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                let cache_creation = fields
                    .get("gen_ai.usage.cache_creation_input_tokens")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                let provider_name = fields.get("gen_ai.provider.name").map(|s| s.to_string());

                let mut entry = self.sessions.entry(session_id).or_default();
                entry.total_tokens_input += input_tokens;
                entry.total_tokens_output += output_tokens;
                entry.cache_read_input += cache_read;
                entry.cache_creation_input += cache_creation;
                entry.llm_calls += 1;
                if entry.provider_name.is_none() {
                    entry.provider_name = provider_name;
                }
            }

            "visp.agent.run" => {
                // Only the primary agent (depth = 0) emits the summary.
                let depth = fields
                    .get("visp.agent.depth")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1);
                if depth != 0 {
                    return;
                }

                let mut entry = match self.sessions.get_mut(&session_id) {
                    Some(e) => e,
                    None => return,
                };

                // Prevent duplicate emission for the same session.
                if entry.ended_at.is_some() {
                    return;
                }
                entry.ended_at = Some(Instant::now());

                let metrics = entry.clone();
                drop(entry); // Release the DashMap guard before emitting.

                tracing::info!(
                    target: "visp.metrics",
                    name = "metrics.session.summary",
                    session.id = %session_id,
                    provider = ?metrics.provider_name,
                    total_tokens_input = metrics.total_tokens_input,
                    total_tokens_output = metrics.total_tokens_output,
                    cache_read_tokens = metrics.cache_read_input,
                    cache_creation_tokens = metrics.cache_creation_input,
                    llm_calls = metrics.llm_calls,
                    tool_calls = metrics.tool_calls,
                    tool_duration_ms = metrics.tool_duration_ms,
                    "session completed",
                );

                // P0-4: Destroy the bucket after summary emission so a
                // subsequent visp.agent.run for the same session starts fresh.
                self.sessions.remove(&session_id);
            }

            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serial_test::serial;
    use tracing::Event;
    use tracing_subscriber::prelude::*;

    use super::*;

    /// Test-only layer that captures events with `target: "visp.metrics"`
    /// so we can assert the summary event was emitted.
    #[allow(clippy::type_complexity)]
    #[derive(Clone)]
    struct TestMetricsEventCollector {
        events: Arc<Mutex<Vec<(String, BTreeMap<String, String>)>>>,
    }

    impl TestMetricsEventCollector {
        fn new() -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn count_summary_events(&self) -> usize {
            let events = self.events.lock().unwrap();
            events
                .iter()
                .filter(|(_, fields)| {
                    fields.get("name").map(|s| s.as_str()) == Some("metrics.session.summary")
                })
                .count()
        }

        fn summary_event_field(&self, key: &str) -> Option<String> {
            let events = self.events.lock().unwrap();
            events
                .iter()
                .find(|(_, fields)| {
                    fields.get("name").map(|s| s.as_str()) == Some("metrics.session.summary")
                })
                .and_then(|(_, fields)| fields.get(key).cloned())
        }
    }

    impl<S> Layer<S> for TestMetricsEventCollector
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            if event.metadata().target() != "visp.metrics" {
                return;
            }
            let mut collector = FieldCollector::default();
            event.record(&mut collector);
            self.events
                .lock()
                .unwrap()
                .push((event.metadata().name().to_string(), collector.fields));
        }
    }

    // ------------------------------------------------------------------
    // W1-S4b-1 红 + W1-S4b-2 绿: Layer 骨架 + 字段收集
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_metrics_layer_collects_tool_durations_per_session() {
        let layer = MetricsLayer::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .set_default();

        // Create parent visp.agent.run with session.id
        let parent = tracing::info_span!(
            "visp.agent.run",
            session.id = "sess_tool_1",
            session.short_id = "sess_too",
            visp.agent.kind = "primary",
            visp.agent.depth = 0u64,
        );
        let _enter = parent.enter();

        // Create a tool span and record duration
        let tool = tracing::info_span!(
            "visp.tool.execute",
            gen_ai.tool.name = "bash",
            gen_ai.tool.call.id = "call_1",
            gen_ai.tool.type = "function",
            visp.tool.is_error = tracing::field::Empty,
            visp.tool.duration_ms = tracing::field::Empty,
        );
        tool.record("visp.tool.duration_ms", 150i64);
        // Tool updates are finalized in on_close, so drop tool first.
        drop(tool);

        // Snapshot metrics while the run span is still alive.
        let metrics = layer.session_metrics("sess_tool_1").unwrap();
        assert_eq!(
            metrics.tool_duration_ms, 150,
            "tool duration should be accumulated"
        );
        assert_eq!(metrics.tool_calls, 1, "should count one tool call");

        drop(_enter);
        drop(parent);
        drop(_guard);

        // Bucket is removed after summary → should be gone.
        assert!(layer.session_metrics("sess_tool_1").is_none());
    }

    #[test]
    #[serial]
    fn test_metrics_layer_collects_llm_tokens_per_session() {
        let layer = MetricsLayer::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .set_default();

        let parent = tracing::info_span!(
            "visp.agent.run",
            session.id = "sess_llm_1",
            session.short_id = "sess_llm",
            visp.agent.kind = "primary",
            visp.agent.depth = 0u64,
        );
        let _enter = parent.enter();

        // Create an LLM operation span and record completion fields
        let llm = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.system = "anthropic",
            gen_ai.request.model = "claude-sonnet-4-6",
            gen_ai.operation.name = "chat",
            gen_ai.request.max_tokens = tracing::field::Empty,
            gen_ai.request.temperature = tracing::field::Empty,
            visp.llm.attempt = 0u64,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read_input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation_input_tokens = tracing::field::Empty,
            gen_ai.response.finish_reasons = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
        );
        llm.record("gen_ai.usage.input_tokens", 100u64);
        llm.record("gen_ai.usage.output_tokens", 50u64);
        llm.record("gen_ai.usage.cache_read_input_tokens", 20u64);
        llm.record("gen_ai.usage.cache_creation_input_tokens", 10u64);
        // LLM updates are finalized in on_close, so drop llm first.
        drop(llm);

        // Snapshot while alive
        let metrics = layer.session_metrics("sess_llm_1").unwrap();
        assert_eq!(metrics.total_tokens_input, 100);
        assert_eq!(metrics.total_tokens_output, 50);
        assert_eq!(metrics.cache_read_input, 20);
        assert_eq!(metrics.cache_creation_input, 10);
        assert_eq!(metrics.llm_calls, 1);

        drop(_enter);
        drop(parent);
        drop(_guard);

        assert!(layer.session_metrics("sess_llm_1").is_none());
    }

    #[test]
    #[serial]
    fn test_metrics_layer_aggregates_per_session_id() {
        let layer = MetricsLayer::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .set_default();

        // Session A: two tool calls + one LLM call
        // Capture metrics while span is alive (before close removes bucket).
        let metrics_a;
        {
            let parent = tracing::info_span!(
                "visp.agent.run",
                session.id = "sess_a",
                session.short_id = "sess_a",
                visp.agent.kind = "primary",
                visp.agent.depth = 0u64,
            );
            let _enter = parent.enter();

            let t1 = tracing::info_span!(
                "visp.tool.execute",
                visp.tool.duration_ms = tracing::field::Empty
            );
            t1.record("visp.tool.duration_ms", 100i64);
            drop(t1);

            let t2 = tracing::info_span!(
                "visp.tool.execute",
                visp.tool.duration_ms = tracing::field::Empty
            );
            t2.record("visp.tool.duration_ms", 50i64);
            drop(t2);

            let llm = tracing::info_span!(
                "gen_ai.client.operation",
                gen_ai.usage.input_tokens = tracing::field::Empty,
                gen_ai.usage.output_tokens = tracing::field::Empty,
                gen_ai.usage.cache_read_input_tokens = tracing::field::Empty,
                gen_ai.usage.cache_creation_input_tokens = tracing::field::Empty,
            );
            llm.record("gen_ai.usage.input_tokens", 50u64);
            llm.record("gen_ai.usage.output_tokens", 25u64);
            drop(llm);

            metrics_a = layer.session_metrics("sess_a").unwrap();
            drop(_enter);
            drop(parent);
        }

        // Session B: one tool call + two LLM calls
        let metrics_b;
        {
            let parent = tracing::info_span!(
                "visp.agent.run",
                session.id = "sess_b",
                session.short_id = "sess_b",
                visp.agent.kind = "primary",
                visp.agent.depth = 0u64,
            );
            let _enter = parent.enter();

            let t1 = tracing::info_span!(
                "visp.tool.execute",
                visp.tool.duration_ms = tracing::field::Empty
            );
            t1.record("visp.tool.duration_ms", 200i64);
            drop(t1);

            let llm1 = tracing::info_span!(
                "gen_ai.client.operation",
                gen_ai.usage.input_tokens = tracing::field::Empty,
                gen_ai.usage.output_tokens = tracing::field::Empty,
                gen_ai.usage.cache_read_input_tokens = tracing::field::Empty,
                gen_ai.usage.cache_creation_input_tokens = tracing::field::Empty,
            );
            llm1.record("gen_ai.usage.input_tokens", 30u64);
            llm1.record("gen_ai.usage.output_tokens", 15u64);
            drop(llm1);

            let llm2 = tracing::info_span!(
                "gen_ai.client.operation",
                gen_ai.usage.input_tokens = tracing::field::Empty,
                gen_ai.usage.output_tokens = tracing::field::Empty,
                gen_ai.usage.cache_read_input_tokens = tracing::field::Empty,
                gen_ai.usage.cache_creation_input_tokens = tracing::field::Empty,
            );
            llm2.record("gen_ai.usage.input_tokens", 70u64);
            llm2.record("gen_ai.usage.output_tokens", 35u64);
            drop(llm2);

            metrics_b = layer.session_metrics("sess_b").unwrap();
            drop(_enter);
            drop(parent);
        }

        drop(_guard);

        assert_eq!(metrics_a.tool_duration_ms, 150);
        assert_eq!(metrics_a.tool_calls, 2);
        assert_eq!(metrics_a.total_tokens_input, 50);
        assert_eq!(metrics_a.total_tokens_output, 25);
        assert_eq!(metrics_a.llm_calls, 1);

        assert_eq!(metrics_b.tool_duration_ms, 200);
        assert_eq!(metrics_b.tool_calls, 1);
        assert_eq!(metrics_b.total_tokens_input, 100);
        assert_eq!(metrics_b.total_tokens_output, 50);
        assert_eq!(metrics_b.llm_calls, 2);
    }

    // ------------------------------------------------------------------
    // W1-S4b-3 红 + W1-S4b-4 绿: session 结束发 summary event
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_metrics_layer_emits_session_summary_on_agent_run_close() {
        let layer = MetricsLayer::new();
        let collector = TestMetricsEventCollector::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .with(collector.clone())
            .set_default();

        let parent = tracing::info_span!(
            "visp.agent.run",
            session.id = "sess_summary_1",
            session.short_id = "sess_sum",
            visp.agent.kind = "primary",
            visp.agent.depth = 0u64,
        );
        let _enter = parent.enter();

        // Add some data
        let tool = tracing::info_span!(
            "visp.tool.execute",
            visp.tool.duration_ms = tracing::field::Empty
        );
        tool.record("visp.tool.duration_ms", 100i64);
        drop(tool);

        let llm = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read_input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation_input_tokens = tracing::field::Empty,
        );
        llm.record("gen_ai.usage.input_tokens", 200u64);
        llm.record("gen_ai.usage.output_tokens", 100u64);
        drop(llm);

        drop(_enter);
        // Closing the parent span triggers the summary event.
        drop(parent);
        drop(_guard);

        assert_eq!(
            collector.count_summary_events(),
            1,
            "expected exactly one summary event"
        );

        let sid = collector.summary_event_field("session.id");
        assert_eq!(sid.as_deref(), Some("sess_summary_1"));

        let total_in = collector.summary_event_field("total_tokens_input").unwrap();
        assert_eq!(total_in, "200");
    }

    #[test]
    #[serial]
    fn test_metrics_layer_emits_per_run_after_bucket_removal() {
        let layer = MetricsLayer::new();
        let collector = TestMetricsEventCollector::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .with(collector.clone())
            .set_default();

        // First agent run → summary emitted, bucket removed.
        {
            let parent = tracing::info_span!(
                "visp.agent.run",
                session.id = "sess_multi",
                session.short_id = "sess_mu",
                visp.agent.kind = "primary",
                visp.agent.depth = 0u64,
            );
            let _enter = parent.enter();
            let tool = tracing::info_span!(
                "visp.tool.execute",
                visp.tool.duration_ms = tracing::field::Empty
            );
            tool.record("visp.tool.duration_ms", 50i64);
            drop(tool);
            drop(_enter);
            drop(parent);
        }

        assert_eq!(collector.count_summary_events(), 1);

        // Second agent run with same session.id → new bucket, new summary.
        {
            let parent = tracing::info_span!(
                "visp.agent.run",
                session.id = "sess_multi",
                session.short_id = "sess_mu",
                visp.agent.kind = "primary",
                visp.agent.depth = 0u64,
            );
            let _enter = parent.enter();
            let tool = tracing::info_span!(
                "visp.tool.execute",
                visp.tool.duration_ms = tracing::field::Empty
            );
            tool.record("visp.tool.duration_ms", 30i64);
            drop(tool);
            drop(_enter);
            drop(parent);
        }

        drop(_guard);

        assert_eq!(
            collector.count_summary_events(),
            2,
            "each visp.agent.run close produces a summary since bucket is removed"
        );
    }

    // ------------------------------------------------------------------
    // W1-S4b-5 红 + W1-S4b-6 绿: 访问器 API
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_metrics_layer_session_metrics_accessor() {
        let layer = MetricsLayer::new();

        // No session yet → None
        assert!(layer.session_metrics("nonexistent").is_none());

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .set_default();

        // Capture metrics while span is alive (before close removes bucket).
        let metrics_snapshot;
        {
            let parent = tracing::info_span!(
                "visp.agent.run",
                session.id = "sess_access",
                session.short_id = "sess_ac",
                visp.agent.kind = "primary",
                visp.agent.depth = 0u64,
            );
            let _enter = parent.enter();
            let tool = tracing::info_span!(
                "visp.tool.execute",
                visp.tool.duration_ms = tracing::field::Empty
            );
            tool.record("visp.tool.duration_ms", 75i64);
            drop(tool);

            metrics_snapshot = layer.session_metrics("sess_access");
            drop(_enter);
            drop(parent);
        }

        drop(_guard);

        assert!(
            metrics_snapshot.is_some(),
            "expected metrics for sess_access"
        );
        assert_eq!(metrics_snapshot.unwrap().tool_calls, 1);
        // After close, bucket is removed.
        assert!(layer.session_metrics("sess_access").is_none());
    }

    #[test]
    #[serial]
    fn test_metrics_layer_all_sessions_accessor() {
        let layer = MetricsLayer::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .set_default();

        // Capture sessions before they close (after close, bucket is removed).
        let mut snapshot_session_count = 0;
        for sid in &["sess_all_a", "sess_all_b"] {
            let parent = tracing::info_span!(
                "visp.agent.run",
                session.id = *sid,
                session.short_id = &sid[..8],
                visp.agent.kind = "primary",
                visp.agent.depth = 0u64,
            );
            let _enter = parent.enter();
            let tool = tracing::info_span!(
                "visp.tool.execute",
                visp.tool.duration_ms = tracing::field::Empty
            );
            tool.record("visp.tool.duration_ms", 10i64);
            drop(tool);

            // Snapshot before close
            let metrics = layer.session_metrics(sid);
            if metrics.is_some() {
                snapshot_session_count += 1;
            }

            drop(_enter);
            drop(parent);
        }

        drop(_guard);

        assert_eq!(snapshot_session_count, 2, "expected 2 sessions while alive");

        // After all sessions closed, all_sessions should be empty.
        let all = layer.all_sessions();
        assert!(all.is_empty(), "expected no sessions after all were closed");
    }

    // ── P0-4: bucket cleanup on summary ─────────────────────────────────

    #[test]
    #[serial]
    fn test_metrics_layer_removes_bucket_after_summary() {
        let layer = MetricsLayer::new();
        let collector = TestMetricsEventCollector::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .with(collector.clone())
            .set_default();

        {
            let parent = tracing::info_span!(
                "visp.agent.run",
                session.id = "sess_cleanup",
                session.short_id = "sess_cl",
                visp.agent.kind = "primary",
                visp.agent.depth = 0u64,
            );
            let _enter = parent.enter();
            let tool = tracing::info_span!(
                "visp.tool.execute",
                visp.tool.duration_ms = tracing::field::Empty
            );
            tool.record("visp.tool.duration_ms", 50i64);
            drop(tool);
            drop(_enter);
            drop(parent);
        }

        // Summary should have been emitted
        assert_eq!(collector.count_summary_events(), 1);

        // Bucket should be removed
        assert!(
            layer.session_metrics("sess_cleanup").is_none(),
            "bucket should be removed after summary emission"
        );
    }

    #[test]
    #[serial]
    fn test_metrics_layer_no_pollution_on_session_rerun() {
        let layer = MetricsLayer::new();
        let collector = TestMetricsEventCollector::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .with(collector.clone())
            .set_default();

        // First run: emits summary, bucket is removed.
        {
            let parent = tracing::info_span!(
                "visp.agent.run",
                session.id = "sess_rerun",
                session.short_id = "sess_re",
                visp.agent.kind = "primary",
                visp.agent.depth = 0u64,
            );
            let _enter = parent.enter();
            let tool = tracing::info_span!(
                "visp.tool.execute",
                visp.tool.duration_ms = tracing::field::Empty
            );
            tool.record("visp.tool.duration_ms", 30i64);
            drop(tool);
            drop(_enter);
            drop(parent);
        }

        assert_eq!(collector.count_summary_events(), 1);
        assert!(layer.session_metrics("sess_rerun").is_none());

        // Second run with same session.id: new bucket, fresh summary.
        {
            let parent = tracing::info_span!(
                "visp.agent.run",
                session.id = "sess_rerun",
                session.short_id = "sess_re",
                visp.agent.kind = "primary",
                visp.agent.depth = 0u64,
            );
            let _enter = parent.enter();
            let tool = tracing::info_span!(
                "visp.tool.execute",
                visp.tool.duration_ms = tracing::field::Empty
            );
            tool.record("visp.tool.duration_ms", 60i64);
            drop(tool);
            drop(_enter);
            drop(parent);
        }

        // Should emit a second summary (fresh bucket, no duplicate guard)
        assert_eq!(
            collector.count_summary_events(),
            2,
            "second run should emit its own summary"
        );

        // Second bucket should also be removed
        assert!(
            layer.session_metrics("sess_rerun").is_none(),
            "second bucket should also be removed"
        );
    }

    #[test]
    #[serial]
    fn test_metrics_layer_captures_provider_name_in_summary() {
        let layer = MetricsLayer::new();
        let collector = TestMetricsEventCollector::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .with(collector.clone())
            .set_default();

        let parent = tracing::info_span!(
            "visp.agent.run",
            session.id = "sess_provider",
            session.short_id = "sess_pr",
            visp.agent.kind = "primary",
            visp.agent.depth = 0u64,
        );
        let _enter = parent.enter();

        let llm = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.provider.name = "anthropic",
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read_input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation_input_tokens = tracing::field::Empty,
        );
        llm.record("gen_ai.usage.input_tokens", 100u64);
        llm.record("gen_ai.usage.output_tokens", 50u64);
        drop(llm);

        drop(_enter);
        drop(parent);
        drop(_guard);

        assert_eq!(collector.count_summary_events(), 1);
        let provider = collector.summary_event_field("provider");
        assert_eq!(
            provider.as_deref(),
            Some("Some(\"anthropic\")"),
            "summary should include provider name"
        );
    }
}
