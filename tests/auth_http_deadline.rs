#![cfg(feature = "device-auth")]

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use apcore_toolkit::auth::{DeviceAuthConfig, HttpTransport, RequestBody, ReqwestTransport};

mod common;

fn discovery_server(delay_headers: bool) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let metadata = serde_json::json!({
        "issuer": issuer,
        "device_authorization_endpoint": "https://example.com/device",
        "token_endpoint": "https://example.com/token",
    })
    .to_string();
    let thread = std::thread::spawn(move || {
        let limit = Instant::now() + Duration::from_secs(3);
        let mut workers = Vec::new();
        while workers.len() < 2 && Instant::now() < limit {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let first = workers.is_empty();
                    let body = metadata.clone();
                    workers.push(std::thread::spawn(move || {
                        stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                        let _ = stream.read(&mut [0; 4096]);
                        if first && delay_headers { std::thread::sleep(Duration::from_millis(1500)); }
                        let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        if stream.write_all(headers.as_bytes()).is_err() { return; }
                        let _ = stream.flush();
                        if first && !delay_headers { std::thread::sleep(Duration::from_millis(1500)); }
                        let _ = stream.write_all(body.as_bytes());
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(error) => panic!("accept: {error}"),
            }
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
    (issuer, thread)
}

#[tokio::test]
async fn test_discovery_http_timeout_covers_headers_and_body_and_allows_fallback() {
    for delay_headers in [true, false] {
        let (issuer, server) = discovery_server(delay_headers);
        let mut config = DeviceAuthConfig::new("test");
        config.issuer = Some(issuer);
        config.http_timeout_seconds = Some(0.08);
        // Leave room for native TLS client construction on macOS; the stalled
        // first response itself exceeds this guard if http_timeout is ignored.
        let result = tokio::time::timeout(Duration::from_secs(1), config.discover_reported()).await;
        server.join().unwrap();
        let (_, report) = result
            .expect("discovery must honor its configured HTTP timeout")
            .unwrap();
        assert_eq!(report.candidate_used, Some(2));
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("failed")));
    }
}

#[tokio::test]
async fn test_auth_post_http_timeout_covers_headers_and_complete_body() {
    for (drip, delay_headers) in [(false, true), (false, false), (true, false)] {
        let (base, _started, _closed, server) = common::slow_server(200, drip, delay_headers);
        let transport = ReqwestTransport::with_timeout(Duration::from_millis(80)).unwrap();
        let outcome = tokio::time::timeout(
            Duration::from_millis(250),
            transport.post(
                &base,
                &std::collections::HashMap::new(),
                RequestBody::Form("grant_type=device_code".into()),
            ),
        )
        .await;
        server.join().unwrap();
        assert!(outcome
            .expect(
                "authentication POST must retain its per-request timeout through body consumption"
            )
            .is_err());
    }
}
