//! Minimal synchronous HTTP/HTTPS client: POST a JSON body, read one
//! Content-Length / Connection:close response. HTTPS via rustls (ring backend);
//! plain HTTP via a bare TcpStream (for local upstreams like Ollama).
//!
//! `rustls::crypto::ring::default_provider().install_default()` MUST have been
//! called once at startup before `post_json` is used over https.

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};

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

fn parse_response(raw: &[u8]) -> io::Result<(u16, String)> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..sep]);
    let body = String::from_utf8_lossy(&raw[sep + 4..]).into_owned();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad status line"))?;
    Ok((status, body))
}

pub fn post_json(
    scheme: &str,
    host: &str,
    port: u16,
    path: &str,
    bearer: Option<&str>,
    body: &str,
) -> io::Result<(u16, String)> {
    let req = request_bytes(host, port, path, bearer, body);
    let mut raw = Vec::new();
    if scheme == "https" {
        let tcp = TcpStream::connect((host, port))?;
        let name = ServerName::try_from(host.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let conn = ClientConnection::new(tls_config(), name).map_err(io::Error::other)?;
        let mut tls = StreamOwned::new(conn, tcp);
        tls.write_all(req.as_bytes())?;
        tls.flush()?;
        tls.read_to_end(&mut raw)?;
    } else {
        let mut sock = TcpStream::connect((host, port))?;
        sock.write_all(req.as_bytes())?;
        sock.flush()?;
        sock.read_to_end(&mut raw)?;
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
}
