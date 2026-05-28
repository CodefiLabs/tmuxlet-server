//! End-to-end coverage for the `api` backend: a mock OpenAI-compatible upstream
//! over real TCP exercises build_body -> http_client::post_json -> extract_content,
//! including the non-2xx fallthrough path.
mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

/// Bind `port` and, on a background thread, answer exactly one HTTP request with
/// the given raw `response`. The listener binds before returning, so the
/// upstream is reachable as soon as this function returns.
fn mock_upstream(port: u16, response: String) -> thread::JoinHandle<()> {
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
        let _ = sock.write_all(response.as_bytes());
        let _ = sock.flush();
    })
}

fn http_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn completion_json(reply: &str) -> String {
    format!(
        r#"{{"id":"mock","object":"chat.completion","choices":[{{"index":0,"message":{{"role":"assistant","content":"{reply}"}},"finish_reason":"stop"}}]}}"#
    )
}

#[test]
fn api_backend_round_trips_through_mock_upstream() {
    let server_port = common::free_port();
    let mock_port = common::free_port();
    let _mock = mock_upstream(
        mock_port,
        http_response(200, "OK", &completion_json("pong-from-mock")),
    );

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

#[test]
fn api_non_2xx_falls_through_to_next_backend() {
    let server_port = common::free_port();
    let mock_port = common::free_port();
    // Upstream returns 500 -> api dispatch must surface Http error and the chain
    // advances to the `echo` cli backend.
    let _mock = mock_upstream(
        mock_port,
        http_response(500, "Internal Server Error", r#"{"error":"boom"}"#),
    );

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

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.viaapi]
order = ["mockapi", "echo"]
"#
    );
    let server = common::start(&cfg);
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"viaapi","messages":[{"role":"user","content":"fallback-marker-99"}]}"#,
    );
    assert_eq!(status, 200, "expected fallthrough to echo: {body}");
    assert!(
        body.contains("fallback-marker-99"),
        "echoed content missing: {body}"
    );
    assert!(
        body.contains(r#""model":"echo""#),
        "served backend should be echo, not the failed api: {body}"
    );
}

/// Mock that echoes the received `Authorization: Bearer <token>` back as the
/// completion content (or "NO-AUTH" if absent).
fn mock_upstream_echo_auth(port: u16) -> thread::JoinHandle<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock upstream");
    thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
        let head = String::from_utf8_lossy(&buf);
        let token = head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
            .and_then(|l| l.split_once(':').map(|x| x.1))
            .map(|v| v.trim().trim_start_matches("Bearer ").trim().to_string())
            .unwrap_or_else(|| "NO-AUTH".to_string());
        let resp = http_response(200, "OK", &completion_json(&token));
        let _ = sock.write_all(resp.as_bytes());
        let _ = sock.flush();
    })
}

#[test]
fn api_backend_sends_bearer_from_api_key_env() {
    let server_port = common::free_port();
    let mock_port = common::free_port();
    let _mock = mock_upstream_echo_auth(mock_port);

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
api_key_env = "TMUXLET_TEST_KEY"

[chains.viaapi]
order = ["mockapi"]
"#
    );
    // api_key_env holds the env var NAME; the value is resolved from the
    // server's captured environment and sent as a bearer token.
    let server = common::start_with_env(&cfg, &[("TMUXLET_TEST_KEY", "secret-token-xyz")]);
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"viaapi","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(
        body.contains("secret-token-xyz"),
        "bearer token from api_key_env was not forwarded: {body}"
    );
}
