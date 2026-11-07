//! E2E integration tests for OTLP export failure scenarios.
//!
//! Verifies that the daemon's observability stack handles OTLP collector
//! being unreachable without panicking or blocking the main flow.
//!
//! Refs: design §D3, plan §Step 6.

use std::time::Instant;

use serial_test::serial;

use visp_config::{LangfuseConfig, ObservabilityConfig, OtlpConfig};
use visp_observability::init::init_observability;

// ---------------------------------------------------------------------------
// D3: OTLP collector unreachable — graceful degradation
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn test_otlp_e2e_export_failure_graceful() {
    // OTLP gRPC-tonic exporter needs a Tokio runtime during construction
    // (BatchSpanProcessor background worker).  Create one and keep it alive.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _guard_rt = rt.enter();

    // ── Arrange ──────────────────────────────────────────────────────────
    // Use a real OTLP gRPC-tonic exporter pointed at an unreachable endpoint.
    // Set a short timeout so the test doesn't wait forever.
    let cfg = ObservabilityConfig {
        enabled: true,
        level: "info".into(),
        format: "json".into(),
        parent_link: false,
        metrics_summary: false,
        log_file: None,
        otlp: OtlpConfig {
            enabled: true,
            endpoint: "http://127.0.0.1:1".into(),
            timeout_secs: 1, // fast failure for unreachable endpoint
            ..Default::default()
        },
        langfuse: LangfuseConfig::default(),
    };

    // ── Act: init observability (real exporter path, not test exporter) ──
    // This should NOT panic even though the collector is unreachable.
    let guard = init_observability(&cfg, &cfg.level);
    // If we reach here, init succeeded (no panic from build_tracer_provider).

    // ── Act: emit some spans (simulating main flow) ──────────────────────
    let start = Instant::now();

    // Emit spans with events inside, similar to what the agent loop does.
    let span = tracing::info_span!("test.dummy", test.field = "value1", test.num = 42u64,);
    {
        let _enter = span.enter();
        tracing::info!("dummy event 1");
        tracing::info!("dummy event 2");
    }
    drop(span);

    // Another span via in_scope.
    tracing::info_span!("test.dummy2").in_scope(|| {
        tracing::info!("dummy event 3");
    });

    let elapsed = start.elapsed();
    assert!(
        elapsed.as_secs() < 10,
        "main flow should complete within 10s, took {}s",
        elapsed.as_secs()
    );

    // ── Act: drop guard — should not panic ──────────────────────────────
    // BatchSpanProcessor.shutdown will attempt to flush to the unreachable
    // endpoint; it should fail gracefully (within the configured timeout).
    drop(guard);

    // Drop the runtime guard explicitly (keeps order clear).
    drop(_guard_rt);
    drop(rt);

    // If we reach here without panic, the test passes.
}
