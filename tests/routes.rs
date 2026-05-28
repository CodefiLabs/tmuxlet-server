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
