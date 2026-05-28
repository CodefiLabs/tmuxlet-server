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
    let (status, content_type, body) = common::post_collect(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","stream":true,"messages":[{"role":"user","content":"streamtest"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(
        content_type.contains("text/event-stream"),
        "wrong SSE content-type: {content_type:?}"
    );
    assert!(
        body.contains("chat.completion.chunk"),
        "no chunk object: {body}"
    );
    assert!(
        body.trim_end().ends_with("data: [DONE]"),
        "stream not terminated by [DONE]: {body}"
    );
    // Frame ordering: role-prime, then content, then terminator.
    let role = body
        .find(r#""role":"assistant""#)
        .expect("role-prime frame");
    let content = body.find("streamtest").expect("content frame");
    let done = body.find("data: [DONE]").expect("DONE terminator");
    assert!(
        role < content && content < done,
        "frames out of order (role={role}, content={content}, done={done}): {body}"
    );
}
