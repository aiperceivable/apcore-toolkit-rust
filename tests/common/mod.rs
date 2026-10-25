use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

// Wait without transmitting data, observing client EOF so tests can prove
// cancellation/drop releases actual HTTP work instead of merely returning early.
fn pause_until_closed(stream: &mut TcpStream, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        match stream.read(&mut [0; 1024]) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return true,
        }
    }
    false
}

// Shared real TCP fixture: optional header stall, stalled/trickling JSON body,
// and separate request-started/client-closed observations. Accept/read waits and
// all worker lifetimes are bounded even when a regression prevents any request.
pub fn slow_server(
    status: u16,
    drip: bool,
    delay_headers: bool,
) -> (
    String,
    mpsc::Receiver<()>,
    mpsc::Receiver<()>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (started, receiver) = mpsc::channel();
    let (closed, closed_receiver) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let limit = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= limit {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream.set_nodelay(true).unwrap();
        let _ = stream.read(&mut [0u8; 4096]);
        stream.set_nonblocking(true).unwrap();
        if delay_headers {
            let _ = started.send(());
            if pause_until_closed(&mut stream, Duration::from_millis(400)) {
                let _ = closed.send(());
                return;
            }
        }
        let headers = format!("HTTP/1.1 {status} test\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n");
        if stream.write_all(headers.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
        if !delay_headers {
            let _ = started.send(());
        }
        if drip {
            for byte in b"{\"ok\":true}" {
                if stream.write_all(&[*byte]).is_err() {
                    return;
                }
                let _ = stream.flush();
                if pause_until_closed(&mut stream, Duration::from_millis(35)) {
                    let _ = closed.send(());
                    return;
                }
            }
        } else {
            if pause_until_closed(&mut stream, Duration::from_millis(400)) {
                let _ = closed.send(());
                return;
            }
            let _ = stream.write_all(b"{\"ok\":true}");
        }
    });
    (base, receiver, closed_receiver, thread)
}
