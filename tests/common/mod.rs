//! Shared integration-test harness: spawn the built binary against a temp
//! config on an ephemeral port, then talk to it over HTTP.
//!
//! Not every test uses every helper, so the module allows dead code.
#![allow(dead_code)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Grab an ephemeral port by binding to :0, then drop the listener so the
/// server can claim it. (Small TOCTOU window, fine for local/CI tests.)
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A spawned server process; killed on drop so tests never leak children.
pub struct Server {
    child: Child,
    pub base: String,
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout_read(Duration::from_secs(30))
        .build()
}

fn try_get(url: &str) -> Option<(u16, String)> {
    match agent().get(url).call() {
        Ok(resp) => Some((resp.status(), resp.into_string().unwrap_or_default())),
        Err(ureq::Error::Status(code, resp)) => {
            Some((code, resp.into_string().unwrap_or_default()))
        }
        Err(_) => None,
    }
}

/// GET, returning (status, body) for any HTTP status. Panics on transport error.
pub fn get(url: &str) -> (u16, String) {
    try_get(url).unwrap_or_else(|| panic!("GET {url} failed (transport error)"))
}

/// POST a JSON body, returning (status, body) for any HTTP status.
pub fn post_json(url: &str, body: &str) -> (u16, String) {
    match agent()
        .post(url)
        .set("Content-Type", "application/json")
        .send_string(body)
    {
        Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(e) => panic!("POST {url} failed (transport error): {e}"),
    }
}

/// POST returning (status, content_type, body) — for asserting SSE headers.
pub fn post_collect(url: &str, body: &str) -> (u16, String, String) {
    let grab = |resp: ureq::Response| {
        let ct = resp.header("Content-Type").unwrap_or("").to_string();
        (resp.status(), ct, resp.into_string().unwrap_or_default())
    };
    match agent()
        .post(url)
        .set("Content-Type", "application/json")
        .send_string(body)
    {
        Ok(resp) => grab(resp),
        Err(ureq::Error::Status(code, resp)) => {
            let (_, ct, b) = grab(resp);
            (code, ct, b)
        }
        Err(e) => panic!("POST {url} failed (transport error): {e}"),
    }
}

/// Send an arbitrary method (HEAD, DELETE, ...); returns (status, Allow header, body).
pub fn request(method: &str, url: &str) -> (u16, Option<String>, String) {
    let grab = |resp: ureq::Response| {
        let allow = resp.header("Allow").map(|s| s.to_string());
        (resp.status(), allow, resp.into_string().unwrap_or_default())
    };
    match agent().request(method, url).call() {
        Ok(resp) => grab(resp),
        Err(ureq::Error::Status(_code, resp)) => {
            let allow = resp.header("Allow").map(|s| s.to_string());
            (resp.status(), allow, resp.into_string().unwrap_or_default())
        }
        Err(e) => panic!("{method} {url} failed (transport error): {e}"),
    }
}

/// Write `config_toml` to a temp file, spawn the binary against it, and poll
/// `/health` until it answers 200 (or panic after ~5s). `config_toml` MUST
/// contain a `listen = "127.0.0.1:PORT"` line. Test configs should set
/// `env_source = "process"` to skip the ~1s interactive-shell env capture.
pub fn start(config_toml: &str) -> Server {
    start_with_env(config_toml, &[])
}

/// Like `start`, but injects extra environment variables into the spawned
/// server process (visible to backends when `env_source = "process"`).
pub fn start_with_env(config_toml: &str, env: &[(&str, &str)]) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let cfgp = dir.path().join("server.toml");
    std::fs::write(&cfgp, config_toml).unwrap();

    let port = config_toml
        .split("127.0.0.1:")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("config must contain listen = \"127.0.0.1:PORT\"")
        .trim()
        .to_string();
    let base = format!("http://127.0.0.1:{port}");

    let child = Command::new(env!("CARGO_BIN_EXE_tmuxlet-server"))
        .arg("--config")
        .arg(&cfgp)
        .envs(env.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tmuxlet-server");

    let health = format!("{base}/health");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some((200, _)) = try_get(&health) {
            break;
        }
        if Instant::now() >= deadline {
            panic!("server did not become ready at {base} within 5s");
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    Server {
        child,
        base,
        _dir: dir,
    }
}
