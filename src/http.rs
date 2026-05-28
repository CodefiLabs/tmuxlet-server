use crate::backend::Backend;
use crate::config::{self, Config};
use crate::env::Env;
use crate::{openai, router};
use std::collections::VecDeque;
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
            let ids = state
                .cfg
                .chains
                .keys()
                .chain(state.cfg.backends.keys())
                .cloned();
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
            let mut raw = String::new();
            if req.as_reader().read_to_string(&mut raw).is_err() {
                return respond_err(
                    req,
                    400,
                    "could not read body",
                    "invalid_request_error",
                    Some("read_error"),
                );
            }
            handle_chat(req, &raw, state);
        }
    }
}

fn handle_chat(req: Request, raw: &str, state: &Arc<State>) {
    let parsed: openai::ChatRequest = match serde_json::from_str(raw) {
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
    let raw_value: serde_json::Value = serde_json::from_str(raw).unwrap_or(serde_json::json!({}));
    let default_to = Duration::from_secs(state.cfg.server.request_timeout_secs);

    let mut errors = Vec::new();
    for &name in &names {
        let cfg_backend = &state.cfg.backends[name];
        let timeout = match cfg_backend {
            config::Backend::Api {
                timeout_secs: Some(t),
                ..
            } => Duration::from_secs(*t),
            _ => default_to,
        };
        let backend = Backend::from_config(name, cfg_backend);
        match backend.dispatch(&prompt, &raw_value, &state.env, timeout) {
            Ok(result) => {
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
                errors.push(e.to_string());
            }
        }
    }
    let detail = serde_json::to_string(&errors).unwrap_or_default();
    respond_err(
        req,
        503,
        &format!("all backends failed: {detail}"),
        "server_error",
        Some("all_backends_failed"),
    );
}

pub fn serve(server: Arc<Server>, state: Arc<State>, workers: usize) {
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        handles.push(std::thread::spawn(move || {
            while let Ok(req) = server.recv() {
                handle(req, &state);
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
