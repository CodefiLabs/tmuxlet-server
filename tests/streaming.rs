//! `stream: true` yields a well-formed SSE body terminated by [DONE].
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
fn stream_emits_chunks_role_and_done() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","stream":true,"messages":[{"role":"user","content":"streamtest"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(
        body.contains("chat.completion.chunk"),
        "no chunk object: {body}"
    );
    assert!(
        body.contains(r#""role":"assistant""#),
        "no role-prime frame: {body}"
    );
    assert!(body.contains("streamtest"), "content frame missing: {body}");
    assert!(
        body.trim_end().ends_with("data: [DONE]"),
        "stream not terminated by [DONE]: {body}"
    );
}
