use crate::backend::{Backend, BackendError, DispatchResult};
use crate::config::Config;
use crate::env::Env;
use crate::{log, openai, router};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{self, Read};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

pub enum Route {
    Health,
    Models,
    Backends,
    Chat,
    Reserved,
    NotFound,
    /// U-17: a known path reached with the wrong method. Carries the `Allow` value.
    MethodNotAllowed(&'static str),
}

pub fn classify(method: &str, path: &str) -> Route {
    // U-17: HEAD routes like GET; the handler emits headers with an empty body.
    let m = if method == "HEAD" { "GET" } else { method };
    match (m, path) {
        ("GET", "/health") => Route::Health,
        ("GET", "/v1/models") => Route::Models,
        ("GET", "/v1/backends") => Route::Backends,
        ("POST", "/v1/chat/completions") => Route::Chat,
        // U-17: known path, wrong method → 405 with an Allow header.
        (_, "/health") | (_, "/v1/models") | (_, "/v1/backends") => {
            Route::MethodNotAllowed("GET, HEAD")
        }
        (_, "/v1/chat/completions") => Route::MethodNotAllowed("POST"),
        _ if path == "/" || path.starts_with("/ui") || path.starts_with("/api") => Route::Reserved,
        _ => Route::NotFound,
    }
}

/// U-2: channel-backed SSE body. `read` blocks on the channel, so the coordinator
/// thread can trickle `: keepalive` frames while dispatch runs, then push the
/// final frames. EOF when the sender drops.
pub struct ChannelSseBody {
    rx: mpsc::Receiver<Vec<u8>>,
    current: Vec<u8>,
    pos: usize,
}

impl Read for ChannelSseBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.current.len() {
                let n = (self.current.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.rx.recv() {
                Ok(chunk) => {
                    self.current = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
    }
}

/// P-9 / U-13: per-backend health, updated after each dispatch.
#[derive(Default, Clone)]
pub struct BackendHealth {
    pub consecutive_failures: u32,
    pub cooling_until: Option<Instant>,
    pub last_error: Option<String>,
    pub last_latency_ms: Option<u64>,
}

pub struct State {
    pub cfg: Config,
    pub env: Env,
    /// P-3: runtime backends built once at startup (with F-4 path resolution).
    pub backends: HashMap<String, Backend>,
    /// S-1: expected bearer token; None disables auth.
    pub auth_token: Option<String>,
    /// S-6: redact per-backend error detail (true when bound non-loopback).
    pub redact_errors: bool,
    /// P-9 / U-13: per-backend health.
    pub health: Mutex<HashMap<String, BackendHealth>>,
    /// U-20: in-flight dispatch counts per backend.
    pub active: Mutex<HashMap<String, usize>>,
}

/// U-20: RAII guard for a `state.active` concurrency slot. `try_acquire`
/// increments the in-flight count under the backend's cap (returning `None` when
/// already at the cap); `Drop` decrements it. Because the server is built to
/// survive dispatch panics (F-3 `catch_unwind` in `serve`), this guard is what
/// keeps a panic between acquire and release from leaking the slot forever — one
/// leak would brick a `max_concurrent = 1` backend until restart.
struct ActiveSlot<'a> {
    active: &'a Mutex<HashMap<String, usize>>,
    name: String,
}

impl<'a> ActiveSlot<'a> {
    /// Reserve one slot for `name` under `max`. `None` if already at capacity.
    fn try_acquire(
        active: &'a Mutex<HashMap<String, usize>>,
        name: &str,
        max: usize,
    ) -> Option<Self> {
        let mut counts = active.lock().unwrap();
        let count = counts.entry(name.to_string()).or_insert(0);
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(Self {
            active,
            name: name.to_string(),
        })
    }
}

impl Drop for ActiveSlot<'_> {
    fn drop(&mut self) {
        if let Ok(mut counts) = self.active.lock()
            && let Some(c) = counts.get_mut(&self.name)
        {
            *c = c.saturating_sub(1);
        }
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

thread_local! {
    // U-10: the allowed Origin to echo on this request's responses. Set once at
    // the top of handle() (each worker thread serves one request to completion
    // before the next), so every response builder can add the header without
    // threading it through every call site. None = no CORS header.
    static CORS_ORIGIN: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

fn set_cors_origin(o: Option<String>) {
    CORS_ORIGIN.with(|c| *c.borrow_mut() = o);
}

/// U-10: add `Access-Control-Allow-Origin` when the request's Origin is
/// allowlisted. Applied to every response so browsers can read them.
fn apply_cors<R: Read>(resp: Response<R>) -> Response<R> {
    CORS_ORIGIN.with(|c| match c.borrow().as_deref() {
        Some(o) => resp
            .with_header(header("Access-Control-Allow-Origin", o))
            .with_header(header("Vary", "Origin")),
        None => resp,
    })
}

fn respond_json(req: Request, status: u16, body: String) {
    let resp = Response::from_string(body)
        .with_header(header("Content-Type", "application/json"))
        .with_status_code(status);
    let _ = req.respond(apply_cors(resp));
}

fn respond_err(req: Request, status: u16, msg: &str, etype: &str, code: Option<&str>) {
    respond_json(
        req,
        status,
        openai::ErrorEnvelope::new(msg, etype, code).to_json(),
    );
}

fn respond_head(req: Request) {
    // U-17: HEAD mirrors GET headers with an empty body.
    let resp =
        Response::empty(StatusCode(200)).with_header(header("Content-Type", "application/json"));
    let _ = req.respond(apply_cors(resp));
}

fn respond_405(req: Request, allow: &str) {
    // U-17: wrong method on a known path — the Allow header names the fix.
    let body = openai::ErrorEnvelope::new(
        format!("method not allowed on this path; use: {allow}"),
        "invalid_request_error",
        Some("method_not_allowed"),
    )
    .to_json();
    let resp = Response::from_string(body)
        .with_header(header("Content-Type", "application/json"))
        .with_header(header("Allow", allow))
        .with_status_code(405);
    let _ = req.respond(apply_cors(resp));
}

fn backend_type_str(b: &crate::config::Backend) -> &'static str {
    match b {
        crate::config::Backend::Tmuxlet { .. } => "tmuxlet",
        crate::config::Backend::Api { .. } => "api",
        crate::config::Backend::Cli { .. } => "cli",
    }
}

pub fn handle(mut req: Request, state: &Arc<State>) {
    let method = match req.method() {
        Method::Get => "GET",
        Method::Post => "POST",
        Method::Head => "HEAD",
        Method::Options => "OPTIONS",
        _ => "OTHER",
    };
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let auth_header = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str().to_string());
    // U-10: resolve the request Origin against the allowlist and record it for
    // this request's responses (apply_cors reads it via the thread-local).
    let origin = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str().to_string());
    let allowed = origin.filter(|o| state.cfg.server.cors_origins.iter().any(|a| a == o));
    set_cors_origin(allowed.clone());
    if method == "OPTIONS" && !state.cfg.server.cors_origins.is_empty() {
        // CORS preflight: 204, echoing the origin + allowed methods/headers.
        let mut resp = Response::empty(StatusCode(204));
        if let Some(o) = &allowed {
            resp = resp
                .with_header(header("Access-Control-Allow-Origin", o))
                .with_header(header("Access-Control-Allow-Methods", "GET, POST, OPTIONS"))
                .with_header(header(
                    "Access-Control-Allow-Headers",
                    "authorization, content-type",
                ))
                .with_header(header("Access-Control-Max-Age", "600"))
                .with_header(header("Vary", "Origin"));
        }
        let _ = req.respond(resp);
        return;
    }
    let route = classify(method, &path);
    // S-1: gate protected routes; /health stays open for liveness probes.
    if let Some(token) = &state.auth_token
        && matches!(route, Route::Models | Route::Chat | Route::Backends)
        && !crate::auth::check_bearer(auth_header.as_deref(), token)
    {
        log::warn(&format!(
            "401 on {path}: token mismatch — the expected token is in ~/.tmuxlet/token (or the auth_token_env var)"
        ));
        return respond_err(
            req,
            401,
            "missing or invalid bearer token",
            "invalid_request_error",
            Some("unauthorized"),
        );
    }
    // U-17: HEAD carries no body — respond with headers only for GET-family routes.
    if method == "HEAD" && matches!(route, Route::Health | Route::Models | Route::Backends) {
        return respond_head(req);
    }
    match route {
        Route::MethodNotAllowed(allow) => respond_405(req, allow),
        Route::Health => {
            let body = format!(
                "{{\"status\":\"ok\",\"version\":\"{}\",\"backends\":{},\"chains\":{}}}",
                env!("CARGO_PKG_VERSION"),
                state.cfg.backends.len(),
                state.cfg.chains.len()
            );
            respond_json(req, 200, body);
        }
        Route::Models => {
            // U-9: deterministic order — routers + chains first, then backends.
            let mut head_ids: Vec<String> = state
                .cfg
                .routers
                .keys()
                .chain(state.cfg.chains.keys())
                .cloned()
                .collect();
            head_ids.sort();
            let mut backend_ids: Vec<String> = state.cfg.backends.keys().cloned().collect();
            backend_ids.sort();
            let ids = head_ids.into_iter().chain(backend_ids);
            respond_json(
                req,
                200,
                serde_json::to_string(&openai::model_list(ids)).unwrap(),
            );
        }
        Route::Backends => respond_backends(req, state),
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
            // Cap the body read so a malformed/huge payload can't exhaust memory.
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

/// U-13: backend status from the P-9 health records.
fn respond_backends(req: Request, state: &Arc<State>) {
    let mut names: Vec<&String> = state.cfg.backends.keys().collect();
    names.sort();
    let health = state.health.lock().unwrap();
    let now = Instant::now();
    let entries: Vec<serde_json::Value> = names
        .iter()
        .map(|name| {
            let h = health.get(*name);
            let cooling = h.and_then(|x| x.cooling_until).filter(|u| *u > now);
            let st = if cooling.is_some() {
                "cooling"
            } else if h.is_some() {
                "ok"
            } else {
                "unknown"
            };
            serde_json::json!({
                "name": name,
                "type": backend_type_str(&state.cfg.backends[*name]),
                "state": st,
                "consecutive_failures": h.map(|x| x.consecutive_failures).unwrap_or(0),
                "last_error": h.and_then(|x| x.last_error.clone()),
                "last_latency_ms": h.and_then(|x| x.last_latency_ms),
                "cooling_secs": cooling.map(|u| u.duration_since(now).as_secs()),
            })
        })
        .collect();
    drop(health);
    respond_json(req, 200, serde_json::to_string(&entries).unwrap());
}

fn handle_chat(req: Request, raw: &str, state: &Arc<State>) {
    let reqid = log::next_reqid();
    // P-2: parse the body once to a Value, then deserialize the typed view.
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

    // ---- Routing (auto-router §5 / U-3 strict models) ----
    let model = parsed.model.clone();
    let (names_owned, route_label): (Vec<String>, Option<String>) = if let Some(router_cfg) =
        state.cfg.routers.get(&model)
    {
        let last_user = openai::last_user_text(&parsed.messages);
        let (class, classifier_ms) = classify_task(state, router_cfg, &last_user);
        let target = router_cfg.routes[&class].clone();
        let names = match router::resolve(&target, &state.cfg) {
            Ok(n) => n.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            Err(e) => return respond_err(req, 500, &e, "server_error", Some("router_error")),
        };
        log::info(&format!(
            "{reqid} route auto class={class} target={target} classifier_ms={classifier_ms}"
        ));
        (names, Some(format!("{class}/{target}")))
    } else {
        let known =
            state.cfg.chains.contains_key(&model) || state.cfg.backends.contains_key(&model);
        if !known {
            if state.cfg.server.strict_models {
                return respond_err(
                    req,
                    404,
                    &format!("unknown model '{model}'"),
                    "invalid_request_error",
                    Some("model_not_found"),
                );
            }
            log::warn(&format!("{reqid} unknown model '{model}' → default chain"));
        }
        match router::resolve(&model, &state.cfg) {
            Ok(n) => (n.iter().map(|s| s.to_string()).collect(), None),
            Err(e) => return respond_err(req, 400, &e, "invalid_request_error", Some("bad_model")),
        }
    };

    let prompt = openai::flatten_to_prompt(&parsed.messages);
    let last_user = openai::last_user_prompt(&parsed.messages); // U-14
    let default_to = Duration::from_secs(state.cfg.server.request_timeout_secs);

    if parsed.stream {
        stream_with_keepalive(
            req,
            state,
            names_owned,
            prompt,
            last_user,
            raw_value,
            default_to,
            route_label,
            reqid,
        );
        return;
    }

    let names_ref: Vec<&str> = names_owned.iter().map(|s| s.as_str()).collect();
    match run_chain(
        state, &names_ref, &prompt, &last_user, &raw_value, default_to, &reqid,
    ) {
        Ok(result) => {
            let id = format!("chatcmpl-{}", result.model_label);
            let body = serde_json::to_string(&openai::build_completion(
                id,
                result.model_label,
                result.content,
            ))
            .unwrap();
            let mut resp = Response::from_string(body)
                .with_header(header("Content-Type", "application/json"))
                .with_status_code(StatusCode(200));
            if let Some(rl) = &route_label {
                resp = resp.with_header(header("x-tmuxlet-route", rl));
            }
            let _ = req.respond(apply_cors(resp));
        }
        Err(errors) => respond_json(req, 503, build_503_body(&errors, state.redact_errors)),
    }
}

/// U-2: stream `: keepalive` comment frames every 15s while the chain runs on a
/// separate thread, then the final completion (or an in-band error) frames.
#[allow(clippy::too_many_arguments)]
fn stream_with_keepalive(
    req: Request,
    state: &Arc<State>,
    names: Vec<String>,
    prompt: String,
    last_user: String,
    raw_value: serde_json::Value,
    default_to: Duration,
    route_label: Option<String>,
    reqid: String,
) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let (rtx, rrx) = mpsc::channel::<Result<DispatchResult, Vec<BackendError>>>();
    let redact = state.redact_errors;
    let state_dispatch = Arc::clone(state);
    let reqid2 = reqid.clone();
    std::thread::spawn(move || {
        let names_ref: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let r = run_chain(
            &state_dispatch,
            &names_ref,
            &prompt,
            &last_user,
            &raw_value,
            default_to,
            &reqid2,
        );
        let _ = rtx.send(r);
    });
    std::thread::spawn(move || {
        let final_result = loop {
            match rrx.recv_timeout(Duration::from_secs(15)) {
                Ok(r) => break r,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if tx.send(b": keepalive\n\n".to_vec()).is_err() {
                        return; // client gone
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        };
        match final_result {
            Ok(result) => {
                let id = format!("chatcmpl-{}", result.model_label);
                for f in openai::stream_frames(&id, &result.model_label, &result.content) {
                    if tx.send(f.into_bytes()).is_err() {
                        return;
                    }
                }
            }
            Err(errors) => {
                let body = build_503_body(&errors, redact);
                let _ = tx.send(format!("data: {body}\n\n").into_bytes());
                let _ = tx.send(b"data: [DONE]\n\n".to_vec());
            }
        }
    });
    let mut headers = vec![
        header("Content-Type", "text/event-stream"),
        header("Cache-Control", "no-cache"),
    ];
    if let Some(rl) = &route_label {
        headers.push(header("x-tmuxlet-route", rl));
    }
    let resp = Response::new(
        StatusCode(200),
        headers,
        ChannelSseBody {
            rx,
            current: Vec::new(),
            pos: 0,
        },
        None,
        None,
    );
    let _ = req.respond(apply_cors(resp));
}

/// Walk the chain with P-9 cooldown, P-6 budget, and U-20 concurrency. Returns
/// the winning result or every leg's error.
fn run_chain(
    state: &State,
    names: &[&str],
    prompt: &str,
    last_user: &str,
    raw_value: &serde_json::Value,
    default_to: Duration,
    reqid: &str,
) -> Result<DispatchResult, Vec<BackendError>> {
    let cooldown_on = state.cfg.server.cooldown;
    let start = Instant::now();
    let budget_deadline = state
        .cfg
        .server
        .chain_budget_secs
        .map(|b| start + Duration::from_secs(b));

    // P-9: snapshot cooling status per candidate.
    let cooling: Vec<Option<Duration>> = names
        .iter()
        .map(|&n| {
            if !cooldown_on {
                return None;
            }
            let h = state.health.lock().unwrap();
            h.get(n)
                .and_then(|hh| hh.cooling_until)
                .and_then(|u| u.checked_duration_since(start))
        })
        .collect();

    let mut errors: Vec<BackendError> = Vec::new();
    for (i, &name) in names.iter().enumerate() {
        // P-6: budget exhausted — skip the rest.
        if let Some(dl) = budget_deadline
            && Instant::now() >= dl
        {
            errors.push(BackendError::Backend(
                name.to_string(),
                "skipped: budget".into(),
            ));
            log::debug(&format!("{reqid} skip backend={name}: budget exhausted"));
            continue;
        }
        // P-9: skip a cooling leg only if a non-cooling leg remains ahead.
        if let Some(remaining) = cooling[i] {
            let non_cooling_ahead = cooling[i + 1..].iter().any(|c| c.is_none());
            if non_cooling_ahead {
                let secs = remaining.as_secs();
                errors.push(BackendError::Backend(
                    name.to_string(),
                    format!("skipped: cooling {secs}s"),
                ));
                log::debug(&format!("{reqid} skip backend={name}: cooling {secs}s"));
                continue;
            }
        }
        let backend = &state.backends[name];
        // U-20: reserve a concurrency slot (RAII: released on Drop, even on a
        // dispatch panic). At the cap this leg is Busy and the chain advances.
        let _slot = match backend.max_concurrent() {
            Some(max) => match ActiveSlot::try_acquire(&state.active, name, max) {
                Some(slot) => Some(slot),
                None => {
                    errors.push(BackendError::Busy(name.to_string()));
                    continue;
                }
            },
            None => None,
        };
        // P-6: clamp the leg timeout to the remaining budget.
        let leg_to = backend.timeout_override().unwrap_or(default_to);
        let timeout = match budget_deadline {
            Some(dl) => dl.saturating_duration_since(Instant::now()).min(leg_to),
            None => leg_to,
        };
        let leg_start = Instant::now();
        let leg_prompt = match backend.prompt_mode() {
            openai::PromptMode::LastUser => last_user,
            openai::PromptMode::Transcript => prompt,
        };
        let result = backend.dispatch(leg_prompt, raw_value, &state.env, timeout);
        let elapsed_ms = leg_start.elapsed().as_millis() as u64;
        match result {
            Ok(res) => {
                if res.content.trim().is_empty() && !backend.allow_empty() {
                    let e = BackendError::Backend(name.to_string(), "empty output".into());
                    record_failure(state, name, &e, elapsed_ms);
                    log::warn(&format!("{reqid} skip backend={name}: empty output"));
                    errors.push(e);
                    continue;
                }
                record_success(state, name, elapsed_ms);
                log::info(&format!(
                    "{reqid} ok backend={name} elapsed_ms={elapsed_ms}"
                ));
                return Ok(res);
            }
            Err(e) => {
                log::warn(&format!("{reqid} skip backend={name}: {e}"));
                record_failure(state, name, &e, elapsed_ms);
                errors.push(e);
            }
        }
    }
    Err(errors)
}

fn record_success(state: &State, name: &str, latency_ms: u64) {
    let mut h = state.health.lock().unwrap();
    let e = h.entry(name.to_string()).or_default();
    e.consecutive_failures = 0;
    e.cooling_until = None;
    e.last_error = None;
    e.last_latency_ms = Some(latency_ms);
}

fn record_failure(state: &State, name: &str, err: &BackendError, latency_ms: u64) {
    let mut h = state.health.lock().unwrap();
    let e = h.entry(name.to_string()).or_default();
    e.consecutive_failures = e.consecutive_failures.saturating_add(1);
    e.last_error = Some(err.to_string());
    e.last_latency_ms = Some(latency_ms);
    if state.cfg.server.cooldown {
        let cd = cooldown_for(err, e.consecutive_failures, state.cfg.server.cooldown_secs);
        e.cooling_until = Some(Instant::now() + cd);
        log::info(&format!(
            "cooldown {name} for {}s ({})",
            cd.as_secs(),
            err.class()
        ));
    }
}

/// P-9: cooldown scaled by what a wasted retry costs.
fn cooldown_for(err: &BackendError, failures: u32, cooldown_secs: u64) -> Duration {
    match err {
        // A retry burns the whole timeout — back off exponentially 30 -> 300s.
        BackendError::Timeout(_) => {
            let shift = failures.saturating_sub(1).min(4);
            Duration::from_secs((30u64 << shift).min(300))
        }
        // Connect-refused fails in milliseconds; the "just restarted Ollama"
        // case must recover fast.
        BackendError::Spawn(_, msg) if msg.to_ascii_lowercase().contains("refused") => {
            Duration::from_secs(5)
        }
        // 429 / other HTTP: base cooldown (Retry-After not yet threaded through).
        _ => Duration::from_secs(cooldown_secs),
    }
}

fn build_503_body(errors: &[BackendError], redact: bool) -> String {
    // U-23: compact summary in `message`; full per-leg strings in details unless
    // redacted (S-6).
    let summary = errors
        .iter()
        .map(|e| format!("{} ({})", e.backend_name(), e.class()))
        .collect::<Vec<_>>()
        .join(", ");
    let details: Vec<String> = if redact {
        errors
            .iter()
            .map(|e| format!("{} ({})", e.backend_name(), e.class()))
            .collect()
    } else {
        errors.iter().map(|e| e.to_string()).collect()
    };
    openai::ErrorEnvelope::with_details(
        format!("all backends failed: {summary}"),
        "server_error",
        Some("all_backends_failed"),
        details,
    )
    .to_json()
}

/// §5: classify the task via the router's classifier backend. Never fails the
/// request — any error/timeout/garbage falls back to `fallback_class`.
fn classify_task(
    state: &State,
    router_cfg: &crate::config::Router,
    last_user: &str,
) -> (String, u64) {
    let classes: Vec<&str> = router_cfg.routes.keys().map(|s| s.as_str()).collect();
    let truncated = truncate_tail(last_user, router_cfg.classifier_max_chars);
    let prompt = match &router_cfg.classifier_prompt {
        Some(tpl) => tpl
            .replace("{classes}", &classes.join(", "))
            .replace("{input}", &truncated),
        None => format!(
            "Classify the task into exactly one of these labels: {}.\n\nTask:\n{}\n\nAnswer with exactly one label.",
            classes.join(", "),
            truncated
        ),
    };
    let Some(classifier) = state.backends.get(&router_cfg.classifier) else {
        return (router_cfg.fallback_class.clone(), 0);
    };
    // U-20: honor the classifier backend's concurrency cap. At capacity, degrade
    // to the fallback class rather than spawning an uncounted session (the same
    // cap run_chain enforces on chain legs). Routing never fails the request.
    let _slot = match classifier.max_concurrent() {
        Some(max) => match ActiveSlot::try_acquire(&state.active, &router_cfg.classifier, max) {
            Some(slot) => Some(slot),
            None => return (router_cfg.fallback_class.clone(), 0),
        },
        None => None,
    };
    let timeout = Duration::from_secs(router_cfg.classifier_timeout_secs);
    let raw = serde_json::json!({"messages": [{"role": "user", "content": prompt}]});
    let start = Instant::now();
    let result = classifier.dispatch(&prompt, &raw, &state.env, timeout);
    let ms = start.elapsed().as_millis() as u64;
    let class = match result {
        Ok(r) => parse_class(&r.content, &router_cfg.routes, &router_cfg.fallback_class),
        Err(_) => router_cfg.fallback_class.clone(),
    };
    (class, ms)
}

fn parse_class(reply: &str, routes: &HashMap<String, String>, fallback: &str) -> String {
    let r = reply.trim().to_ascii_lowercase();
    for k in routes.keys() {
        if k.to_ascii_lowercase() == r {
            return k.clone();
        }
    }
    for k in routes.keys() {
        if r.contains(&k.to_ascii_lowercase()) {
            return k.clone();
        }
    }
    fallback.to_string()
}

fn truncate_tail(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

pub fn serve(server: Arc<Server>, state: Arc<State>, workers: usize) {
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        handles.push(std::thread::spawn(move || {
            while let Ok(req) = server.recv() {
                // F-3: a panic while handling one request must not tear down the
                // worker (which would silently shrink capacity).
                let st = &state;
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(req, st)));
                if let Err(p) = result {
                    let msg = p
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into());
                    log::error(&format!("worker recovered from panic: {msg}"));
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
        assert!(matches!(classify("GET", "/v1/backends"), Route::Backends));
        assert!(matches!(
            classify("POST", "/v1/chat/completions"),
            Route::Chat
        ));
        assert!(matches!(classify("GET", "/ui/index.html"), Route::Reserved));
        assert!(matches!(classify("GET", "/"), Route::Reserved));
        assert!(matches!(classify("GET", "/nope"), Route::NotFound));
        // U-17: HEAD mirrors GET routing.
        assert!(matches!(classify("HEAD", "/health"), Route::Health));
        assert!(matches!(classify("HEAD", "/v1/models"), Route::Models));
        // U-17: known path, wrong method → 405 with the right Allow value.
        assert!(matches!(
            classify("DELETE", "/v1/models"),
            Route::MethodNotAllowed("GET, HEAD")
        ));
        assert!(matches!(
            classify("GET", "/v1/chat/completions"),
            Route::MethodNotAllowed("POST")
        ));
    }

    #[test]
    fn channel_sse_body_streams_then_eof_on_drop() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b": keepalive\n\n".to_vec()).unwrap();
        tx.send(b"data: x\n\n".to_vec()).unwrap();
        drop(tx); // sender drop -> EOF
        let mut body = ChannelSseBody {
            rx,
            current: Vec::new(),
            pos: 0,
        };
        let mut s = String::new();
        std::io::Read::read_to_string(&mut body, &mut s).unwrap();
        assert_eq!(s, ": keepalive\n\ndata: x\n\n");
    }

    #[test]
    fn cooldown_scales_with_error_class() {
        assert_eq!(
            cooldown_for(&BackendError::Timeout("t".into()), 1, 60).as_secs(),
            30
        );
        assert_eq!(
            cooldown_for(&BackendError::Timeout("t".into()), 3, 60).as_secs(),
            120
        );
        assert_eq!(
            cooldown_for(&BackendError::Timeout("t".into()), 9, 60).as_secs(),
            300
        );
        assert_eq!(
            cooldown_for(
                &BackendError::Spawn("t".into(), "Connection refused".into()),
                1,
                60
            )
            .as_secs(),
            5
        );
        assert_eq!(
            cooldown_for(&BackendError::Http("t".into(), 429, String::new()), 1, 60).as_secs(),
            60
        );
    }

    #[test]
    fn parse_class_matches_exact_then_contains_then_fallback() {
        let mut routes = HashMap::new();
        routes.insert("research".to_string(), "best".to_string());
        routes.insert("execution".to_string(), "fast".to_string());
        assert_eq!(parse_class("RESEARCH", &routes, "execution"), "research");
        assert_eq!(
            parse_class("I think this is execution work", &routes, "research"),
            "execution"
        );
        assert_eq!(parse_class("nonsense", &routes, "execution"), "execution");
    }

    #[test]
    fn active_slot_caps_and_frees() {
        // U-20: the guard blocks a second acquire at cap and frees on Drop.
        let active = Mutex::new(HashMap::new());
        let s1 = ActiveSlot::try_acquire(&active, "b", 1);
        assert!(s1.is_some(), "first acquire under cap succeeds");
        assert_eq!(*active.lock().unwrap().get("b").unwrap(), 1);
        assert!(
            ActiveSlot::try_acquire(&active, "b", 1).is_none(),
            "second acquire at cap is refused"
        );
        drop(s1);
        assert_eq!(*active.lock().unwrap().get("b").unwrap(), 0);
        assert!(
            ActiveSlot::try_acquire(&active, "b", 1).is_some(),
            "slot is reusable after Drop"
        );
    }

    #[test]
    fn active_slot_releases_on_unwind() {
        // A2: a panic between acquire and release must not leak the slot (the
        // server survives dispatch panics via F-3 catch_unwind).
        let active = Mutex::new(HashMap::new());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = ActiveSlot::try_acquire(&active, "b", 1).unwrap();
            panic!("boom");
        }));
        assert!(r.is_err());
        assert_eq!(
            *active.lock().unwrap().get("b").unwrap(),
            0,
            "slot must be freed after an unwind"
        );
    }
}
