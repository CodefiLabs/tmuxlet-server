use super::{BackendError, CliBackend, DispatchResult};
use crate::env::Env;
use crate::pty;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Placeholder token in a cli backend's `args`. When present, the prompt is
/// substituted in place (e.g. `args = ["-p", "{prompt}"]` -> `-p <prompt>`)
/// instead of being appended / written to stdin. Required for tools like `agy`
/// whose `--print` flag takes the prompt as its value, not via stdin.
pub const PROMPT_PLACEHOLDER: &str = "{prompt}";

/// Substitute `{prompt}` in each arg. Returns the rewritten args and whether any
/// substitution happened, so the caller knows not to also pass the prompt by the
/// default channel (trailing positional for plain, stdin for PTY).
fn substitute_prompt(args: &[String], prompt: &str) -> (Vec<String>, bool) {
    let mut substituted = false;
    let out = args
        .iter()
        .map(|a| {
            if a.contains(PROMPT_PLACEHOLDER) {
                substituted = true;
                a.replace(PROMPT_PLACEHOLDER, prompt)
            } else {
                a.clone()
            }
        })
        .collect();
    (out, substituted)
}

/// For a non-PTY CLI: the prompt substituted into a `{prompt}` placeholder if
/// present, otherwise appended as the final positional argument.
pub fn plain_args(b: &CliBackend, prompt: &str) -> Vec<String> {
    let (mut a, substituted) = substitute_prompt(&b.args, prompt);
    if !substituted {
        a.push(prompt.into());
    }
    a
}

/// Run the CLI backend and return cleaned output. `pty=true` runs the binary in
/// a PTY (for TUI tools like agy); `pty=false` uses a plain piped child guarded
/// by a try_wait watchdog. Either way the output is stripped of ANSI/control
/// bytes via `pty::clean_output`.
pub fn dispatch(
    b: &CliBackend,
    prompt: &str,
    env: &Env,
    timeout: Duration,
) -> Result<DispatchResult, BackendError> {
    let content = if b.pty {
        // If args carry `{prompt}`, the prompt goes into argv and stdin stays
        // empty (so the PTY doesn't echo the prompt back into the output).
        // Otherwise the prompt is written to the child's stdin, as before.
        let (pty_args, substituted) = substitute_prompt(&b.args, prompt);
        let stdin_payload = if substituted { "" } else { prompt };
        let (raw, status) = pty::run_in_pty(
            &b.bin.display().to_string(),
            &pty_args,
            &env.as_pairs(),
            &std::env::var("HOME").unwrap_or_else(|_| ".".into()),
            stdin_payload,
            timeout,
        )
        .map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?;
        let cleaned = pty::clean_output(&raw);
        // A non-zero exit with no usable output is a failure, so the fallback
        // chain advances to the next backend (mirrors the non-PTY branch).
        if !status.success() && cleaned.trim().is_empty() {
            return Err(BackendError::Exit(
                b.name.clone(),
                status.exit_code() as i32,
                String::new(),
            ));
        }
        cleaned
    } else {
        let mut child = Command::new(&b.bin)
            .args(plain_args(b, prompt))
            .env_clear()
            .envs(env.as_pairs())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?;
        let mut out = child.stdout.take().unwrap();
        let mut err = child.stderr.take().unwrap();
        let (otx, orx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut s = Vec::new();
            let _ = std::io::Read::read_to_end(&mut out, &mut s);
            let _ = otx.send(s);
        });
        let (etx, erx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut s = Vec::new();
            let _ = std::io::Read::read_to_end(&mut err, &mut s);
            let _ = etx.send(s);
        });
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child
                .try_wait()
                .map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?
            {
                Some(s) => break s,
                None => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(BackendError::Timeout(b.name.clone()));
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        };
        // Bounded collection (see tmuxlet backend): a grandchild holding the
        // pipe must not block the worker forever after the child has exited.
        let grace = Duration::from_secs(5);
        let cleaned = pty::clean_output(&orx.recv_timeout(grace).unwrap_or_default());
        let stderr =
            String::from_utf8_lossy(&erx.recv_timeout(grace).unwrap_or_default()).into_owned();
        // A non-zero exit with no usable stdout is a failure, so the fallback
        // chain advances to the next backend (mirrors the tmuxlet backend).
        if !status.success() && cleaned.trim().is_empty() {
            return Err(BackendError::Exit(
                b.name.clone(),
                status.code().unwrap_or(-1),
                stderr.lines().last().unwrap_or("").to_string(),
            ));
        }
        cleaned
    };
    Ok(DispatchResult {
        content,
        model_label: b.name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn plain_args_append_prompt() {
        let b = CliBackend {
            name: "x".into(),
            bin: PathBuf::from("/bin/echo"),
            args: vec!["-n".into()],
            pty: false,
        };
        assert_eq!(
            plain_args(&b, "hello"),
            vec!["-n".to_string(), "hello".to_string()]
        );
    }

    #[test]
    fn plain_args_substitutes_placeholder_instead_of_appending() {
        // When `{prompt}` appears in args, the prompt is substituted there and
        // NOT also appended as a trailing positional (which would double it).
        let b = CliBackend {
            name: "x".into(),
            bin: PathBuf::from("/bin/echo"),
            args: vec!["-p".into(), "{prompt}".into()],
            pty: false,
        };
        assert_eq!(
            plain_args(&b, "hello world"),
            vec!["-p".to_string(), "hello world".to_string()]
        );
    }

    #[test]
    fn pty_dispatch_substitutes_placeholder_into_argv() {
        // A PTY tool whose flag takes the prompt as its value (like agy's
        // `-p <prompt>`) must receive the prompt in argv, not on stdin.
        let b = CliBackend {
            name: "echoer".into(),
            bin: PathBuf::from("/bin/echo"),
            args: vec!["{prompt}".into()],
            pty: true,
        };
        let env = Env::capture("process", "");
        let r = dispatch(&b, "READYTOKEN", &env, Duration::from_secs(10)).unwrap();
        assert_eq!(r.content.trim(), "READYTOKEN");
    }

    #[test]
    fn pty_dispatch_fails_on_nonzero_exit_with_no_output() {
        // The fallback chain advances only if a failed backend surfaces as an
        // error. Use the `{prompt}` placeholder so the prompt goes to argv and
        // stdin stays empty (no PTY echo); `exit 1` then produces no output.
        let b = CliBackend {
            name: "failer".into(),
            bin: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "exit 1".into(), "{prompt}".into()],
            pty: true,
        };
        let env = Env::capture("process", "");
        let err = dispatch(&b, "anything", &env, Duration::from_secs(10)).unwrap_err();
        assert!(
            matches!(err, BackendError::Exit(ref n, _, _) if n == "failer"),
            "expected Exit error naming the backend, got: {err:?}"
        );
    }
}
