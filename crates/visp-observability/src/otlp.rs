//! OTel tracer provider construction for visp-daemon.
//!
//! Provides `build_tracer_provider` (production gRPC-OTLP), a test-friendly
//! `build_tracer_provider_with_exporter`, and resource construction.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider, SpanExporter};

use visp_config::OtlpConfig;

/// Build a production OTLP tracer provider via gRPC-tonic.
///
/// When `sample_rate` is 0.0, the provider is built with an AlwaysOff sampler
/// and no span processor — skipping gRPC exporter construction entirely.
/// This avoids an unnecessary tonic channel and the unsafe env-var mutation
/// for headers when the user has explicitly disabled sampling.
pub(crate) fn build_tracer_provider(cfg: &OtlpConfig) -> SdkTracerProvider {
    // Fast-path: sample_rate=0.0 means no spans will ever be recorded.
    // Skip gRPC exporter setup entirely — no env var mutation, no tonic channel.
    if cfg.sample_rate <= 0.0 {
        return SdkTracerProvider::builder()
            .with_sampler(Sampler::AlwaysOff)
            .with_resource(build_resource())
            .build();
    }

    use opentelemetry_otlp::WithExportConfig;

    // Headers: set via env var so the tonic exporter picks them up.
    // This avoids direct tonic MetadataMap manipulation which would require
    // aligning tonic versions between our workspace (0.13) and
    // opentelemetry-otlp's internal tonic (0.14).
    if !cfg.headers.is_empty() {
        let header_str = cfg
            .headers
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join(",");
        // Safety: thread-local side effect only affects the OTLP exporter
        // construction on this thread; no other code reads this var.
        unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", header_str) };
    }

    let exporter = if uses_http_protocol(&cfg.protocol) {
        opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(&cfg.endpoint)
            .with_timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .expect("failed to build OTLP HTTP span exporter")
    } else {
        opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&cfg.endpoint)
            .with_timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .expect("failed to build OTLP gRPC span exporter")
    };

    let processor = opentelemetry_sdk::trace::BatchSpanProcessor::builder(exporter).build();

    SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            cfg.sample_rate,
        ))))
        .with_resource(build_resource())
        .build()
}

/// Resolve whether the configured OTLP protocol uses HTTP transport.
/// Protocol matching is case-insensitive ("http"/"HTTP"/"Http" all match);
/// anything else (including the default "grpc" and unknown values) falls
/// back to gRPC for backwards compatibility.
fn uses_http_protocol(protocol: &str) -> bool {
    protocol.eq_ignore_ascii_case("http")
}

/// Build a tracer provider with a caller-supplied exporter (e.g., InMemorySpanExporter).
/// Uses SimpleSpanProcessor for synchronous export (test friendliness).
#[allow(dead_code)]
pub(crate) fn build_tracer_provider_with_exporter<E>(
    exporter: E,
    cfg: &OtlpConfig,
) -> SdkTracerProvider
where
    E: SpanExporter + 'static,
{
    let processor = opentelemetry_sdk::trace::SimpleSpanProcessor::new(exporter);

    SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            cfg.sample_rate,
        ))))
        .with_resource(build_resource())
        .build()
}

/// Build the OTel Resource with service identity and host metadata.
pub(crate) fn build_resource() -> Resource {
    Resource::builder()
        .with_attributes([
            KeyValue::new("service.name", "visp-daemon"),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new(
                "host.name",
                std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()),
            ),
            KeyValue::new("process.pid", std::process::id() as i64),
        ])
        .build()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{Span, Tracer, TracerProvider};
    use opentelemetry_sdk::trace::InMemorySpanExporter;

    #[test]
    fn test_sample_rate_zero_no_spans_exported() {
        // sample_rate=0.0 → all spans discarded at the SDK sampler level.
        // Verifies that zero-sampled spans never reach the exporter.
        let cfg = OtlpConfig {
            sample_rate: 0.0,
            ..Default::default()
        };
        let exporter = InMemorySpanExporter::default();
        let provider = build_tracer_provider_with_exporter(exporter.clone(), &cfg);
        let tracer = provider.tracer("test");

        {
            let _span = tracer.start("zero_sampled_span");
        }

        let _ = provider.force_flush();
        let exported = exporter
            .get_finished_spans()
            .expect("get_finished_spans should succeed");
        assert!(
            exported.is_empty(),
            "sample_rate=0.0 should export 0 spans, got {}",
            exported.len()
        );
    }

    #[test]
    fn test_sample_rate_zero_build_tracer_provider_fast_path_no_panic() {
        // Fast-path: build_tracer_provider with sample_rate=0.0 must
        // NOT attempt gRPC connection and must NOT panic.
        // (Internally requires opentelemetry_otlp as dep but fast-path skips it.)
        let cfg = OtlpConfig {
            sample_rate: 0.0,
            ..Default::default()
        };
        let provider = build_tracer_provider(&cfg);
        let tracer = provider.tracer("test");
        let span = tracer.start("fast_path_span");
        assert!(
            !span.is_recording(),
            "AlwaysOff sampler should prevent recording"
        );
    }

    #[tokio::test]
    async fn test_protocol_http_builds_provider() {
        // protocol="http" must select the HTTP exporter and build without panic.
        let cfg = OtlpConfig {
            protocol: "http".into(),
            sample_rate: 1.0,
            ..Default::default()
        };
        let provider = build_tracer_provider(&cfg);
        let tracer = provider.tracer("test");
        let span = tracer.start("http_protocol_span");
        assert!(
            span.is_recording(),
            "sample_rate=1.0 should record spans via HTTP exporter"
        );
    }

    #[tokio::test]
    async fn test_protocol_grpc_builds_provider() {
        // protocol="grpc" (the default) must keep using the gRPC exporter.
        let cfg = OtlpConfig {
            protocol: "grpc".into(),
            sample_rate: 1.0,
            ..Default::default()
        };
        let provider = build_tracer_provider(&cfg);
        let tracer = provider.tracer("test");
        let span = tracer.start("grpc_protocol_span");
        assert!(
            span.is_recording(),
            "sample_rate=1.0 should record spans via gRPC exporter"
        );
    }

    #[test]
    fn test_protocol_resolution_case_insensitive_with_grpc_fallback() {
        // Case-insensitive: HTTP/Http/http all resolve to the HTTP transport.
        for p in ["http", "HTTP", "Http"] {
            assert!(
                super::uses_http_protocol(p),
                "protocol {p:?} should resolve to HTTP"
            );
        }
        // grpc and any unknown value fall back to gRPC (backwards compatible).
        for p in ["grpc", "GRPC", "h2c", "unknown", ""] {
            assert!(
                !super::uses_http_protocol(p),
                "protocol {p:?} should NOT resolve to HTTP (fallback to gRPC)"
            );
        }
    }

    #[test]
    fn test_build_resource_has_service_name() {
        let resource = build_resource();
        let attrs: Vec<_> = resource
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        assert!(
            attrs
                .iter()
                .any(|(k, v)| k == "service.name" && v == "visp-daemon"),
            "expected service.name='visp-daemon' in resource, got: {:?}",
            attrs
        );
        assert!(
            attrs
                .iter()
                .any(|(k, v)| k == "service.version" && v == env!("CARGO_PKG_VERSION")),
            "expected service.version='{}' in resource, got: {:?}",
            env!("CARGO_PKG_VERSION"),
            attrs
        );
        assert!(
            attrs.iter().any(|(k, _)| k == "host.name"),
            "expected host.name in resource, got: {:?}",
            attrs
        );
        assert!(
            attrs.iter().any(|(k, _)| k == "process.pid"),
            "expected process.pid in resource, got: {:?}",
            attrs
        );
    }
}
