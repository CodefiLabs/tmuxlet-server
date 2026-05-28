use super::{BackendError, DispatchResult, TmuxletBackend};
use crate::env::Env;
use std::time::Duration;

/// Assemble the argv for `tmuxlet -p --target T --output-format json -C cwd
/// --timeout S [--target-arg A ...] <prompt>`.
pub fn build_args(b: &TmuxletBackend, prompt: &str, timeout_secs: u64) -> Vec<String> {
    let mut a = vec![
        "-p".into(),
        "--target".into(),
        b.target.clone(),
        "--output-format".into(),
        "json".into(),
        "-C".into(),
        b.cwd.display().to_string(),
        "--timeout".into(),
        timeout_secs.to_string(),
    ];
    for ta in &b.target_args {
        a.push("--target-arg".into());
        a.push(ta.clone());
    }
    a.push(prompt.into());
    a
}

/// Parse tmuxlet's JSON output. Success iff `status == "completed"` (verified
/// against tmuxlet 0.3.0 `result_status_code`; deviation #1). Non-JSON stdout
/// is treated as raw content.
pub fn parse_output(name: &str, stdout: &str) -> Result<String, BackendError> {
    let trimmed = stdout.trim();
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => {
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if status == "completed" {
                Ok(v.get("output")
                    .and_then(|o| o.as_str())
                    .unwrap_or("")
                    .to_string())
            } else {
                Err(BackendError::Backend(
                    name.into(),
                    format!("tmuxlet status={status}"),
                ))
            }
        }
        Err(_) => Ok(stdout.to_string()),
    }
}

/// Spawn `tmuxlet` and return its output. STUB — implemented in Task 7
/// (see docs/superpowers/plans/2026-05-28-tmuxlet-server.md).
pub fn dispatch(
    _b: &TmuxletBackend,
    _prompt: &str,
    _env: &Env,
    _timeout: Duration,
) -> Result<DispatchResult, BackendError> {
    todo!("Task 7: spawn tmuxlet with build_args + watchdog, then parse_output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn b() -> TmuxletBackend {
        TmuxletBackend {
            name: "t".into(),
            target: "claude".into(),
            target_args: vec!["--effort".into(), "max".into()],
            cwd: PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn assembles_args_with_target_args_and_timeout() {
        let a = build_args(&b(), "hello", 1800);
        assert_eq!(
            a,
            vec![
                "-p",
                "--target",
                "claude",
                "--output-format",
                "json",
                "-C",
                "/tmp",
                "--timeout",
                "1800",
                "--target-arg",
                "--effort",
                "--target-arg",
                "max",
                "hello"
            ]
        );
    }

    #[test]
    fn parses_completed_status() {
        let j = r#"{"id":"r1","target":"claude","status":"completed","output":"done","cwd":"/tmp","tmuxSession":"s","completionSource":"hook","elapsedMs":12}"#;
        assert_eq!(parse_output("t", j).unwrap(), "done");
    }

    #[test]
    fn non_completed_status_is_error() {
        let j = r#"{"id":"r1","target":"claude","status":"timeout","output":"partial","cwd":"/tmp","tmuxSession":"s","completionSource":"none","elapsedMs":9}"#;
        assert!(parse_output("t", j).is_err());
    }

    #[test]
    fn non_json_falls_back_to_raw() {
        assert_eq!(parse_output("t", "just text").unwrap(), "just text");
    }
}
