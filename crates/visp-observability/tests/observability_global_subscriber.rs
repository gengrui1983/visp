//! Integration test verifying that a global subscriber (installed via
//! `try_init()`) captures tracing events from `tokio::spawn` tasks on a
//! multi-threaded tokio runtime.
//!
//! This file is a **separate test binary**, so it runs in its own process
//! and `try_init()` succeeds uncontested.  See also:
//!
//! - `init.rs` unit test `test_init_global_observability_with_writer_function_exists`
//!   (compilation / smoke check).
//!
//! Run: `cargo test -p visp-observability --test observability_global_subscriber`

use std::io;
use std::sync::{Arc, Mutex};

use serial_test::serial;
use tracing_subscriber::fmt::MakeWriter;

use visp_config::ObservabilityConfig;
use visp_observability::init::init_observability_with_writer;

// ---------------------------------------------------------------------------
// In-memory writer
// ---------------------------------------------------------------------------

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

    fn len(&self) -> usize {
        self.buf.lock().unwrap().len()
    }

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

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Verifies that `init_observability_with_writer` (which uses `try_init()`,
/// global subscriber) captures events from a `tokio::spawn` task running
/// on a **different** worker thread.
///
/// This is the exact scenario that was broken with the old thread-local
/// `set_default()`: spawned tasks on other threads could not see the
/// subscriber and their tracing events were silently lost.
///
/// Because this test binary contains only one test, `try_init()` succeeds
/// on the first (and only) call, installing the global subscriber with
/// the test writer.
#[test]
#[serial]
fn test_spawned_task_events_captured_with_global_subscriber() {
    let writer = TestVecWriter::new();

    // Install the subscriber GLOBALLY via try_init().
    // init_observability_with_writer now uses try_init() so spawned
    // tasks on any thread can emit tracing events.
    let cfg = ObservabilityConfig {
        enabled: true,
        format: "json".into(),
        parent_link: false,
        metrics_summary: false,
        log_file: None,
        ..Default::default()
    };
    let _guard = init_observability_with_writer(&cfg, "info", writer.clone());
    // init_observability_with_writer now returns _set_default: None for
    // global; the guard keeps the tracer_provider alive if OTel is active.

    let offset = writer.len();

    // Multi-thread runtime: spawned tasks may run on a different thread.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let handle = tokio::spawn(async {
            tracing::info!("hello from spawned task");
        });
        handle.await.expect("task joined");
    });
    drop(rt);

    let output = writer.content_since(offset);
    assert!(
        output.contains("hello from spawned task"),
        "expected tracing event from spawned task in global subscriber \
         output.\nOutput:\n{}",
        output
    );
}
