//! The OTLP exporter actually reaches the endpoint it was configured with.
//!
//! `#115` moved this stack from opentelemetry 0.22 / tonic 0.11 to 0.32 / tonic
//! 0.14, which rewrote how the exporter is built. That is a change unit tests do
//! not cover and that fails *silently*: a misbuilt exporter leaves the process
//! running normally and simply stops emitting, so the first symptom is an empty
//! dashboard nobody is looking at.
//!
//! This binds a real socket and asserts the exporter connects and writes to it.
//! It deliberately does not implement the OTLP gRPC service — the question here
//! is "does the export path still go out over the wire to the configured
//! address", not "is the protobuf well-formed", and a listener answers that
//! without vendoring a collector into the test suite.

use std::io::Read;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use opentelemetry::trace::{Tracer, TracerProvider as _};
use opentelemetry_otlp::WithExportConfig;

/// Bind an ephemeral port and report the first bytes any client sends to it.
fn listening_endpoint() -> (String, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 1024];
            match stream.read(&mut buf) {
                Ok(n) if n > 0 => {
                    let _ = tx.send(buf[..n].to_vec());
                }
                _ => {
                    // Connected but sent nothing — report it as empty rather than
                    // hanging, so the assertion below explains what happened.
                    let _ = tx.send(Vec::new());
                }
            }
        }
    });

    (format!("http://127.0.0.1:{port}"), rx)
}

#[test]
fn the_span_exporter_connects_to_its_configured_endpoint() {
    let (endpoint, rx) = listening_endpoint();

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&endpoint)
            .build()
            .expect("the exporter must build against a plain endpoint");

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .build();

        let tracer = provider.tracer("jamjet-test");
        tracer.in_span("probe", |_| {});

        // Force the batch out rather than waiting on the scheduled interval.
        let _ = provider.force_flush();
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    let bytes = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the exporter never connected to the endpoint it was given");

    assert!(
        !bytes.is_empty(),
        "the exporter connected but sent nothing — the export path is wired to the \
         endpoint yet producing no data"
    );
    // gRPC is HTTP/2, which always opens with this preface. Asserting it (rather
    // than just "some bytes") catches an exporter that connects over the wrong
    // protocol, which would reach a collector and be dropped.
    assert!(
        bytes.starts_with(b"PRI * HTTP/2.0"),
        "expected an HTTP/2 connection preface, got: {:?}",
        String::from_utf8_lossy(&bytes[..bytes.len().min(32)])
    );
}
