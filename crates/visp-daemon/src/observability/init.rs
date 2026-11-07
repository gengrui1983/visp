//! init_observability — subscriber assembly for visp-daemon.
//!
//! Orchestrates EnvFilter, ParentLinkLayer, MetricsLayer, and fmt layer
//! into a single tracing subscriber stack with JSON or pretty output.
//! Refs: design §7.3, plan §Step 5.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

use crate::observability::otlp;
use crate::observability::parent_link::ParentLinkLayer;
use visp_config::ObservabilityConfig;
use visp_metrics::MetricsLayer;

/// Output of [`init_observability`]; held for the lifetime of the program.
#[allow(dead_code)]
pub struct ObservabilityGuard {
    pub metrics: Option<Arc<MetricsLayer>>,
    pub parent_link: Option<Arc<ParentLinkLayer>>,
    /// OTel tracer provider; shut down on drop if set.
    pub tracer_provider: Option<SdkTracerProvider>,
    _file_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
    /// Keeps a thread-local `DefaultGuard` alive when the subscriber was
    /// installed via `set_default` (test helpers).  `None` when the
    /// subscriber was installed globally via `try_init` (production or
    /// `init_global_observability_with_writer`).
    _set_default: Option<tracing::subscriber::DefaultGuard>,
}

/// Check that a configured log directory can actually be written before
/// constructing the rolling appender. `tracing_appender::rolling::daily`
/// panics when its directory is inaccessible, which must not bring down the
/// desktop client during startup.
fn log_dir_is_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".visp-log-write-test-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

/// Build the fmt-layer filter from `log_level`.
///
/// A bare `LevelFilter` would admit the OpenTelemetry SDK's internal DEBUG
/// events (target `opentelemetry_sdk`, e.g. `BatchSpanProcessor.
/// ExportingDueToTimer`, emitted once per scheduled export tick) into the
/// JSON log file whenever `log_level` is `debug`.  Capping that target at
/// WARN keeps the SDK's real diagnostics (export failures, etc.) while
/// silencing the per-tick noise.
fn fmt_layer_filter(log_level: &str) -> EnvFilter {
    let base = LevelFilter::from_str(log_level).unwrap_or(LevelFilter::INFO);
    EnvFilter::new(format!("{base},opentelemetry_sdk=warn"))
}

/// Initialise the tracing subscriber stack from configuration.
///
/// Assembly order: EnvFilter → ParentLinkLayer → MetricsLayer → fmt layer.
/// Layers are always registered (they are lightweight noops when unused);
/// only the guard handles are conditional on config.
///
/// The subscriber is installed globally (`try_init`) so that any
/// `tokio::spawn` task (even on another worker thread) can emit tracing
/// events.  The guard controls OTel tracer-provider shutdown only.
///
/// # Panics
/// Panics only if a previous call already installed a **different** global
/// subscriber (same layers, different identity).  Identical subscribers
/// from the same `tracing_subscriber::registry` do not cause issues.
#[allow(dead_code)]
pub fn init_observability(cfg: &ObservabilityConfig, log_level: &str) -> ObservabilityGuard {
    if !cfg.enabled {
        return ObservabilityGuard {
            metrics: None,
            parent_link: None,
            tracer_provider: None,
            _file_guard: None,
            _set_default: None,
        };
    }

    // 1. Per-layer level filters: fmt layer (file/stdout) uses `log_level`;
    //    OTel export + metrics use `cfg.level` (observability.level).
    let log_filter = fmt_layer_filter(log_level);
    let otel_filter = LevelFilter::from_str(&cfg.level).unwrap_or(LevelFilter::INFO);

    // 2. Always create layers (lightweight noops when unused).
    let parent_link = if cfg.otlp.enabled {
        ParentLinkLayer::with_otel_mode(true)
    } else {
        ParentLinkLayer::new()
    };
    let metrics = MetricsLayer::new();

    // 3. File output (optional).
    let mut _file_guard: Option<tracing_appender::non_blocking::WorkerGuard> = None;
    let mut fmt_writer: Box<dyn Fn() -> Box<dyn std::io::Write + Send> + Send + Sync> =
        Box::new(|| Box::new(std::io::stdout()));
    if let Some(ref path) = cfg.log_file {
        // Expand ~/ to $HOME for the log directory path
        let expanded = visp_config::path::expand_home(path);
        if log_dir_is_writable(&expanded) {
            // `daily` panics internally on an inaccessible directory. The
            // probe above handles the normal case; catch the remaining race
            // so observability can safely fall back to stdout.
            let appender = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tracing_appender::rolling::daily(&expanded, "visp-daemon.log")
            }))
            .ok();
            if let Some(appender) = appender {
                let (nb, guard) = tracing_appender::non_blocking(appender);
                _file_guard = Some(guard);
                fmt_writer =
                    Box::new(move || -> Box<dyn std::io::Write + Send> { Box::new(nb.clone()) });
            } else {
                eprintln!(
                    "visp-daemon log directory became unavailable: {}; using stdout",
                    expanded.display()
                );
            }
        } else {
            eprintln!(
                "visp-daemon log directory is not writable: {}; using stdout",
                expanded.display()
            );
        }
    }

    // 4. Build OTel layer conditionally.
    let tracer_provider: Option<SdkTracerProvider>;
    let otel_layer = if cfg.otlp.enabled {
        let provider = otlp::build_tracer_provider(&cfg.otlp);
        let tracer = provider.tracer("visp-daemon");
        tracer_provider = Some(provider);
        Some(OpenTelemetryLayer::new(tracer).with_context_activation(true))
    } else {
        tracer_provider = None;
        None::<OpenTelemetryLayer<_, _>>
    };

    // 5. Assemble subscriber (json vs pretty produce different concrete types).
    //
    // Assembly order: OTelLayer? → ParentLinkLayer → MetricsLayer → fmt.
    // OTel layer must be outer to ParentLinkLayer (registered first = outer),
    // so ParentLinkLayer.on_enter sees the OTel-fixed SpanContext.
    //
    // Install globally so all threads (including tokio worker threads) see
    // the subscriber.  Use `ok()` to ignore "already set" — in tests the
    // first call wins and subsequent tests silently re-use that subscriber.
    let _ = if cfg.format == "json" {
        tracing_subscriber::registry()
            .with(otel_layer.with_filter(otel_filter))
            .with(parent_link.clone())
            .with(metrics.clone().with_filter(otel_filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(fmt_writer)
                    .with_filter(log_filter),
            )
            .try_init()
    } else {
        tracing_subscriber::registry()
            .with(otel_layer.with_filter(otel_filter))
            .with(parent_link.clone())
            .with(metrics.clone().with_filter(otel_filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .pretty()
                    .with_writer(fmt_writer)
                    .with_filter(log_filter),
            )
            .try_init()
    };

    ObservabilityGuard {
        metrics: cfg.metrics_summary.then(|| Arc::new(metrics)),
        parent_link: cfg.parent_link.then(|| Arc::new(parent_link)),
        tracer_provider,
        _file_guard,
        _set_default: None, // global subscriber, no DefaultGuard needed
    }
}

/// Like [`init_observability`] but accepts a custom [`MakeWriter`] for testing.
///
/// This allows integration tests to capture JSON output in memory without
/// writing to stdout or a file.  All other assembly (EnvFilter, ParentLinkLayer,
/// MetricsLayer) is identical to `init_observability`.
///
/// When `cfg.enabled` is `false`, a no-op guard is returned and the writer
/// is **not** used (mirrors the production behaviour).
///
/// **Note:** This function uses `try_init()` (global subscriber) so that
/// `tokio::spawn` tasks on any tokio worker thread can emit tracing events.
/// Because only one global subscriber can exist per process, repeated calls
/// silently re-use the subscriber from the first call—the returned guard's
/// other fields (metrics, parent_link, tracer_provider) are still valid.
///
/// # Panics
/// Never—`try_init()` errors are silently ignored (expected when the global
/// subscriber was already set by an earlier call).
#[allow(dead_code)]
pub fn init_observability_with_writer<W>(
    cfg: &ObservabilityConfig,
    log_level: &str,
    writer: W,
) -> ObservabilityGuard
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    if !cfg.enabled {
        return ObservabilityGuard {
            metrics: None,
            parent_link: None,
            tracer_provider: None,
            _file_guard: None,
            _set_default: None,
        };
    }

    // Per-layer level filters: fmt layer uses `log_level`, OTel + metrics use cfg.level.
    let log_filter = fmt_layer_filter(log_level);
    let otel_filter = LevelFilter::from_str(&cfg.level).unwrap_or(LevelFilter::INFO);
    let parent_link = if cfg.otlp.enabled {
        ParentLinkLayer::with_otel_mode(true)
    } else {
        ParentLinkLayer::new()
    };
    let metrics = MetricsLayer::new();

    let tracer_provider: Option<SdkTracerProvider>;
    let otel_layer = if cfg.otlp.enabled {
        let provider = otlp::build_tracer_provider(&cfg.otlp);
        let tracer = provider.tracer("visp-daemon");
        tracer_provider = Some(provider);
        Some(OpenTelemetryLayer::new(tracer).with_context_activation(true))
    } else {
        tracer_provider = None;
        None::<OpenTelemetryLayer<_, _>>
    };

    // Install globally so spawned tasks on other threads see the subscriber.
    let _ = if cfg.format == "json" {
        tracing_subscriber::registry()
            .with(otel_layer.with_filter(otel_filter))
            .with(parent_link.clone())
            .with(metrics.clone().with_filter(otel_filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(writer)
                    .with_filter(log_filter),
            )
            .try_init()
    } else {
        tracing_subscriber::registry()
            .with(otel_layer.with_filter(otel_filter))
            .with(parent_link.clone())
            .with(metrics.clone().with_filter(otel_filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .pretty()
                    .with_writer(writer)
                    .with_filter(log_filter),
            )
            .try_init()
    };

    ObservabilityGuard {
        metrics: cfg.metrics_summary.then(|| Arc::new(metrics)),
        parent_link: cfg.parent_link.then(|| Arc::new(parent_link)),
        tracer_provider,
        _file_guard: None,
        _set_default: None, // global subscriber, no DefaultGuard needed
    }
}

impl Drop for ObservabilityGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.tracer_provider.take() {
            let _ = provider.shutdown();
        }
    }
}

/// Test-only entry point that accepts a custom exporter for verifying OTel integration.
///
/// Mirrors [`init_observability_with_writer`] but additionally wires an OTel
/// layer using the given exporter (via [`otlp::build_tracer_provider_with_exporter`]).
/// The exporter is expected to be an `InMemorySpanExporter` in tests.
///
/// Returns a guard whose `tracer_provider` field holds the constructed provider.
///
/// **Note:** Uses `set_default()` (thread-local) to preserve test isolation.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn init_observability_with_exporter<W, E>(
    cfg: &ObservabilityConfig,
    log_level: &str,
    writer: W,
    exporter: E,
) -> ObservabilityGuard
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
    E: opentelemetry_sdk::trace::SpanExporter + 'static,
{
    if !cfg.enabled {
        return ObservabilityGuard {
            metrics: None,
            parent_link: None,
            tracer_provider: None,
            _file_guard: None,
            _set_default: None,
        };
    }

    // Per-layer level filters: fmt layer uses `log_level`, OTel + metrics use cfg.level.
    let log_filter = fmt_layer_filter(log_level);
    let otel_filter = LevelFilter::from_str(&cfg.level).unwrap_or(LevelFilter::INFO);
    let parent_link = if cfg.otlp.enabled {
        ParentLinkLayer::with_otel_mode(true)
    } else {
        ParentLinkLayer::new()
    };
    let metrics = MetricsLayer::new();

    let tracer_provider: Option<SdkTracerProvider>;
    let otel_layer = if cfg.otlp.enabled {
        let provider = otlp::build_tracer_provider_with_exporter(exporter, &cfg.otlp);
        let tracer = provider.tracer("visp-daemon");
        tracer_provider = Some(provider);
        Some(OpenTelemetryLayer::new(tracer).with_context_activation(true))
    } else {
        tracer_provider = None;
        None::<OpenTelemetryLayer<_, _>>
    };

    let set_default = if cfg.format == "json" {
        tracing_subscriber::registry()
            .with(otel_layer.with_filter(otel_filter))
            .with(parent_link.clone())
            .with(metrics.clone().with_filter(otel_filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(writer)
                    .with_filter(log_filter),
            )
            .set_default()
    } else {
        tracing_subscriber::registry()
            .with(otel_layer.with_filter(otel_filter))
            .with(parent_link.clone())
            .with(metrics.clone().with_filter(otel_filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .pretty()
                    .with_writer(writer)
                    .with_filter(log_filter),
            )
            .set_default()
    };

    ObservabilityGuard {
        metrics: cfg.metrics_summary.then(|| Arc::new(metrics)),
        parent_link: cfg.parent_link.then(|| Arc::new(parent_link)),
        tracer_provider,
        _file_guard: None,
        _set_default: Some(set_default),
    }
}

/// Like [`init_observability_with_writer`] but installs the subscriber
/// **globally** via `try_init()` so that `tokio::spawn` tasks on any
/// tokio worker thread can emit tracing events.
///
/// This is the same approach used by [`init_observability`] (production).
/// Use this in tests that exercise a multi-thread tokio runtime.
///
/// Because `try_init()` can succeed only once per process, this function
/// ignores an "already set" error — the subscriber from the first call
/// remains active.
#[cfg(test)]
pub(crate) fn init_global_observability_with_writer<W>(
    cfg: &ObservabilityConfig,
    log_level: &str,
    writer: W,
) -> ObservabilityGuard
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    if !cfg.enabled {
        return ObservabilityGuard {
            metrics: None,
            parent_link: None,
            tracer_provider: None,
            _file_guard: None,
            _set_default: None,
        };
    }

    // Per-layer level filters: fmt layer uses `log_level`, OTel + metrics use cfg.level.
    let log_filter = fmt_layer_filter(log_level);
    let otel_filter = LevelFilter::from_str(&cfg.level).unwrap_or(LevelFilter::INFO);
    let parent_link = if cfg.otlp.enabled {
        ParentLinkLayer::with_otel_mode(true)
    } else {
        ParentLinkLayer::new()
    };
    let metrics = MetricsLayer::new();

    let tracer_provider: Option<SdkTracerProvider>;
    let otel_layer = if cfg.otlp.enabled {
        let provider = otlp::build_tracer_provider(&cfg.otlp);
        let tracer = provider.tracer("visp-daemon");
        tracer_provider = Some(provider);
        Some(OpenTelemetryLayer::new(tracer).with_context_activation(true))
    } else {
        tracer_provider = None;
        None::<OpenTelemetryLayer<_, _>>
    };

    // Install globally — use ok() to ignore "already set" error.
    let _ = if cfg.format == "json" {
        tracing_subscriber::registry()
            .with(otel_layer.with_filter(otel_filter))
            .with(parent_link.clone())
            .with(metrics.clone().with_filter(otel_filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(writer)
                    .with_filter(log_filter),
            )
            .try_init()
    } else {
        tracing_subscriber::registry()
            .with(otel_layer.with_filter(otel_filter))
            .with(parent_link.clone())
            .with(metrics.clone().with_filter(otel_filter))
            .with(
                tracing_subscriber::fmt::layer()
                    .pretty()
                    .with_writer(writer)
                    .with_filter(log_filter),
            )
            .try_init()
    };

    ObservabilityGuard {
        metrics: cfg.metrics_summary.then(|| Arc::new(metrics)),
        parent_link: cfg.parent_link.then(|| Arc::new(parent_link)),
        tracer_provider,
        _file_guard: None,
        _set_default: None, // global subscriber, no DefaultGuard needed
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;
    use std::sync::Mutex;

    use serial_test::serial;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;
    use crate::config::{ObservabilityConfig, OtlpConfig};

    /// In-memory writer that captures bytes via `Arc<Mutex<Vec<u8>>>`.
    #[derive(Clone)]
    struct TestVecWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl TestVecWriter {
        fn new() -> Self {
            Self {
                buf: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn into_string(self) -> String {
            String::from_utf8(self.buf.lock().unwrap().clone()).unwrap_or_default()
        }

        #[allow(dead_code)]
        fn len(&self) -> usize {
            self.buf.lock().unwrap().len()
        }

        #[allow(dead_code)]
        fn content_since(&self, offset: usize) -> String {
            let buf = self.buf.lock().unwrap();
            String::from_utf8(buf[offset..].to_vec()).unwrap_or_default()
        }
    }

    impl<'a> MakeWriter<'a> for TestVecWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl io::Write for TestVecWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buf.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.buf.lock().unwrap().flush()
        }
    }

    /// Build a test subscriber with custom writer (mirrors init_observability
    /// assembly logic but uses the test writer instead of stdout/file).
    ///
    /// Per-layer filters: fmt layer uses `log_level`, metrics layer uses
    /// `cfg.level` (otel filter).  Mirrors the production per-layer setup.
    fn make_test_subscriber(
        writer: TestVecWriter,
        log_level: &str,
        cfg: &ObservabilityConfig,
    ) -> tracing::subscriber::DefaultGuard {
        let log_filter = fmt_layer_filter(log_level);
        let otel_filter = LevelFilter::from_str(&cfg.level).unwrap_or(LevelFilter::INFO);

        let parent_link = ParentLinkLayer::new();
        let metrics = MetricsLayer::new();

        if cfg.format == "json" {
            tracing_subscriber::registry()
                .with(parent_link)
                .with(metrics.with_filter(otel_filter))
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_writer(writer)
                        .with_filter(log_filter),
                )
                .set_default()
        } else {
            tracing_subscriber::registry()
                .with(parent_link)
                .with(metrics.with_filter(otel_filter))
                .with(
                    tracing_subscriber::fmt::layer()
                        .pretty()
                        .with_writer(writer)
                        .with_filter(log_filter),
                )
                .set_default()
        }
    }

    // ------------------------------------------------------------------
    // Global subscriber: spawned task events captured
    // ------------------------------------------------------------------

    /// Verify that `init_global_observability_with_writer` (which uses
    /// `try_init()`) compiles and the function signature is correct.
    ///
    /// The actual spawned-task capture behaviour is verified by the
    /// integration test `test_spawned_task_events_captured_with_global_subscriber`
    /// in `tests/observability_global_subscriber.rs`, which runs in its own
    /// process so `try_init()` succeeds uncontested.
    ///
    /// Run in isolation:  `cargo test -p visp-daemon --test observability_global_subscriber`
    #[test]
    #[serial]
    fn test_init_global_observability_with_writer_function_exists() {
        let w = TestVecWriter::new();
        let cfg = ObservabilityConfig {
            enabled: true,
            format: "json".into(),
            parent_link: false,
            metrics_summary: false,
            log_file: None,
            ..Default::default()
        };
        let guard = init_global_observability_with_writer(&cfg, &cfg.level, w.clone());
        // Guard fields should be populated from config
        assert!(guard.metrics.is_none());
        assert!(guard.parent_link.is_none());
        assert!(guard.tracer_provider.is_none());
    }

    // ------------------------------------------------------------------
    // Red + Green: disabled / noop guard
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_init_observability_disabled_returns_noop_guard() {
        let cfg = ObservabilityConfig {
            enabled: false,
            ..Default::default()
        };
        let guard = init_observability(&cfg, &cfg.level);

        assert!(guard.metrics.is_none());
        assert!(guard.parent_link.is_none());

        // Calling init_observability again with disabled should not panic.
        let guard2 = init_observability(&cfg, &cfg.level);
        assert!(guard2.metrics.is_none());
        assert!(guard2.parent_link.is_none());
    }

    // ------------------------------------------------------------------
    // Red + Green: handles (guard fields)
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_init_observability_returns_metrics_layer_handle() {
        let cfg = ObservabilityConfig {
            enabled: true,
            metrics_summary: true,
            parent_link: false,
            ..Default::default()
        };
        let guard = init_observability(&cfg, &cfg.level);
        assert!(guard.metrics.is_some(), "expected metrics handle");
        assert!(guard.parent_link.is_none());
    }

    #[test]
    #[serial]
    fn test_init_observability_returns_parent_link_handle() {
        let cfg = ObservabilityConfig {
            enabled: true,
            metrics_summary: false,
            parent_link: true,
            ..Default::default()
        };
        let guard = init_observability(&cfg, &cfg.level);
        assert!(guard.parent_link.is_some(), "expected parent_link handle");
        assert!(guard.metrics.is_none());
    }

    #[test]
    #[serial]
    fn test_init_observability_layers_disabled_individually() {
        let cfg = ObservabilityConfig {
            enabled: true,
            parent_link: false,
            metrics_summary: false,
            ..Default::default()
        };
        let guard = init_observability(&cfg, &cfg.level);
        assert!(
            guard.parent_link.is_none(),
            "parent_link should be None when disabled"
        );
        assert!(
            guard.metrics.is_none(),
            "metrics should be None when disabled"
        );
    }

    // ------------------------------------------------------------------
    // Red + Green: JSON format output is valid JSON
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_init_observability_json_format() {
        let writer = TestVecWriter::new();
        let cfg = ObservabilityConfig {
            enabled: true,
            format: "json".into(),
            parent_link: false,
            metrics_summary: false,
            log_file: None,
            ..Default::default()
        };
        let _guard = make_test_subscriber(writer.clone(), "info", &cfg);

        tracing::info!("hello json");

        let output = writer.into_string();
        assert!(!output.is_empty(), "expected non-empty JSON output");

        // Each line must be valid JSON.
        for line in output.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: serde_json::Value =
                serde_json::from_str(line).expect("each output line should be valid JSON");
            assert!(parsed.is_object(), "each JSON line should be an object");
        }
    }

    // ------------------------------------------------------------------
    // Red + Green: pretty format output (weak assertion: >1 line or msg)
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_init_observability_pretty_format() {
        let writer = TestVecWriter::new();
        let cfg = ObservabilityConfig {
            enabled: true,
            format: "pretty".into(),
            parent_link: false,
            metrics_summary: false,
            log_file: None,
            ..Default::default()
        };
        let _guard = make_test_subscriber(writer.clone(), "info", &cfg);

        tracing::info!("hello pretty");

        let output = writer.into_string();
        assert!(!output.is_empty(), "expected non-empty pretty output");
        assert!(
            output.contains("hello pretty") || output.lines().count() > 1,
            "pretty output should contain the message or span multiple lines"
        );
    }

    // ------------------------------------------------------------------
    // Red + Green: EnvFilter level applies
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_init_observability_level_applies_env_filter() {
        let writer = TestVecWriter::new();
        let cfg = ObservabilityConfig {
            enabled: true,
            level: "debug".into(),
            format: "json".into(),
            parent_link: false,
            metrics_summary: false,
            log_file: None,
            ..Default::default()
        };
        // log_level="debug": debug + error events both pass the fmt filter.
        let _guard = make_test_subscriber(writer.clone(), "debug", &cfg);

        tracing::debug!("this is a debug message");
        tracing::error!("this is an error message");

        let output = writer.into_string();
        let line_count = output.lines().filter(|l| !l.trim().is_empty()).count();
        assert!(
            line_count >= 2,
            "expected at least 2 events (debug + error), got {} lines: {:?}",
            line_count,
            output
        );
    }

    // ------------------------------------------------------------------
    // Red + Green: OTel SDK internal debug noise is capped at WARN
    // ------------------------------------------------------------------

    /// The OpenTelemetry SDK emits a DEBUG event with target
    /// `opentelemetry_sdk` (e.g. `BatchSpanProcessor.ExportingDueToTimer`)
    /// on every scheduled export tick.  The fmt-layer filter must cap that
    /// target at WARN so these events never reach the log file, while
    /// app-target debug events still pass.
    #[test]
    #[serial]
    fn test_fmt_filter_caps_opentelemetry_sdk_at_warn() {
        let writer = TestVecWriter::new();
        let cfg = ObservabilityConfig {
            enabled: true,
            format: "json".into(),
            parent_link: false,
            metrics_summary: false,
            log_file: None,
            ..Default::default()
        };
        let _guard = make_test_subscriber(writer.clone(), "debug", &cfg);

        tracing::debug!(
            target: "opentelemetry_sdk",
            name = "BatchSpanProcessor.ExportingDueToTimer",
            "sdk internal noise"
        );
        tracing::debug!(target: "visp_daemon", "app debug stays visible");

        let output = writer.into_string();
        assert!(
            !output.contains("ExportingDueToTimer"),
            "opentelemetry_sdk internal debug event must be filtered out, got: {output}"
        );
        assert!(
            output.contains("app debug stays visible"),
            "app-target debug events must still pass the fmt filter, got: {output}"
        );
    }

    // ------------------------------------------------------------------
    // Red + Green: end-to-end JSON includes trace_id from ParentLinkLayer
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_init_observability_json_includes_trace_id_field_for_subagent_span() {
        let writer = TestVecWriter::new();
        let parent_link_layer = ParentLinkLayer::new();

        let _guard = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new("info"))
            .with(parent_link_layer.clone())
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(writer.clone()),
            )
            .set_default();

        // Create a visp.subagent.spawn span with trace context fields declared
        // and recorded via span field passing (matches orchestrator approach).
        let span = tracing::info_span!(
            "visp.subagent.spawn",
            visp.subagent.name = "test",
            trace_id = tracing::field::Empty,
            parent_span_id = tracing::field::Empty,
        );
        // Record trace context fields (ParentLinkLayer.on_record writes
        // ParentLinkFields into the extension).
        span.record("trace_id", "0af7651916cd43dd8448eb211c80319c");
        span.record("parent_span_id", "aaaaaaaaaaaaaaaa");
        {
            let _enter = span.enter();
            // on_enter triggers ParentLinkLayer to record fields from the
            // ParentLinkFields extension onto the span for JSON output.
            tracing::info!("inside span");
        }
        drop(span);

        let output = writer.into_string();
        assert!(!output.is_empty(), "expected JSON output");

        let trace_id_found = output.contains("trace_id");
        let parent_span_id_found = output.contains("parent_span_id");

        assert!(
            trace_id_found,
            "expected trace_id in JSON output.\nOutput:\n{}",
            output
        );
        assert!(
            parent_span_id_found,
            "expected parent_span_id in JSON output.\nOutput:\n{}",
            output
        );
    }

    // ------------------------------------------------------------------
    // W2-S2-2 红: 测试钩子 — 编译通过即视为通过
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_init_with_exporter_function_exists() {
        // Simply verifying that `init_observability_with_exporter` exists and
        // compiles is sufficient for this test.
        let cfg = ObservabilityConfig {
            enabled: false,
            ..Default::default()
        };
        let writer = TestVecWriter::new();
        let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
        let guard = init_observability_with_exporter(&cfg, &cfg.level, writer, exporter);
        assert!(guard.tracer_provider.is_none());
    }

    // ------------------------------------------------------------------
    // W2-S2-1: OTel 装配链单元测试
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_otlp_disabled_no_otel_layer() {
        let cfg = ObservabilityConfig {
            enabled: true,
            otlp: OtlpConfig {
                enabled: false,
                ..Default::default()
            },
            format: "json".into(),
            parent_link: false,
            metrics_summary: false,
            log_file: None,
            ..Default::default()
        };
        let guard = init_observability(&cfg, &cfg.level);
        assert!(
            guard.tracer_provider.is_none(),
            "tracer_provider should be None when OTel is disabled"
        );
    }

    #[test]
    #[serial]
    fn test_otlp_enabled_attaches_otel_layer() {
        let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
        let writer = TestVecWriter::new();
        let cfg = ObservabilityConfig {
            enabled: true,
            otlp: OtlpConfig {
                enabled: true,
                ..Default::default()
            },
            format: "json".into(),
            parent_link: false,
            metrics_summary: false,
            log_file: None,
            ..Default::default()
        };
        let guard = init_observability_with_exporter(&cfg, &cfg.level, writer, exporter.clone());

        assert!(
            guard.tracer_provider.is_some(),
            "expected tracer_provider when OTel is enabled"
        );

        // Emit a span; with SimpleSpanProcessor it is exported synchronously.
        let span = tracing::info_span!("test_otel_span");
        drop(span);

        let finished = exporter
            .get_finished_spans()
            .expect("get_finished_spans should succeed");
        assert!(
            !finished.is_empty(),
            "expected at least one finished span with OTel enabled"
        );
        assert_eq!(
            finished[0].name.as_ref(),
            "test_otel_span",
            "span name should match"
        );
    }

    #[test]
    #[serial]
    fn test_otlp_resource_has_service_name() {
        let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
        let writer = TestVecWriter::new();
        let cfg = ObservabilityConfig {
            enabled: true,
            otlp: OtlpConfig {
                enabled: true,
                ..Default::default()
            },
            format: "json".into(),
            parent_link: false,
            metrics_summary: false,
            log_file: None,
            ..Default::default()
        };
        let _guard = init_observability_with_exporter(&cfg, &cfg.level, writer, exporter.clone());

        let span = tracing::info_span!("otel_resource_test");
        drop(span);

        let finished = exporter
            .get_finished_spans()
            .expect("get_finished_spans should succeed");
        assert!(!finished.is_empty(), "expected at least one finished span");

        // Verify resource attributes via build_resource() directly,
        // since SpanData in opentelemetry_sdk 0.32 does not carry a `resource`
        // field on individual spans (resource is attached at the exporter level).
        let resource = otlp::build_resource();
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
    }

    #[test]
    #[serial]
    fn test_otlp_w1_layers_still_attached_when_enabled() {
        let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
        let writer = TestVecWriter::new();
        let cfg = ObservabilityConfig {
            enabled: true,
            otlp: OtlpConfig {
                enabled: true,
                ..Default::default()
            },
            format: "json".into(),
            parent_link: true,
            metrics_summary: false, // No metrics layer handle needed
            log_file: None,
            ..Default::default()
        };
        let guard = init_observability_with_exporter(&cfg, &cfg.level, writer.clone(), exporter);

        // W1: ParentLinkLayer handle is still available.
        assert!(
            guard.parent_link.is_some(),
            "parent_link handle should be Some when parent_link=true"
        );

        // Emit a span with visp.span.w3c_id field to exercise ParentLinkLayer.
        let span = tracing::info_span!(
            "visp.subagent.spawn",
            visp.span.w3c_id = tracing::field::Empty,
            trace_id = tracing::field::Empty,
            parent_span_id = tracing::field::Empty,
        );
        span.record("visp.span.w3c_id", "w2_w1_test_12345678");
        span.record("trace_id", "0af7651916cd43dd8448eb211c80319c");
        span.record("parent_span_id", "aaaaaaaaaaaaaaaa");
        {
            let _enter = span.enter();
            tracing::info!("inside W1+OTel span");
        }
        drop(span);

        // Verify fmt JSON output still contains ParentLinkLayer fields.
        let output = writer.into_string();
        assert!(!output.is_empty(), "expected JSON output");

        // visp.span.w3c_id should appear in JSON fmt output
        // (ParentLinkLayer records it onto the span during on_enter).
        assert!(
            output.contains("visp.span.w3c_id"),
            "visp.span.w3c_id should appear in JSON output when ParentLinkLayer is attached.\nOutput:\n{}",
            output
        );
    }

    #[test]
    fn test_log_dir_is_writable_true_for_temp_dir() {
        let dir = std::env::temp_dir().join(format!(
            "visp-log-probe-ok-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            log_dir_is_writable(&dir),
            "a creatable temp dir must be reported writable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_log_dir_is_writable_false_when_path_is_not_a_dir() {
        // A regular file standing where a directory is expected makes
        // create_dir_all fail with ENOTDIR -> must report unwritable
        // instead of letting rolling::daily panic at startup.
        let base = std::env::temp_dir().join(format!(
            "visp-log-probe-bad-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create base temp dir");
        let blocker = base.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("write blocker file");
        assert!(
            !log_dir_is_writable(&blocker.join("nested")),
            "a path under a regular file must be reported unwritable"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
