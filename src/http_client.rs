//! Minimal synchronous HTTP/HTTPS client: POST a JSON body and read one
//! response (Content-Length, chunked, or Connection:close delimited). HTTPS via
//! rustls (ring backend); plain HTTP via a bare TcpStream (for local upstreams
//! like Ollama).
//!
//! All socket operations are bounded by the caller-supplied `timeout` so a
//! slow/hung upstream surfaces as an error and the fallback chain advances
//! instead of wedging a worker thread.
//!
//! `rustls::crypto::ring::default_provider().install_default()` MUST have been
//! called once at startup before `post_json` is used over https.

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

fn tls_config() -> Arc<ClientConfig> {
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    })
    .clone()
}

fn request_bytes(host: &str, port: u16, path: &str, bearer: Option<&str>, body: &str) -> String {
    let auth = bearer
        .map(|b| format!("Authorization: Bearer {b}\r\n"))
        .unwrap_or_default();
    format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Connect with a bounded connect timeout and apply read/write timeouts so no
/// socket operation can block indefinitely. A zero `timeout` means "no
/// read/write deadline" (connect is still clamped to a sane window).
fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("cannot resolve {host}:{port}"),
        )
    })?;
    // connect_timeout rejects a zero duration; clamp to a sane window.
    let connect_to = timeout.clamp(Duration::from_secs(1), Duration::from_secs(15));
    let tcp = TcpStream::connect_timeout(&addr, connect_to)?;
    let rw = if timeout.is_zero() {
        None
    } else {
        Some(timeout)
    };
    tcp.set_read_timeout(rw)?;
    tcp.set_write_timeout(rw)?;
    Ok(tcp)
}

/// Read to EOF, tolerating an unclean close: TLS peers and some HTTP servers
/// drop the connection without a close_notify / clean FIN once the body is
/// sent (rustls 0.23 surfaces this as `UnexpectedEof`). A genuine stall instead
/// surfaces as `WouldBlock`/`TimedOut` and IS propagated.
fn read_body<R: Read>(r: &mut R, buf: &mut Vec<u8>) -> io::Result<()> {
    match r.read_to_end(buf) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
        Err(e) => Err(e),
    }
}

fn parse_response(raw: &[u8]) -> io::Result<(u16, String)> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..sep]);
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad status line"))?;
    let body_bytes = &raw[sep + 4..];
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked {
        String::from_utf8_lossy(&dechunk(body_bytes)).into_owned()
    } else {
        String::from_utf8_lossy(body_bytes).into_owned()
    };
    Ok((status, body))
}

/// Decode an HTTP/1.1 `Transfer-Encoding: chunked` body. Lenient: stops at the
/// terminating zero-size chunk or when the data is exhausted.
fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(nl) = data.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let size_line = &data[..nl];
        let hex = size_line.split(|&b| b == b';').next().unwrap_or(size_line);
        let size = usize::from_str_radix(String::from_utf8_lossy(hex).trim(), 16).unwrap_or(0);
        data = &data[nl + 2..];
        if size == 0 {
            break;
        }
        let take = size.min(data.len());
        out.extend_from_slice(&data[..take]);
        data = &data[take..];
        if data.starts_with(b"\r\n") {
            data = &data[2..];
        }
    }
    out
}

pub fn post_json(
    scheme: &str,
    host: &str,
    port: u16,
    path: &str,
    bearer: Option<&str>,
    body: &str,
    timeout: Duration,
) -> io::Result<(u16, String)> {
    let req = request_bytes(host, port, path, bearer, body);
    let mut raw = Vec::new();
    let tcp = connect(host, port, timeout)?;
    if scheme == "https" {
        let name = ServerName::try_from(host.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let conn = ClientConnection::new(tls_config(), name).map_err(io::Error::other)?;
        let mut tls = StreamOwned::new(conn, tcp);
        tls.write_all(req.as_bytes())?;
        tls.flush()?;
        read_body(&mut tls, &mut raw)?;
    } else {
        let mut sock = tcp;
        sock.write_all(req.as_bytes())?;
        sock.flush()?;
        read_body(&mut sock, &mut raw)?;
    }
    parse_response(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_request_with_bearer_and_length() {
        let r = request_bytes("h", 443, "/v1/chat/completions", Some("tok"), "{}");
        assert!(r.contains("POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(r.contains("Host: h:443\r\n"));
        assert!(r.contains("Authorization: Bearer tok\r\n"));
        assert!(r.contains("Content-Length: 2\r\n"));
        assert!(r.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn parses_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"ok\":1}";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"ok\":1}");
    }

    #[test]
    fn dechunks_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "hello world");
    }

    #[test]
    fn dechunk_handles_extensions_and_truncation() {
        // chunk-extension after ';' is ignored; truncated stream stops cleanly.
        assert_eq!(dechunk(b"3;foo=bar\r\nabc\r\n0\r\n\r\n"), b"abc");
        assert_eq!(dechunk(b"4\r\nab"), b"ab");
    }
}
