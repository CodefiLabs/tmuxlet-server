//! S-1: bearer-token auth gate wiring — which routes are protected, which stay
//! open. Hermetic: the token comes from an injected env var (`auth_token_env`),
//! so no `~/.tmuxlet/token` file is touched.
mod common;

const TOKEN: &str = "s3cr3t-test-token";

fn auth_config(port: u16) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"
auth = true
auth_token_env = "TEST_AUTH_TOKEN"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo"]
"#
    )
}

fn start_auth(port: u16) -> common::Server {
    common::start_with_env(&auth_config(port), &[("TEST_AUTH_TOKEN", TOKEN)])
}

#[test]
fn protected_routes_are_401_without_a_token() {
    let server = start_auth(common::free_port());

    let (models, _h, body) = common::send("GET", &format!("{}/v1/models", server.base), &[]);
    assert_eq!(models, 401, "models must require auth: {body}");
    assert!(
        body.contains("unauthorized"),
        "401 envelope missing: {body}"
    );

    let (backends, _h2, _b2) = common::send("GET", &format!("{}/v1/backends", server.base), &[]);
    assert_eq!(backends, 401, "backends must require auth");

    let (chat, _h3, _b3) = common::send_body(
        "POST",
        &format!("{}/v1/chat/completions", server.base),
        &[],
        r#"{"model":"default","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(chat, 401, "chat must require auth");
}

#[test]
fn a_valid_bearer_token_is_accepted() {
    let server = start_auth(common::free_port());
    let auth = format!("Bearer {TOKEN}");
    let (status, _h, body) = common::send(
        "GET",
        &format!("{}/v1/models", server.base),
        &[("Authorization", &auth)],
    );
    assert_eq!(status, 200, "valid token must be accepted: {body}");
    assert!(body.contains(r#""object":"list""#), "body: {body}");
}

#[test]
fn a_wrong_bearer_token_is_rejected() {
    let server = start_auth(common::free_port());
    let (status, _h, body) = common::send(
        "GET",
        &format!("{}/v1/models", server.base),
        &[("Authorization", "Bearer not-the-token")],
    );
    assert_eq!(status, 401, "a wrong token must be rejected: {body}");
}

#[test]
fn health_stays_open_without_a_token() {
    // Liveness probes must not need a token, or orchestrators can't check health.
    let server = start_auth(common::free_port());
    let (status, body) = common::get(&format!("{}/health", server.base));
    assert_eq!(status, 200, "health must be open: {body}");
}
