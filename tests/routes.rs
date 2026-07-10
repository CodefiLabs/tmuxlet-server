//! Route classification: /v1/models listing, 404 envelope, reserved 501.
mod common;

fn config(port: u16) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn models_lists_chains_and_backends() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::get(&format!("{}/v1/models", server.base));
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""object":"list""#), "body: {body}");
    assert!(
        body.contains(r#""id":"echo""#),
        "backend name missing: {body}"
    );
    assert!(
        body.contains(r#""id":"default""#),
        "chain name missing: {body}"
    );
}

#[test]
fn unknown_route_is_404_with_error_envelope() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::get(&format!("{}/nope", server.base));
    assert_eq!(status, 404, "body: {body}");
    assert!(body.contains(r#""error""#), "no error envelope: {body}");
    assert!(body.contains("unknown_route"), "body: {body}");
}

#[test]
fn reserved_ui_route_is_501() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::get(&format!("{}/ui/index.html", server.base));
    assert_eq!(status, 501, "body: {body}");
    assert!(body.contains("not_implemented"), "body: {body}");
}

#[test]
fn malformed_json_body_is_400() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        "{not valid json",
    );
    assert_eq!(status, 400, "body: {body}");
    assert!(body.contains("parse_error"), "body: {body}");
}

#[test]
fn empty_messages_is_400() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","messages":[]}"#,
    );
    assert_eq!(status, 400, "body: {body}");
    assert!(body.contains("missing_messages"), "body: {body}");
}

#[test]
fn reserved_api_and_root_are_501() {
    let server = common::start(&config(common::free_port()));
    let (api_status, _) = common::get(&format!("{}/api/sessions/1", server.base));
    assert_eq!(api_status, 501, "/api/* should be reserved");
    let (root_status, _) = common::get(&format!("{}/", server.base));
    assert_eq!(root_status, 501, "/ should be reserved");
}

#[test]
fn head_on_known_get_route_is_200_with_empty_body() {
    // U-17: HEAD mirrors GET but carries no body.
    let server = common::start(&config(common::free_port()));
    let (status, _allow, body) = common::request("HEAD", &format!("{}/v1/models", server.base));
    assert_eq!(status, 200, "HEAD /v1/models should be 200");
    assert!(body.is_empty(), "HEAD must not return a body: {body:?}");
}

#[test]
fn wrong_method_on_known_path_is_405_with_allow_header() {
    // U-17: a known path reached with the wrong method → 405 naming the fix.
    let server = common::start(&config(common::free_port()));
    let (status, allow, body) = common::request("DELETE", &format!("{}/v1/models", server.base));
    assert_eq!(status, 405, "DELETE /v1/models should be 405: {body}");
    assert_eq!(
        allow.as_deref(),
        Some("GET, HEAD"),
        "Allow header missing/wrong"
    );
    assert!(body.contains("method_not_allowed"), "body: {body}");

    let (chat_status, chat_allow, _) =
        common::request("GET", &format!("{}/v1/chat/completions", server.base));
    assert_eq!(chat_status, 405, "GET /v1/chat/completions should be 405");
    assert_eq!(chat_allow.as_deref(), Some("POST"), "Allow should be POST");
}

fn echo_config(port: u16, prompt_mode: &str) -> String {
    // /bin/echo echoes the shaped prompt as argv, so the completion content is
    // exactly what the backend received — lets us assert U-14 shaping end-to-end.
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.echo]
type = "cli"
bin = "/bin/echo"
prompt_mode = "{prompt_mode}"

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn prompt_mode_last_user_sends_verbatim_without_role_labels() {
    // U-14: last_user shaping drops "System:"/"User:" labels and the assistant turn.
    let server = common::start(&echo_config(common::free_port(), "last_user"));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","messages":[{"role":"system","content":"be terse"},{"role":"user","content":"hi"},{"role":"assistant","content":"prev"},{"role":"user","content":"final ask"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("final ask"), "missing user text: {body}");
    assert!(body.contains("be terse"), "missing system text: {body}");
    assert!(
        !body.contains("User:"),
        "last_user must not label roles: {body}"
    );
    assert!(
        !body.contains("Assistant:"),
        "last_user drops the assistant turn: {body}"
    );
}

#[test]
fn prompt_mode_transcript_is_the_default_shaping() {
    // U-14: default (transcript) keeps role labels, unified to "User:"/"System:".
    let server = common::start(&echo_config(common::free_port(), "transcript"));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","messages":[{"role":"system","content":"S"},{"role":"user","content":"U"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("System: S"), "body: {body}");
    assert!(body.contains("User: U"), "body: {body}");
}

fn cors_config(port: u16, origin: &str) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"
cors_origins = ["{origin}"]

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn cors_preflight_204_and_echo_when_origin_allowed() {
    // U-10: OPTIONS preflight is answered; the allowed Origin is echoed on both
    // the preflight and the actual GET.
    let origin = "http://localhost:5173";
    let server = common::start(&cors_config(common::free_port(), origin));

    let (status, headers, _) = common::send(
        "OPTIONS",
        &format!("{}/v1/models", server.base),
        &[("Origin", origin)],
    );
    assert_eq!(status, 204, "preflight should be 204");
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .map(String::as_str),
        Some(origin)
    );
    assert!(
        headers
            .get("access-control-allow-headers")
            .is_some_and(|h| h.contains("authorization") && h.contains("content-type")),
        "preflight allow-headers: {headers:?}"
    );

    let (gs, gh, _) = common::send(
        "GET",
        &format!("{}/v1/models", server.base),
        &[("Origin", origin)],
    );
    assert_eq!(gs, 200);
    assert_eq!(
        gh.get("access-control-allow-origin").map(String::as_str),
        Some(origin),
        "actual response must echo the origin"
    );
}

#[test]
fn cors_absent_for_unlisted_origin_and_by_default() {
    // U-10: an unlisted Origin gets no ACAO even on a CORS-enabled server.
    let server = common::start(&cors_config(common::free_port(), "http://localhost:5173"));
    let (_s, gh, _) = common::send(
        "GET",
        &format!("{}/v1/models", server.base),
        &[("Origin", "http://evil.test")],
    );
    assert!(
        !gh.contains_key("access-control-allow-origin"),
        "unlisted origin must not be echoed: {gh:?}"
    );

    // And the default config (no cors_origins) never emits CORS headers.
    let plain = common::start(&config(common::free_port()));
    let (_s2, ph, _) = common::send(
        "GET",
        &format!("{}/v1/models", plain.base),
        &[("Origin", "http://localhost:5173")],
    );
    assert!(
        !ph.contains_key("access-control-allow-origin"),
        "CORS off by default: {ph:?}"
    );
}

#[test]
fn zero_workers_is_clamped_and_still_serves() {
    // P-1: workers = 0 must not exit the process; serve clamps to 1. If the clamp
    // regressed, the server would exit and common::start would panic on readiness.
    let cfg = config(common::free_port()).replace(
        "env_source = \"process\"",
        "env_source = \"process\"\nworkers = 0",
    );
    let server = common::start(&cfg);
    let (status, _) = common::get(&format!("{}/health", server.base));
    assert_eq!(status, 200, "clamped server must answer /health");
}

fn router_config(port: u16) -> String {
    // A single-class router over /bin/echo. Whatever the classifier echoes,
    // parse_class finds "only" (or falls back to it), so routing is deterministic:
    // class "only" -> chain "default" -> winning backend "echo".
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[routers.smart]
classifier = "echo"
fallback_class = "only"
routes = {{ only = "default" }}

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn route_header_names_the_winning_backend() {
    // B1: x-tmuxlet-route is class/chain/backend — the third segment names the
    // fallback leg that actually answered (unrecoverable from the body).
    let server = common::start(&router_config(common::free_port()));
    let (status, headers, body) = common::post_headers(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"smart","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    let route = headers
        .get("x-tmuxlet-route")
        .expect("x-tmuxlet-route must be present on a router request");
    let segs: Vec<&str> = route.split('/').collect();
    assert_eq!(
        segs.len(),
        3,
        "route must be class/chain/backend, got '{route}'"
    );
    assert_eq!(
        segs[2], "echo",
        "third segment must name the winning backend: '{route}'"
    );
}

#[test]
fn strict_models_off_falls_back_to_default_chain() {
    // U-3: default (strict_models unset) — an unknown model runs default_chain.
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"nonexistent-model","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "unknown model should fall back, got: {body}");
    assert!(body.contains("chat.completion"), "body: {body}");
}

#[test]
fn strict_models_on_rejects_unknown_model_and_names_the_fix() {
    // U-3 + invariant 3: strict_models = true -> 404 that names its remedy.
    let cfg = config(common::free_port()).replace(
        "env_source = \"process\"",
        "env_source = \"process\"\nstrict_models = true",
    );
    let server = common::start(&cfg);
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"nonexistent-model","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(
        status, 404,
        "strict_models must reject unknown model: {body}"
    );
    assert!(body.contains("model_not_found"), "body: {body}");
    assert!(
        body.contains("/v1/models") && body.contains("strict_models = false"),
        "404 must name both remedies: {body}"
    );
}

#[test]
fn backends_endpoint_shape_and_ok_transition() {
    // U-13/B5: /v1/backends reports each backend's health with cooling_secs (the
    // deliberate rename of cooling_until). A fresh backend is "unknown"; after a
    // successful dispatch it becomes "ok".
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::get(&format!("{}/v1/backends", server.base));
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""name":"echo""#), "body: {body}");
    assert!(body.contains(r#""type":"cli""#), "body: {body}");
    assert!(
        body.contains(r#""state":"unknown""#),
        "a fresh backend is unknown: {body}"
    );
    for field in [
        "consecutive_failures",
        "last_error",
        "last_latency_ms",
        "cooling_secs",
    ] {
        assert!(body.contains(field), "missing field {field}: {body}");
    }

    // Drive one successful request, then the backend transitions to "ok".
    let (cs, cb) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(cs, 200, "chat body: {cb}");
    let (_s, body2) = common::get(&format!("{}/v1/backends", server.base));
    assert!(
        body2.contains(r#""state":"ok""#),
        "backend should be ok after a success: {body2}"
    );
}
