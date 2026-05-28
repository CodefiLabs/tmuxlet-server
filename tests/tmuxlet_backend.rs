//! Integration coverage for the tmuxlet backend via a fake `tmuxlet` on PATH
//! (real tmuxlet isn't available in CI). Exercises spawn -> drain -> parse_output.
#![cfg(unix)]
mod common;

use std::os::unix::fs::PermissionsExt;

#[test]
fn tmuxlet_backend_parses_completed_json() {
    // Fake `tmuxlet` that ignores its args and emits a completed-status JSON.
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("tmuxlet");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s' '{\"id\":\"r1\",\"target\":\"claude\",\"status\":\"completed\",\"output\":\"tmuxlet-marker-77\",\"cwd\":\"/tmp\",\"tmuxSession\":\"s\",\"completionSource\":\"hook\",\"elapsedMs\":1}'\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake, perms).unwrap();

    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let port = common::free_port();
    let cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "tmux"
env_source = "process"

[backends.claude]
type = "tmuxlet"
target = "claude"

[chains.tmux]
order = ["claude"]
"#
    );
    let server = common::start_with_env(&cfg, &[("PATH", &path)]);
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"tmux","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(
        body.contains("tmuxlet-marker-77"),
        "tmuxlet output missing: {body}"
    );
}
