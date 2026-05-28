//! Fallback chain: a failing backend is skipped; an all-failing chain 503s.
mod common;

fn config(port: u16) -> String {
    // `broken` = /usr/bin/false (exits non-zero, empty stdout -> Exit error).
    // `good`   = /bin/echo (echoes the flattened prompt).
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "fallback"
env_source = "process"

[backends.broken]
type = "cli"
bin = "/usr/bin/false"

[backends.good]
type = "cli"
bin = "/bin/echo"

[chains.fallback]
order = ["broken", "good"]

[chains.onlybroken]
order = ["broken"]
"#
    )
}

#[test]
fn falls_through_failing_backend_to_a_working_one() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"fallback","messages":[{"role":"user","content":"ping-7788"}]}"#,
    );
    assert_eq!(status, 200, "expected fallback to `good`: {body}");
    assert!(
        body.contains(r#""object":"chat.completion""#),
        "body: {body}"
    );
    // `good` echoes the flattened prompt, which contains the user content.
    assert!(body.contains("ping-7788"), "echoed content missing: {body}");
    // The served backend label should be `good`, not `broken`.
    assert!(
        body.contains(r#""model":"good""#),
        "served-backend label: {body}"
    );
}

#[test]
fn all_failing_chain_returns_503() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"onlybroken","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 503, "body: {body}");
    assert!(body.contains("all_backends_failed"), "body: {body}");
}
