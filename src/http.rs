use crate::backend::{Backend, BackendError};
use crate::config::Config;
use crate::env::Env;
use crate::{openai, router};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

pub enum Route {
    Health,
    Models,
    Chat,
    Reserved,
    NotFound,
}

pub fn classify(method: &str, path: &str) -> Route {
    match (method, path) {
        ("GET", "/health") => Route::Health,
        ("GET", "/v1/models") => Route::Models,
        ("POST", "/v1/chat/completions") => Route::Chat,
        _ if path == "/" || path.starts_with("/ui") || path.starts_with("/api") => Route::Reserved,
        _ => Route::NotFound,
    }
}

/// Pull-based SSE body: tiny_http reads frames lazily, so the body is not
/// buffered into a single allocation by the framework.
pub struct SseBody {
    frames: VecDeque<Vec<u8>>,
    cursor: usize,
}

impl SseBody {
    pub fn from_frames(frames: Vec<String>) -> Self {
        SseBody {
            frames: frames.into_iter().map(String::into_bytes).collect(),
            cursor: 0,
        }
    }
}

impl Read for SseBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(front) = self.frames.front() else {
                return Ok(0);
            };
            let rem = &front[self.cursor..];
            if rem.is_empty() {
                self.frames.pop_front();
                self.cursor = 0;
                continue;
            }
            let n = rem.len().min(buf.len());
            buf[..n].copy_from_slice(&rem[..n]);
            self.cursor += n;
            return Ok(n);
        }
    }
}

pub struct State {
    pub cfg: Config,
    pub env: Env,
    /// P-3: runtime backends built once at startup (with F-4 path resolution),
    /// keyed by name; shared read-only across workers.
    pub backends: HashMap<String, Backend>,
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn respond_json(req: Request, status: u16, body: String) {
    let resp = Response::from_string(body)
        .with_header(header("Content-Type", "application/json"))
        .with_status_code(status);
    let _ = req.respond(resp);
}

fn respond_err(req: Request, status: u16, msg: &str, etype: &str, code: Option<&str>) {
    respond_json(
        req,
        status,
        openai::ErrorEnvelope::new(msg, etype, code).to_json(),
    );
}

pub fn handle(mut req: Request, state: &Arc<State>) {
    let method = match req.method() {
        Method::Get => "GET",
        Method::Post => "POST",
        Method::Head => "HEAD",
        _ => "OTHER",
    };
    let path = req.url().split('?').next().unwrap_or("").to_string();
    match classify(method, &path) {
        Route::Health => {
            let body = format!(
                "{{\"status\":\"ok\",\"backends\":{},\"chains\":{}}}",
                state.cfg.backends.len(),
                state.cfg.chains.len()
            );
            respond_json(req, 200, body);
        }
        Route::Models => {
            // U-9: deterministic order — chains first, then backends, each
            // sorted alphabetically (HashMap iteration order changes per run).
            let mut chain_ids: Vec<String> = state.cfg.chains.keys().cloned().collect();
            chain_ids.sort();
            let mut backend_ids: Vec<String> = state.cfg.backends.keys().cloned().collect();
            backend_ids.sort();
            let ids = chain_ids.into_iter().chain(backend_ids);
            respond_json(
                req,
                200,
                serde_json::to_string(&openai::model_list(ids)).unwrap(),
            );
        }
        Route::Reserved => respond_err(
            req,
            501,
            "not implemented in V1",
            "server_error",
            Some("not_implemented"),
        ),
        Route::NotFound => respond_err(
            req,
            404,
            "no such route",
            "invalid_request_error",
            Some("unknown_route"),
        ),
        Route::Chat => {
            // Cap the body read so a malformed/huge payload can't exhaust memory
            // (loopback-only by convention, but bound it regardless).
            const MAX_BODY: u64 = 16 * 1024 * 1024; // 16 MiB
            let mut raw = String::new();
            if req
                .as_reader()
                .take(MAX_BODY + 1)
                .read_to_string(&mut raw)
                .is_err()
            {
                return respond_err(
                    req,
                    400,
                    "could not read body",
                    "invalid_request_error",
                    Some("read_error"),
                );
            }
            if raw.len() as u64 > MAX_BODY {
                return respond_err(
                    req,
                    413,
                    "request body too large",
                    "invalid_request_error",
                    Some("payload_too_large"),
                );
            }
            handle_chat(req, &raw, state);
        }
    }
}

fn handle_chat(req: Request, raw: &str, state: &Arc<State>) {
    // P-2: parse the body once to a Value, then deserialize the typed view from
    // it (the api backend forwards the Value verbatim).
    let raw_value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            return respond_err(
                req,
                400,
                &format!("parse error: {e}"),
                "invalid_request_error",
                Some("parse_error"),
            );
        }
    };
    let parsed: openai::ChatRequest = match openai::ChatRequest::deserialize(&raw_value) {
        Ok(p) => p,
        Err(e) => {
            return respond_err(
                req,
                400,
                &format!("parse error: {e}"),
                "invalid_request_error",
                Some("parse_error"),
            );
        }
    };
    if parsed.messages.is_empty() {
        return respond_err(
            req,
            400,
            "messages must not be empty",
            "invalid_request_error",
            Some("missing_messages"),
        );
    }

    let names = match router::resolve(&parsed.model, &state.cfg) {
        Ok(n) => n,
        Err(e) => return respond_err(req, 400, &e, "invalid_request_error", Some("bad_model")),
    };
    let prompt = openai::flatten_to_prompt(&parsed.messages);
    let default_to = Duration::from_secs(state.cfg.server.request_timeout_secs);

    let mut errors: Vec<BackendError> = Vec::new();
    for &name in &names {
        // P-3: reuse the prebuilt runtime backend instead of rebuilding it.
        let backend = &state.backends[name];
        let timeout = backend.timeout_override().unwrap_or(default_to);
        match backend.dispatch(&prompt, &raw_value, &state.env, timeout) {
            Ok(result) => {
                // U-6: an empty completion ends the chain with a working answer
                // unused; treat it as a failure so the chain advances (unless the
                // backend opts in with allow_empty).
                if result.content.trim().is_empty() && !backend.allow_empty() {
                    let e = BackendError::Backend(name.to_string(), "empty output".into());
                    eprintln!("[skip] {e}");
                    errors.push(e);
                    continue;
                }
                eprintln!("[ok] {name}");
                let id = format!("chatcmpl-{}", result.model_label);
                if parsed.stream {
                    let frames = openai::stream_frames(&id, &result.model_label, &result.content);
                    let resp = Response::new(
                        StatusCode(200),
                        vec![
                            header("Content-Type", "text/event-stream"),
                            header("Cache-Control", "no-cache"),
                        ],
                        SseBody::from_frames(frames),
                        None,
                        None,
                    );
                    let _ = req.respond(resp);
                } else {
                    let body = serde_json::to_string(&openai::build_completion(
                        id,
                        result.model_label,
                        result.content,
                    ))
                    .unwrap();
                    respond_json(req, 200, body);
                }
                return;
            }
            Err(e) => {
                eprintln!("[skip] {e}");
                errors.push(e);
            }
        }
    }
    // U-23: compact human summary in `message` (chat UIs that render only
    // `message` stay debuggable); full per-leg strings in `error.details`.
    let summary = errors
        .iter()
        .map(|e| format!("{} ({})", e.backend_name(), e.class()))
        .collect::<Vec<_>>()
        .join(", ");
    let details: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
    let body = openai::ErrorEnvelope::with_details(
        format!("all backends failed: {summary}"),
        "server_error",
        Some("all_backends_failed"),
        details,
    )
    .to_json();
    respond_json(req, 503, body);
}

pub fn serve(server: Arc<Server>, state: Arc<State>, workers: usize) {
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        handles.push(std::thread::spawn(move || {
            while let Ok(req) = server.recv() {
                // F-3: a panic while handling one request must not tear down the
                // worker (which would silently shrink capacity). Recover and
                // keep serving. The panicking request's socket is dropped.
                let st = &state;
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(req, st)));
                if let Err(p) = result {
                    let msg = p
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into());
                    eprintln!("[error] worker recovered from panic: {msg}");
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_routes() {
        assert!(matches!(classify("GET", "/health"), Route::Health));
        assert!(matches!(classify("GET", "/v1/models"), Route::Models));
        assert!(matches!(
            classify("POST", "/v1/chat/completions"),
            Route::Chat
        ));
        assert!(matches!(classify("GET", "/ui/index.html"), Route::Reserved));
        assert!(matches!(
            classify("GET", "/api/sessions/1"),
            Route::Reserved
        ));
        assert!(matches!(classify("GET", "/"), Route::Reserved));
        assert!(matches!(classify("GET", "/nope"), Route::NotFound));
    }

    #[test]
    fn sse_body_reads_all_frames_then_eof() {
        let mut body = SseBody::from_frames(vec!["data: a\n\n".into(), "data: [DONE]\n\n".into()]);
        let mut s = String::new();
        std::io::Read::read_to_string(&mut body, &mut s).unwrap();
        assert_eq!(s, "data: a\n\ndata: [DONE]\n\n");
    }
}
