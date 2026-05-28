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
