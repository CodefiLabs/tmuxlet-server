//! cli-backend paths with no other coverage: PTY execution and the timeout
//! watchdog kill path. Unix-only (uses executable-bit + /bin tools).
#![cfg(unix)]
mod common;

use std::os::unix::fs::PermissionsExt;

#[test]
fn pty_backend_runs_in_a_pty_and_cleans_output() {
    let port = common::free_port();
    // pty=true runs the binary in a PTY and writes the prompt to its stdin.
    // /bin/echo ignores stdin and prints its configured arg; the marker proves
    // the PTY path (openpty -> spawn -> read -> clean_output) executed.
    let cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.ptyecho]
type = "cli"
bin = "/bin/echo"
args = ["pty-works-42"]
pty = true

[chains.default]
order = ["ptyecho"]
"#
    );
    let server = common::start(&cfg);
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("pty-works-42"), "PTY output missing: {body}");
}

#[test]
fn watchdog_kills_an_overrunning_backend() {
    // A backend that sleeps far longer than the 1s request timeout must be
    // killed by the watchdog and reported as a failure (here, an all-failing
    // chain -> 503), rather than blocking the worker for the full sleep.
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("sleeper.sh");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let port = common::free_port();
    let cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "slow"
env_source = "process"
request_timeout_secs = 1

[backends.slow]
type = "cli"
bin = "{script}"

[chains.slow]
order = ["slow"]
"#,
        script = script.display()
    );
    let server = common::start(&cfg);
    let start = std::time::Instant::now();
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"slow","messages":[{"role":"user","content":"hi"}]}"#,
    );
    let elapsed = start.elapsed();
    assert_eq!(status, 503, "body: {body}");
    assert!(body.contains("all_backends_failed"), "body: {body}");
    // Killed near the 1s deadline, nowhere near the 30s sleep.
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "watchdog did not kill promptly: {elapsed:?}"
    );
}
