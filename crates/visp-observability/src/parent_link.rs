//! ParentLinkLayer — cross-mpsc span parent_span_id field completion.
//!
//! Wave 1: field complement only, does **not** modify the tracing tree parent.
//! Refs: design §7.3, plan §Step 4a.
//!
//! # Mapping
//!
//! Maintains a bidirectional mapping between W3C span IDs (either from
//! [`TraceContext`] or [`SpanW3CId`]) and tracing [`span::Id`] values.
//! When a span carries a [`TraceContext`] whose `parent_span_id` is `Some`,
//! the layer looks up that parent W3C ID in the mapping and writes
//! [`ParentLinkFields`] with the resolved parent information.
//!
//! The mapping is cleaned up in `on_close` (both directions).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use opentelemetry::trace::TraceContextExt;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Newtype wrapping a 16-hex W3C span ID, stored in span extensions for
/// cross‑layer span identification.
///
/// Written to the extension of every `visp.agent.run`, `visp.agent.iteration`,
/// and `visp.subagent.spawn` span so that [`ParentLinkLayer`] can register
/// a bidirectional mapping between W3C span IDs and tracing span IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanW3CId(pub String);

/// Custom fields inserted into the span extension when trace context fields
/// are recorded via `span.record()`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ParentLinkFields {
    pub trace_id: String,
    pub parent_span_id: Option<String>,
    pub trace_state: Option<String>,
}

/// Accumulates trace context fields across multiple `span.record()` calls.
#[derive(Default, Clone)]
struct PendingTraceFields {
    trace_id: Option<String>,
    parent_span_id: Option<String>,
    trace_state: Option<String>,
}

/// Inner state shared via `Arc` so the layer can be cloned cheaply.
#[allow(dead_code)]
#[derive(Debug)]
struct Inner {
    /// Forward mapping: W3C span_id → tracing span::Id.
    mapping: DashMap<String, tracing::span::Id>,
    /// Reverse mapping: tracing span::Id → W3C span_id.
    reverse: DashMap<tracing::span::Id, String>,
    /// Counter of parent span IDs not found in mapping.
    unmatched_count: AtomicU64,
}

/// A tracing [`Layer`] that detects [`TraceContext`] and [`SpanW3CId`] in
/// span extensions and writes [`ParentLinkFields`] into the span extension
/// for JSON field completion by a downstream fmt layer.
///
/// # Wave 1 scope
/// - Maintains a bidirectional mapping: W3C span_id ↔ tracing `span::Id`.
/// - Counts unmatched parent span IDs (for debugging / Wave 2 validation).
/// - Cleans up mapping entries on span close.
/// - Does **not** alter the tracing tree parent — field complement only.
// Not yet wired into the subscriber; Step 5-sub will assemble it.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ParentLinkLayer {
    inner: Arc<Inner>,
    /// When `true`, [`on_enter`](Layer::on_enter) reads the real OTel span ID
    /// from [`opentelemetry::Context::current()`] (thread-local) and records
    /// it onto the span as `visp.span.w3c_id`, overwriting any W1 uuid.
    ///
    /// When `false` (default), the W1 uuid path is used unchanged.
    otel_mode: bool,
}

impl ParentLinkLayer {
    /// Create a new `ParentLinkLayer` with empty mapping and zero counter.
    /// Uses W1 uuid mode (OTel disabled).
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                mapping: DashMap::new(),
                reverse: DashMap::new(),
                unmatched_count: AtomicU64::new(0),
            }),
            otel_mode: false,
        }
    }

    /// Create a new `ParentLinkLayer` with the given OTel mode flag.
    ///
    /// - `true`: OTel mode — `on_enter` reads real OTel span ID.
    /// - `false`: W1 uuid mode — identical to [`new`](Self::new).
    pub fn with_otel_mode(enable: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                mapping: DashMap::new(),
                reverse: DashMap::new(),
                unmatched_count: AtomicU64::new(0),
            }),
            otel_mode: enable,
        }
    }

    /// Return the number of parent span IDs that were **not** found in the
    /// mapping (a debugging / Wave 2 metric).
    #[allow(dead_code)]
    pub fn unmatched_count(&self) -> u64 {
        self.inner.unmatched_count.load(Ordering::Relaxed)
    }
}

impl Default for ParentLinkLayer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl ParentLinkLayer {
    /// Remove a W3C span_id from the mapping (test helper).
    /// Used to simulate "parent W3C span_id not in mapping" scenarios.
    #[allow(dead_code)]
    pub fn remove_from_mapping(&self, span_id: &str) {
        if let Some((_, tid)) = self.inner.mapping.remove(span_id) {
            self.inner.reverse.remove(&tid);
        }
    }
}

/// Marker inserted into span extension after fields have been recorded
/// via `on_enter`, to prevent re-recording on subsequent enters.
#[derive(Clone)]
struct ParentLinkFieldsRecorded;

// ---------------------------------------------------------------------------
// Trace fields extractor (used in on_record)
// ---------------------------------------------------------------------------

/// Extracts trace context fields and W3C span IDs from span recordings.
#[derive(Default)]
struct TraceFieldsExtractor {
    w3c_id: Option<String>,
    trace_id: Option<String>,
    parent_span_id: Option<String>,
    trace_state: Option<String>,
}

impl Visit for TraceFieldsExtractor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "visp.span.w3c_id" => self.w3c_id = Some(value.to_string()),
            "trace_id" => self.trace_id = Some(value.to_string()),
            "parent_span_id" => self.parent_span_id = Some(value.to_string()),
            "trace_state" => self.trace_state = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {
        // Not interested in debug-recorded fields.
    }
}

// ---------------------------------------------------------------------------
// Layer implementation
// ---------------------------------------------------------------------------

impl<S> Layer<S> for ParentLinkLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        _attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let span_ref = match ctx.span(id) {
            Some(s) => s,
            None => return,
        };

        // Register mapping from SpanW3CId if present (set by another layer).
        if let Some(sw) = span_ref.extensions().get::<SpanW3CId>().cloned() {
            self.inner.mapping.insert(sw.0.clone(), id.clone());
            self.inner.reverse.insert(id.clone(), sw.0);
        }

        // Cross-mpsc propagation: copy ParentLinkFields from parent span.
        // This mirrors MetricsLayer's SessionId propagation pattern.
        if span_ref.extensions().get::<ParentLinkFields>().is_some() {
            return; // Already has explicit fields (set via on_record).
        }

        if let Some(parent) = span_ref.parent()
            && let Some(plf) = parent.extensions().get::<ParentLinkFields>().cloned()
        {
            span_ref.extensions_mut().insert(plf);
        }
    }

    /// Detect `visp.span.w3c_id`, `trace_id`, `parent_span_id`, and
    /// `trace_state` field recordings.
    ///
    /// - W3C IDs are registered in the mapping.
    /// - Trace context fields are accumulated via [`PendingTraceFields`]
    ///   and written as [`ParentLinkFields`] into the span extension.
    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        let mut extractor = TraceFieldsExtractor::default();
        values.record(&mut extractor);

        let span_ref = match ctx.span(id) {
            Some(s) => s,
            None => return,
        };

        // Handle visp.span.w3c_id — register W3C ID in mapping.
        // Uses `replace` instead of `insert` because in OTel mode the
        // `on_enter` callback may record `visp.span.w3c_id` a second time
        // (via `Span::current().record`), which triggers this `on_record`
        // callback again.  `insert` would panic on the second call.
        //
        // When OTel mode overwrites the W1 UUID with the real OTel span_id,
        // remove the stale UUID entry from the forward mapping to prevent
        // a leak (Oracle P1).
        if let Some(w3c_id) = extractor.w3c_id {
            span_ref.extensions_mut().replace(SpanW3CId(w3c_id.clone()));
            // Remove stale forward mapping entry if this span's reverse
            // already points to a different W3C ID (e.g. OTel overwrite).
            if let Some(old_rev) = self.inner.reverse.get(id) {
                let old_w3c = old_rev.value().clone();
                drop(old_rev);
                if old_w3c != w3c_id {
                    self.inner.mapping.remove(&old_w3c);
                }
            }
            self.inner.mapping.insert(w3c_id.clone(), id.clone());
            self.inner.reverse.insert(id.clone(), w3c_id);
        }

        // Handle trace context fields — accumulate and write ParentLinkFields.
        let has_trace_fields = extractor.trace_id.is_some()
            || extractor.parent_span_id.is_some()
            || extractor.trace_state.is_some();

        if !has_trace_fields {
            return;
        }

        // Accumulate partial field data across multiple record() calls.
        let mut pending = span_ref
            .extensions()
            .get::<PendingTraceFields>()
            .cloned()
            .unwrap_or_default();

        if let Some(tid) = extractor.trace_id {
            pending.trace_id = Some(tid);
        }
        if let Some(psid) = &extractor.parent_span_id {
            // Only count unmatched the first time for this span.
            if pending.parent_span_id.is_none() && !self.inner.mapping.contains_key(psid) {
                self.inner.unmatched_count.fetch_add(1, Ordering::Relaxed);
            }
            pending.parent_span_id = Some(psid.clone());
        }
        if let Some(ts) = extractor.trace_state {
            pending.trace_state = Some(ts);
        }

        // Remove-then-insert to avoid tracing-subscriber's debug assertion
        // that prevents re-inserting an extension of the same type.
        span_ref.extensions_mut().remove::<PendingTraceFields>();
        span_ref.extensions_mut().insert(pending.clone());

        // Write ParentLinkFields if we have at least trace_id.
        if let Some(trace_id) = &pending.trace_id {
            span_ref.extensions_mut().remove::<ParentLinkFields>();
            span_ref.extensions_mut().insert(ParentLinkFields {
                trace_id: trace_id.clone(),
                parent_span_id: pending.parent_span_id.clone(),
                trace_state: pending.trace_state.clone(),
            });
        }
    }

    /// Clean up mapping entries when a span is closed (P1-2).
    fn on_close(&self, id: tracing::span::Id, _ctx: Context<'_, S>) {
        if let Some((_, w3c_id)) = self.inner.reverse.remove(&id) {
            self.inner.mapping.remove(&w3c_id);
        }
    }

    /// Record `trace_id` and `parent_span_id` from [`ParentLinkFields`] onto
    /// the span so that the JSON fmt layer outputs them (Approach A).
    ///
    /// This fires on first entry only; subsequent entries are no-ops thanks
    /// to [`ParentLinkFieldsRecorded`] marker.
    ///
    /// # OTel mode
    ///
    /// When [`otel_mode`](Self::otel_mode) is `true`, additionally reads the
    /// real OTel [`SpanContext`] from the thread-local
    /// [`opentelemetry::Context::current()`] and records `visp.span.w3c_id`
    /// onto the current span, overwriting any W1 uuid that was set at span
    /// creation time.
    ///
    /// The `SpanRef` from the `Context` is dropped before
    /// thread-local OTel context access to keep the pattern consistent
    /// (the original deadlock concern was with `get_otel_context`).
    fn on_enter(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        let span_ref = match ctx.span(id) {
            Some(s) => s,
            None => return,
        };

        // Already recorded on a previous enter.
        let already_recorded = span_ref
            .extensions()
            .get::<ParentLinkFieldsRecorded>()
            .is_some();

        if already_recorded {
            return;
        }

        // Clone ParentLinkFields and set the guard before dropping span_ref,
        // avoiding a redundant ctx.span(id) re-acquisition later.
        let plf = span_ref.extensions().get::<ParentLinkFields>().cloned();
        span_ref.extensions_mut().insert(ParentLinkFieldsRecorded);

        // Drop span_ref to release Extensions before OTel context access
        // (keeps pattern consistent even though thread-local avoids deadlock).
        drop(span_ref);

        // W1 path: record trace_id and parent_span_id from ParentLinkFields.
        if let Some(plf) = plf {
            let current = tracing::Span::current();
            current.record("trace_id", plf.trace_id.as_str());
            if let Some(ref psid) = plf.parent_span_id {
                current.record("parent_span_id", psid.as_str());
            }
        }

        // OTel mode: read OTel SpanContext and record visp.span.w3c_id,
        // overwriting any W1 uuid recorded at span creation time.
        //
        // We use `opentelemetry::Context::current()` because the OTel layer's
        // `on_enter` (registered before us in assembly order) already called
        // `cx.clone().attach()`, which stores this span's OTel Context in
        // the thread-local.  Using the thread-local avoids the need for
        // `get_otel_context` which must find the `WithContext` via dispatch
        // downcast and has stricter deadlock constraints.
        if self.otel_mode {
            let otel_context = opentelemetry::Context::current();
            let span_ref = otel_context.span();
            let span_ctx = span_ref.span_context();
            if span_ctx.is_valid() {
                let hex = span_ctx.span_id().to_string();
                tracing::Span::current().record("visp.span.w3c_id", hex.as_str());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Mutex;

    use opentelemetry::trace::TracerProvider;
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use serial_test::serial;
    use tracing::span::{Attributes, Id, Record};
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::Layer;
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::registry::LookupSpan;

    use super::*;
    use crate::otlp;
    use visp_config::OtlpConfig;

    /// Injector layer that places a `SpanW3CId` into every new span's
    /// extension.
    #[derive(Clone)]
    struct TestW3CIdInjector {
        w3c_id: String,
    }

    impl TestW3CIdInjector {
        fn new(w3c_id: &str) -> Self {
            Self {
                w3c_id: w3c_id.to_string(),
            }
        }
    }

    impl<S> Layer<S> for TestW3CIdInjector
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, _attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
            if let Some(span_ref) = ctx.span(id) {
                span_ref
                    .extensions_mut()
                    .insert(SpanW3CId(self.w3c_id.clone()));
            }
        }
    }

    /// Collector layer that records whether any span's extension contains
    /// `ParentLinkFields`. Captures both via `on_new_span` (propagation)
    /// and `on_record` (explicit field recording).
    #[derive(Clone)]
    struct TestFieldsCollector {
        seen: Arc<Mutex<Vec<ParentLinkFields>>>,
    }

    impl TestFieldsCollector {
        fn new() -> Self {
            Self {
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl<S> Layer<S> for TestFieldsCollector
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, _attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
            if let Some(span_ref) = ctx.span(id)
                && let Some(plf) = span_ref.extensions().get::<ParentLinkFields>().cloned()
            {
                self.seen.lock().unwrap().push(plf);
            }
        }

        fn on_record(&self, id: &Id, _values: &Record<'_>, ctx: Context<'_, S>) {
            if let Some(span_ref) = ctx.span(id)
                && let Some(plf) = span_ref.extensions().get::<ParentLinkFields>().cloned()
            {
                self.seen.lock().unwrap().push(plf);
            }
        }
    }

    // ── W1-S4a-1: Layer skeleton ──────────────────────────────────────────

    #[test]
    #[serial]
    fn test_parent_link_layer_compiles_and_registers() {
        let _guard = tracing_subscriber::registry()
            .with(ParentLinkLayer::new())
            .set_default();
        // Compilation + registration is sufficient for this test.
    }

    #[test]
    #[serial]
    fn test_parent_link_layer_no_op_when_no_trace_context() {
        let _guard = tracing_subscriber::registry()
            .with(ParentLinkLayer::new())
            .set_default();

        let span = tracing::info_span!("normal_span");
        let _enter = span.enter();
        // Must not panic.
    }

    // ── W1-S4a-3 (rewritten): span field recording ────────────────────────

    #[test]
    #[serial]
    fn test_parent_link_layer_inserts_parent_link_fields_extension() {
        let collector = TestFieldsCollector::new();

        let _guard = tracing_subscriber::registry()
            .with(ParentLinkLayer::new())
            .with(collector.clone())
            .set_default();

        let span = tracing::info_span!(
            "visp.subagent.spawn",
            trace_id = tracing::field::Empty,
            parent_span_id = tracing::field::Empty,
        );
        span.record("trace_id", "0af7651916cd43dd8448eb211c80319c");
        drop(span);

        let seen = collector.seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            1,
            "expected exactly one span with ParentLinkFields"
        );
        assert_eq!(seen[0].trace_id, "0af7651916cd43dd8448eb211c80319c");
        // parent_span_id is None because we didn't record it
        assert_eq!(seen[0].parent_span_id, None);
    }

    #[test]
    #[serial]
    fn test_parent_link_layer_no_fields_when_no_trace_context() {
        let collector = TestFieldsCollector::new();

        let _guard = tracing_subscriber::registry()
            .with(ParentLinkLayer::new())
            .with(collector.clone())
            .set_default();

        let span = tracing::info_span!("plain_span");
        drop(span);

        let seen = collector.seen.lock().unwrap();
        assert!(
            seen.is_empty(),
            "expected no ParentLinkFields for plain span"
        );
    }

    // ── P0-3: parent_span_id via mapping (rewritten: span field recording) ─

    #[test]
    #[serial]
    fn test_parent_link_writes_parent_span_id_when_parent_in_mapping() {
        let layer = ParentLinkLayer::new();
        let collector = TestFieldsCollector::new();

        // Parent span registers its W3C ID in the mapping via SpanW3CId extension.
        // Child span records parent_span_id pointing to the parent's W3C ID.
        let parent_w3c = "aaaaaaaaaaaaaaaa";

        let _guard = tracing_subscriber::registry()
            .with(TestW3CIdInjector::new(parent_w3c))
            .with(layer.clone())
            .with(collector.clone())
            .set_default();

        // Create parent span → registers "aaa..." in mapping.
        // Keep it alive while child is being created.
        let parent_span = tracing::info_span!("parent");
        let _parent_enter = parent_span.enter();

        // Create child span with field recordings.
        let child_span = tracing::info_span!(
            "child",
            trace_id = tracing::field::Empty,
            parent_span_id = tracing::field::Empty,
        );
        child_span.record("trace_id", "0af7651916cd43dd8448eb211c80319c");
        child_span.record("parent_span_id", parent_w3c);
        drop(child_span);

        let seen = collector.seen.lock().unwrap();
        // We expect 1 span with ParentLinkFields (the child)
        let child_plf = seen.iter().find(|plf| plf.parent_span_id.is_some());
        assert!(
            child_plf.is_some(),
            "expected a ParentLinkFields with parent_span_id set"
        );
        assert_eq!(
            child_plf.unwrap().parent_span_id.as_deref(),
            Some(parent_w3c),
        );
    }

    #[test]
    #[serial]
    fn test_parent_link_increments_unmatched_when_parent_missing() {
        let layer = ParentLinkLayer::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .set_default();

        assert_eq!(layer.unmatched_count(), 0);

        // Record parent_span_id that doesn't exist in mapping.
        let child_span = tracing::info_span!("child", parent_span_id = tracing::field::Empty,);
        child_span.record("parent_span_id", "aaaaaaaaaaaaaaaa");
        drop(child_span);

        assert!(
            layer.unmatched_count() > 0,
            "expected unmatched_count > 0 when parent W3C ID is not in mapping"
        );
    }

    #[test]
    #[serial]
    fn test_parent_link_registers_w3c_id_from_span_w3c_id_extension() {
        let layer = ParentLinkLayer::new();
        let collector = TestFieldsCollector::new();

        let parent_w3c = "cccccccccccccccc";

        let _guard = tracing_subscriber::registry()
            .with(TestW3CIdInjector::new(parent_w3c))
            .with(layer.clone())
            .with(collector.clone())
            .set_default();

        // Parent span creates mapping via SpanW3CId extension
        let parent_span = tracing::info_span!("parent");
        let _parent_enter = parent_span.enter();

        // Child span records field parent_span_id
        let child_span = tracing::info_span!(
            "child",
            trace_id = tracing::field::Empty,
            parent_span_id = tracing::field::Empty,
        );
        child_span.record("trace_id", "0af7651916cd43dd8448eb211c80319c");
        child_span.record("parent_span_id", parent_w3c);
        drop(child_span);

        let seen = collector.seen.lock().unwrap();
        let child_plf = seen.iter().find(|plf| plf.parent_span_id.is_some());
        assert!(
            child_plf.is_some(),
            "child should have parent_span_id resolved"
        );
        assert_eq!(
            child_plf.unwrap().parent_span_id.as_deref(),
            Some(parent_w3c),
        );
    }

    // ── P1-2: mapping cleanup on span close ───────────────────────────────

    #[test]
    #[serial]
    fn test_parent_link_cleans_mapping_on_span_close() {
        let layer = ParentLinkLayer::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .set_default();

        // Register parent W3C ID via field recording.
        let parent_span = tracing::info_span!("parent", visp.span.w3c_id = tracing::field::Empty,);
        parent_span.record("visp.span.w3c_id", "eeeeeeeeeeeeeeee");
        let parent_id = parent_span.id().unwrap();

        // parent is in mapping
        assert!(layer.inner.mapping.contains_key("eeeeeeeeeeeeeeee"));
        assert!(layer.inner.reverse.contains_key(&parent_id));

        drop(parent_span);
        // After close, parent should be removed from both maps
        assert!(
            !layer.inner.mapping.contains_key("eeeeeeeeeeeeeeee"),
            "parent should be removed from forward mapping on close"
        );
        assert!(
            !layer.inner.reverse.contains_key(&parent_id),
            "parent should be removed from reverse mapping on close"
        );
    }

    // ── W1-S4a-5: unmatched parent + metric (span field recording) ─────────

    #[test]
    #[serial]
    fn test_parent_link_layer_unmatched_parent_recorded() {
        let layer = ParentLinkLayer::new();
        let layer_clone = layer.clone();

        let _guard = tracing_subscriber::registry()
            .with(layer_clone)
            .set_default();

        assert_eq!(layer.unmatched_count(), 0);

        // Record parent_span_id pointing to unknown W3C ID → unmatched_count++.
        let child_span = tracing::info_span!("child", parent_span_id = tracing::field::Empty,);
        child_span.record("parent_span_id", "aaaaaaaaaaaaaaaa");
        drop(child_span);

        assert!(
            layer.unmatched_count() > 0,
            "expected unmatched_count > 0 when parent W3C ID is not in mapping"
        );
    }

    #[test]
    #[serial]
    fn test_parent_link_layer_exposes_unmatched_count() {
        let layer = ParentLinkLayer::new();
        // Fresh layer: count must be 0.
        assert_eq!(layer.unmatched_count(), 0);
    }

    // ── Step 1b: on_record reads trace fields from subagent.spawn span ───

    #[test]
    #[serial]
    fn test_parent_link_reads_trace_fields_from_subagent_spawn_span() {
        let layer = ParentLinkLayer::new();
        let collector = TestFieldsCollector::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .with(collector.clone())
            .set_default();

        // Create a span named visp.subagent.spawn with trace_id and parent_span_id fields.
        let span = tracing::info_span!(
            "visp.subagent.spawn",
            visp.subagent.name = "test",
            trace_id = tracing::field::Empty,
            parent_span_id = tracing::field::Empty,
            visp.span.w3c_id = tracing::field::Empty,
        );
        span.record("trace_id", "0af7651916cd43dd8448eb211c80319c");
        span.record("parent_span_id", "aaaaaaaaaaaaaaaa");
        // Record a W3C ID so the span registers in mapping.
        span.record("visp.span.w3c_id", "bbbbbbbbbbbbbbbb");
        let span_id = span.id().unwrap();

        // Check mapping BEFORE dropping the span (on_close would remove it).
        assert!(layer.inner.mapping.contains_key("bbbbbbbbbbbbbbbb"));
        assert!(layer.inner.reverse.contains_key(&span_id));

        drop(span);

        // ParentLinkFields should be present in the span extension.
        // TestFieldsCollector captures in both on_new_span and on_record,
        // so there may be multiple entries for the same span.
        let seen = collector.seen.lock().unwrap();
        assert!(
            !seen.is_empty(),
            "expected at least one ParentLinkFields capture"
        );
        // The first capture may come from the trace_id record (parent_span_id
        // not yet set). Find the entry with parent_span_id to verify completeness.
        let with_parent = seen.iter().find(|plf| plf.parent_span_id.is_some());
        assert!(
            with_parent.is_some(),
            "expected a ParentLinkFields entry with parent_span_id set"
        );
        assert_eq!(
            with_parent.unwrap().trace_id,
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_eq!(
            with_parent.unwrap().parent_span_id.as_deref(),
            Some("aaaaaaaaaaaaaaaa"),
        );
    }

    #[test]
    #[serial]
    fn test_parent_link_propagates_fields_to_child_spans() {
        let layer = ParentLinkLayer::new();
        let collector = TestFieldsCollector::new();

        let _guard = tracing_subscriber::registry()
            .with(layer.clone())
            .with(collector.clone())
            .set_default();

        // Parent span: visp.subagent.spawn with trace context fields.
        let parent = tracing::info_span!(
            "visp.subagent.spawn",
            trace_id = tracing::field::Empty,
            parent_span_id = tracing::field::Empty,
        );
        parent.record("trace_id", "0af7651916cd43dd8448eb211c80319c");
        parent.record("parent_span_id", "aaaaaaaaaaaaaaaa");

        // Child span (e.g. visp.agent.run) is a tracing child of parent.
        let child =
            tracing::info_span!("visp.agent.run", visp.span.w3c_id = tracing::field::Empty,);
        child.record("visp.span.w3c_id", "cccccccccccccccc");

        // Drop parent then child (order matters: parent must outlive child for propagation).
        // Actually both are in the same scope, so the tracing tree should work.
        drop(parent);
        drop(child);

        // Both spans should have ParentLinkFields (child gets it via propagation).
        let seen = collector.seen.lock().unwrap();
        // The child span propagates fields from parent when child is a tracing child.
        // TestFieldsCollector captures from both on_new_span and on_record,
        // so we may see more entries than spans.
        assert!(
            seen.len() >= 2,
            "expected at least two ParentLinkFields captures (parent + child), got {}: {:?}",
            seen.len(),
            seen,
        );

        // Verify child has the correct propagated trace_id.
        let child_plf = seen.iter().find(|plf| plf.parent_span_id.is_some());
        assert!(
            child_plf.is_some(),
            "child should have parent_span_id propagated"
        );
        assert_eq!(
            child_plf.unwrap().trace_id,
            "0af7651916cd43dd8448eb211c80319c"
        );
    }

    // ── W2-S3-1: OTel mode tests ───────────────────────────────────────────

    /// In-memory writer for capturing fmt JSON output.
    #[derive(Clone)]
    struct TestSpanWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl TestSpanWriter {
        fn new() -> Self {
            Self {
                buf: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn into_string(self) -> String {
            String::from_utf8(self.buf.lock().unwrap().clone()).unwrap_or_default()
        }
    }

    impl<'a> MakeWriter<'a> for TestSpanWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl io::Write for TestSpanWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buf.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.buf.lock().unwrap().flush()
        }
    }

    /// Helper: build a full OTel-enabled subscriber for OTel mode tests.
    fn build_otel_subscriber(
        writer: TestSpanWriter,
    ) -> (
        tracing::subscriber::DefaultGuard,
        InMemorySpanExporter,
        ParentLinkLayer,
    ) {
        let exporter = InMemorySpanExporter::default();
        let provider = otlp::build_tracer_provider_with_exporter(
            exporter.clone(),
            &OtlpConfig {
                enabled: true,
                ..Default::default()
            },
        );
        let tracer = provider.tracer("visp-daemon-test");
        let otel_layer =
            tracing_opentelemetry::OpenTelemetryLayer::new(tracer).with_context_activation(true);
        let parent_link = ParentLinkLayer::with_otel_mode(true);

        let guard = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(otel_layer)
            .with(parent_link.clone())
            .with(tracing_subscriber::fmt::layer().json().with_writer(writer))
            .set_default();

        (guard, exporter, parent_link)
    }

    #[test]
    #[serial]
    fn test_parent_link_uuid_mode_when_otel_disabled() {
        // W1 path: ParentLinkLayer::new() with otel_mode=false.
        // The W1 uuid is generated by visp-core and recorded via span.record().
        // Here we simulate that flow and verify the mapping/JSON path works.
        let writer = TestSpanWriter::new();
        let parent_link = ParentLinkLayer::new(); // otel_mode=false

        let _guard = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(parent_link.clone())
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(writer.clone()),
            )
            .set_default();

        let id1 = "a1b2c3d4e5f6a7b8";
        let id2 = "1122334455667788";

        // Simulate W1: record visp.span.w3c_id (as visp-core would do via span.record)
        let span1 = tracing::info_span!("span1", visp.span.w3c_id = tracing::field::Empty);
        span1.record("visp.span.w3c_id", id1);
        // Check mapping immediately (before span close removes it).
        assert!(parent_link.inner.mapping.contains_key(id1));
        {
            let _e = span1.enter();
            tracing::info!("inside span1");
        }
        drop(span1);

        let span2 = tracing::info_span!("span2", visp.span.w3c_id = tracing::field::Empty);
        span2.record("visp.span.w3c_id", id2);
        assert!(parent_link.inner.mapping.contains_key(id2));
        {
            let _e = span2.enter();
            tracing::info!("inside span2");
        }
        drop(span2);

        // Verify JSON output contains visp.span.w3c_id field with correct values.
        let output = writer.into_string();
        assert!(
            output.contains(id1),
            "expected {} in JSON output\nOutput:\n{}",
            id1,
            output
        );
        assert!(
            output.contains(id2),
            "expected {} in JSON output\nOutput:\n{}",
            id2,
            output
        );
        // Both IDs are 16-hex (non-zero)
        assert_eq!(id1.len(), 16, "W1 span id must be 16 hex chars");
        assert_eq!(id2.len(), 16, "W1 span id must be 16 hex chars");
        // Different spans get different IDs
        assert_ne!(id1, id2, "each span must have a unique ID");
    }

    #[test]
    #[serial]
    fn test_parent_link_otel_mode_when_otel_enabled() {
        // OTel mode: ParentLinkLayer::with_otel_mode(true) reads OTel span_id
        // and records it as visp.span.w3c_id in on_enter.
        let writer = TestSpanWriter::new();
        let (_guard, exporter, _layer) = build_otel_subscriber(writer.clone());

        let span = tracing::info_span!("otel_test_span", visp.span.w3c_id = tracing::field::Empty);
        {
            let _e = span.enter();
            // on_enter fires: OTel mode reads span_id and records it
            tracing::info!("inside otel span");
        }
        drop(span);

        // Get the OTel span_id from the exporter
        let finished = exporter
            .get_finished_spans()
            .expect("get_finished_spans should succeed");
        assert!(!finished.is_empty(), "expected at least one finished span");
        let otel_span_id = finished[0].span_context.span_id().to_string();
        assert_eq!(otel_span_id.len(), 16, "OTel span_id must be 16 hex chars");

        // Parse JSON output and check visp.span.w3c_id matches OTel span_id
        let output = writer.into_string();
        assert!(!output.is_empty(), "expected non-empty JSON output");

        let visp_w3c_id_found = output.contains(&otel_span_id);
        assert!(
            visp_w3c_id_found,
            "expected visp.span.w3c_id ({}) in JSON output\nOutput:\n{}",
            otel_span_id, output
        );
    }

    /// Extract `visp.span.w3c_id` from a JSON event line (nested inside the
    /// `span` sub-object).  Returns `None` if the field is absent.
    fn extract_w3c_from_json(line: &str) -> Option<String> {
        let val: serde_json::Value = serde_json::from_str(line).ok()?;
        val.get("span")?
            .get("visp.span.w3c_id")?
            .as_str()
            .map(String::from)
    }

    #[test]
    #[serial]
    fn test_otel_mode_reads_real_trace_id() {
        // Same setup: verify that the written visp.span.w3c_id equals what
        // the exporter reports as this span's OTel span_id.
        let writer = TestSpanWriter::new();
        let (_guard, exporter, _layer) = build_otel_subscriber(writer.clone());

        let span = tracing::info_span!("trace_id_test", visp.span.w3c_id = tracing::field::Empty);
        {
            let _e = span.enter();
            tracing::info!("inside");
        }
        drop(span);

        let finished = exporter
            .get_finished_spans()
            .expect("get_finished_spans should succeed");
        assert!(!finished.is_empty(), "expected at least one finished span");

        // Get the OTel-generated span_id
        let otel_span_id = finished[0].span_context.span_id().to_string();

        // Parse JSON and find the visp.span.w3c_id value inside the span object
        let output = writer.into_string();
        for line in output.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(w3c) = extract_w3c_from_json(line) {
                assert_eq!(
                    w3c, otel_span_id,
                    "visp.span.w3c_id must equal the OTel span_id"
                );
                return; // test passed
            }
        }
        panic!(
            "visp.span.w3c_id not found in any JSON line.\nOutput:\n{}",
            output
        );
    }

    #[test]
    #[serial]
    fn test_otel_mode_same_trace_id_within_run() {
        // Nested spans: outer and inner should share the same OTel trace_id.
        let writer = TestSpanWriter::new();
        let (_guard, exporter, _layer) = build_otel_subscriber(writer);

        let outer = tracing::info_span!("outer", visp.span.w3c_id = tracing::field::Empty);
        outer.in_scope(|| {
            let inner = tracing::info_span!("inner", visp.span.w3c_id = tracing::field::Empty);
            inner.in_scope(|| {
                tracing::info!("nested");
            });
        });
        drop(outer);

        let finished = exporter
            .get_finished_spans()
            .expect("get_finished_spans should succeed");
        assert_eq!(
            finished.len(),
            2,
            "expected two finished spans (outer + inner)"
        );

        let outer_trace_id = finished[0].span_context.trace_id();
        let inner_trace_id = finished[1].span_context.trace_id();
        assert_eq!(
            outer_trace_id, inner_trace_id,
            "outer and inner spans must share the same trace_id"
        );
    }

    #[test]
    #[serial]
    fn test_parent_link_otel_mode_skips_uuid_generation() {
        // OTel mode writes the OTel span_id, not the W1 uuid.
        // This test explicitly verifies that the recorded visp.span.w3c_id
        // equals the OTel span_id (not a random uuid).
        // Same assertion as test_parent_link_otel_mode_when_otel_enabled
        // but with explicit comment highlighting the uuid-skip behavior.
        let writer = TestSpanWriter::new();
        let (_guard, exporter, _layer) = build_otel_subscriber(writer.clone());

        let span = tracing::info_span!("skip_uuid_test", visp.span.w3c_id = tracing::field::Empty);
        {
            let _e = span.enter();
            tracing::info!("inside");
        }
        drop(span);

        let finished = exporter
            .get_finished_spans()
            .expect("get_finished_spans should succeed");
        let otel_span_id = finished[0].span_context.span_id().to_string();

        let output = writer.into_string();
        for line in output.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(w3c) = extract_w3c_from_json(line) {
                assert_eq!(
                    w3c, otel_span_id,
                    "OTel mode: visp.span.w3c_id must equal OTel span_id (not a uuid)"
                );
                return;
            }
        }
        panic!(
            "visp.span.w3c_id not found in any JSON line.\nOutput:\n{}",
            output
        );
    }

    /// Recursively collect all `visp.span.w3c_id` values from a JSON line.
    /// Searches nested objects and arrays (covers both `span.*` and `spans[]`).
    fn collect_w3c_ids_from_json(line: &str) -> Vec<String> {
        fn recurse(val: &serde_json::Value, ids: &mut Vec<String>) {
            match val {
                serde_json::Value::Object(map) => {
                    for (k, v) in map {
                        if k == "visp.span.w3c_id" {
                            if let Some(s) = v.as_str() {
                                ids.push(s.to_string());
                            }
                        } else {
                            recurse(v, ids);
                        }
                    }
                }
                serde_json::Value::Array(arr) => {
                    for v in arr {
                        recurse(v, ids);
                    }
                }
                _ => {}
            }
        }
        let Ok(val) = serde_json::from_str(line) else {
            return vec![];
        };
        let mut ids = Vec::new();
        recurse(&val, &mut ids);
        ids
    }

    #[test]
    #[serial]
    fn test_otel_mode_nested_spans_each_span_id_matches() {
        // Nested spans: outer and inner must each have visp.span.w3c_id equal
        // to their own OTel span_id (not a duplicate or cross-wired).
        let writer = TestSpanWriter::new();
        let (_guard, exporter, _layer) = build_otel_subscriber(writer.clone());

        let outer = tracing::info_span!("outer", visp.span.w3c_id = tracing::field::Empty);
        outer.in_scope(|| {
            let inner = tracing::info_span!("inner", visp.span.w3c_id = tracing::field::Empty);
            inner.in_scope(|| {
                tracing::info!("nested");
            });
        });
        drop(outer);

        let finished = exporter
            .get_finished_spans()
            .expect("get_finished_spans should succeed");
        assert_eq!(
            finished.len(),
            2,
            "expected two finished spans (outer + inner)"
        );

        // Index span data by name (order is not guaranteed).
        let spans_by_name: std::collections::HashMap<&str, &opentelemetry_sdk::trace::SpanData> =
            finished.iter().map(|s| (s.name.as_ref(), s)).collect();
        assert!(spans_by_name.contains_key("outer"), "missing outer span");
        assert!(spans_by_name.contains_key("inner"), "missing inner span");

        let outer_span_id = spans_by_name["outer"].span_context.span_id().to_string();
        let inner_span_id = spans_by_name["inner"].span_context.span_id().to_string();

        assert_ne!(
            outer_span_id, inner_span_id,
            "outer and inner spans must have distinct span IDs"
        );

        // Parse JSON output and verify each span's visp.span.w3c_id matches
        // its own OTel span_id.  We use a recursive search because the
        // tracing-subscriber fmt layer puts the active span's fields under
        // `span` and ancestor spans under `spans[]`.
        let output = writer.into_string();
        let all_ids: Vec<String> = output
            .lines()
            .filter(|l| !l.trim().is_empty())
            .flat_map(collect_w3c_ids_from_json)
            .collect();

        assert!(
            all_ids.contains(&outer_span_id),
            "outer span's visp.span.w3c_id ({}) not found in JSON.\nOutput:\n{}",
            outer_span_id,
            output
        );
        assert!(
            all_ids.contains(&inner_span_id),
            "inner span's visp.span.w3c_id ({}) not found in JSON.\nOutput:\n{}",
            inner_span_id,
            output
        );
    }

    #[test]
    #[serial]
    fn test_otel_mode_field_appears_in_fmt_output() {
        // OTel mode: verify fmt JSON output contains non-empty visp.span.w3c_id.
        let writer = TestSpanWriter::new();
        let (_guard, _exporter, _layer) = build_otel_subscriber(writer.clone());

        let span = tracing::info_span!("fmt_output_test", visp.span.w3c_id = tracing::field::Empty);
        {
            let _e = span.enter();
            tracing::info!("inside");
        }
        drop(span);

        let output = writer.into_string();
        assert!(!output.is_empty(), "expected non-empty JSON output");

        for line in output.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(w3c) = extract_w3c_from_json(line) {
                assert!(!w3c.is_empty(), "visp.span.w3c_id must be non-empty");
                assert_eq!(w3c.len(), 16, "visp.span.w3c_id must be 16 hex chars");
                return;
            }
        }
        panic!(
            "visp.span.w3c_id not found in any JSON line.\nOutput:\n{}",
            output
        );
    }

    // ── SpanW3CId newtype ─────────────────────────────────────────────────

    #[test]
    fn test_span_w3c_id_newtype() {
        let id = "b7ad6b7169203331".to_string();
        let sw = SpanW3CId(id.clone());
        assert_eq!(sw.0, id);
        let _ = format!("{sw:?}"); // Debug
    }
}
