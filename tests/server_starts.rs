//! The binary boots against a minimal config and answers `/health`.
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
fn health_reports_ok() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::get(&format!("{}/health", server.base));
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""status":"ok""#), "body: {body}");
}
