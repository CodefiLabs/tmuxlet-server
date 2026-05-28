//! End-to-end coverage for the `api` backend: a mock OpenAI-compatible upstream
//! over real TCP exercises build_body -> http_client::post_json -> extract_content.
mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

/// Bind `port` and, on a background thread, answer exactly one HTTP request with
/// a canned chat-completion whose content is `reply`. The listener binds before
/// returning, so the upstream is reachable as soon as this function returns.
fn spawn_mock_upstream(port: u16, reply: &str) -> thread::JoinHandle<()> {
    let body = format!(
        r#"{{"id":"mock","object":"chat.completion","choices":[{{"index":0,"message":{{"role":"assistant","content":"{reply}"}},"finish_reason":"stop"}}]}}"#
    );
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock upstream");
    thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        // Read headers, then drain the Content-Length body so the client's write
        // completes cleanly before we respond and close.
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let mut header_end = None;
        while header_end.is_none() {
            match sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    header_end = buf.windows(4).position(|w| w == b"\r\n\r\n");
                }
                Err(_) => break,
            }
        }
        if let Some(he) = header_end {
            let head = String::from_utf8_lossy(&buf[..he]).to_ascii_lowercase();
            let clen = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut remaining = clen.saturating_sub(buf.len() - (he + 4));
            while remaining > 0 {
                match sock.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => remaining = remaining.saturating_sub(n),
                    Err(_) => break,
                }
            }
        }
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes());
        let _ = sock.flush();
    })
}

#[test]
fn api_backend_round_trips_through_mock_upstream() {
    let server_port = common::free_port();
    let mock_port = common::free_port();
    let _mock = spawn_mock_upstream(mock_port, "pong-from-mock");

    let cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{server_port}"
default_chain = "viaapi"
env_source = "process"

[backends.mockapi]
type = "api"
base_url = "http://127.0.0.1:{mock_port}/v1"
model = "mock-model"

[chains.viaapi]
order = ["mockapi"]
"#
    );
    let server = common::start(&cfg);
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"viaapi","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(
        body.contains("pong-from-mock"),
        "upstream content missing: {body}"
    );
    // The label is the pinned upstream model, not the chain/backend name.
    assert!(
        body.contains(r#""model":"mock-model""#),
        "pinned model label missing: {body}"
    );
}
