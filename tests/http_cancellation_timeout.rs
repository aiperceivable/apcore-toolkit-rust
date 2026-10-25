#![cfg(feature = "http-proxy")]

use std::io::{Read, Write};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use apcore::{cancel::CancelToken, errors::ErrorCode, Context, Registry};
use apcore_toolkit::{output::http_proxy_writer::HTTPProxyRegistryWriter, types::ScannedModule};
use serde_json::json;

fn proxy(base: String, timeout: f64) -> Arc<dyn apcore::Module> {
    proxy_with_auth(base, timeout, None)
}

fn proxy_with_auth(
    base: String,
    timeout: f64,
    auth: Option<Box<dyn Fn() -> std::collections::HashMap<String, String> + Send + Sync>>,
) -> Arc<dyn apcore::Module> {
    let registry = Registry::new();
    let mut scanned = ScannedModule::new(
        "probe.get".into(),
        "probe".into(),
        json!({}),
        json!({}),
        vec![],
        "probe:handler".into(),
    );
    scanned.metadata.insert("http_method".into(), json!("GET"));
    scanned.metadata.insert("url_path".into(), json!("/"));
    let writer = HTTPProxyRegistryWriter::new(base, auth, timeout).unwrap();
    assert!(writer.write(&[scanned], &registry)[0].verified);
    registry.get("probe.get").unwrap().unwrap()
}

mod common;
use common::slow_server;

#[tokio::test]
async fn test_proxy_pre_cancelled_context_sends_no_request() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let module = proxy(format!("http://{}", listener.local_addr().unwrap()), 0.05);
    let token = CancelToken::new();
    token.cancel();
    let mut context = Context::anonymous();
    context.cancel_token = Some(token);
    let error = module.execute(json!({}), &context).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::ExecutionCancelled);
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
}

async fn cancel_during_http(status: u16, delay_headers: bool) {
    let (base, started, closed, server) = slow_server(status, false, delay_headers);
    let module = proxy(base, 2.0);
    let parent = CancelToken::new();
    let mut context = Context::anonymous();
    context.cancel_token = Some(parent.child());
    let cancelling = std::thread::spawn(move || {
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        parent.cancel();
    });
    let result = tokio::time::timeout(
        Duration::from_millis(250),
        module.execute(json!({}), &context),
    )
    .await;
    cancelling.join().unwrap();
    let error = result
        .expect("context cancellation must interrupt HTTP I/O")
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::ExecutionCancelled);
    assert_connection_closed(&closed).await;
    server.join().unwrap();
}

#[tokio::test]
async fn test_proxy_parent_cancellation_interrupts_response_body() {
    cancel_during_http(200, false).await;
}
#[tokio::test]
async fn test_proxy_parent_cancellation_interrupts_error_body() {
    cancel_during_http(500, false).await;
}
#[tokio::test]
async fn test_proxy_parent_cancellation_interrupts_waiting_for_headers() {
    cancel_during_http(200, true).await;
}

#[tokio::test]
async fn test_proxy_timeout_covers_stalled_and_dripping_bodies() {
    for (status, drip) in [(200, false), (200, true), (500, false)] {
        let (base, _started, _closed, server) = slow_server(status, drip, false);
        let module = proxy(base, 0.09);
        let error = tokio::time::timeout(
            Duration::from_millis(250),
            module.execute(json!({}), &Context::anonymous()),
        )
        .await
        .expect("body must remain under the configured request timeout")
        .unwrap_err();
        server.join().unwrap();
        assert!(
            error.message.contains("timed out"),
            "must report the I/O timeout, not JSON or HTTP-status failure: {error}"
        );
    }
}

#[tokio::test]
async fn test_proxy_auth_factory_cancellation_sends_no_request() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let token = CancelToken::new();
    let cancelling = token.clone();
    let module = proxy_with_auth(
        format!("http://{}", listener.local_addr().unwrap()),
        0.05,
        Some(Box::new(move || {
            cancelling.cancel();
            std::collections::HashMap::new()
        })),
    );
    let mut context = Context::anonymous();
    context.cancel_token = Some(token);
    let error = module.execute(json!({}), &context).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::ExecutionCancelled);
    assert!(
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "a factory that cancels its Context must prevent HTTP transmission"
    );
}

#[tokio::test]
async fn test_proxy_preserves_error_response_charset_decoding() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let module = proxy(format!("http://{}", listener.local_addr().unwrap()), 1.0);
    let server = std::thread::spawn(move || {
        let limit = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < limit {
            if let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let _ = stream.read(&mut [0; 4096]);
                let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=iso-8859-1\r\nContent-Length: 4\r\nConnection: close\r\n\r\ncaf\xe9");
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    let error = module
        .execute(json!({}), &Context::anonymous())
        .await
        .unwrap_err();
    server.join().unwrap();
    assert!(
        error.message.contains("café"),
        "HTTP error text must retain reqwest charset decoding: {error}"
    );
}

async fn assert_connection_closed(closed: &mpsc::Receiver<()>) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    loop {
        match closed.try_recv() {
            Ok(()) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                panic!("HTTP work outlived cancellation/drop instead of closing its socket")
            }
            Err(mpsc::TryRecvError::Empty) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the dropped HTTP future must close its socket promptly"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}

#[tokio::test]
async fn test_dropping_proxy_execution_closes_owned_http_work() {
    for with_token in [false, true] {
        let (base, started, closed, server) = slow_server(200, false, false);
        let module = proxy(base, 2.0);
        let mut context = Context::anonymous();
        if with_token {
            context.cancel_token = Some(CancelToken::new());
        }
        let result = tokio::time::timeout(
            Duration::from_millis(80),
            module.execute(json!({}), &context),
        )
        .await;
        assert!(
            result.is_err(),
            "the outer timeout must drop an active HTTP execution"
        );
        started
            .try_recv()
            .expect("the server must have received a real request");
        assert_connection_closed(&closed).await;
        server.join().unwrap();
    }
}

#[tokio::test]
async fn test_openapi_timeout_covers_headers_and_slow_response_bodies() {
    use apcore_toolkit::openapi_scanner::{load_spec_with_options, LoadSpecError, LoadSpecOptions};
    for (drip, delay_headers) in [(false, true), (false, false), (true, false)] {
        let (base, _started, _closed, server) = slow_server(200, drip, delay_headers);
        let options = LoadSpecOptions {
            timeout_secs: 0.08,
            ..Default::default()
        };
        // Client construction can load native TLS roots synchronously on macOS.
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            load_spec_with_options(&base, &options),
        )
        .await;
        server.join().unwrap();
        assert!(
            matches!(outcome.expect("OpenAPI HTTP work must stay bounded"), Err(LoadSpecError::Http(error)) if error.is_timeout()),
            "configured timeout must cover headers and every response-body chunk"
        );
    }
}
