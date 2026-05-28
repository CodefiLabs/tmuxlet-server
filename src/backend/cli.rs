use super::{BackendError, CliBackend, DispatchResult};
use crate::env::Env;
use crate::pty;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// For a non-PTY CLI: the configured args with the prompt appended as the
/// final positional argument.
pub fn plain_args(b: &CliBackend, prompt: &str) -> Vec<String> {
    let mut a = b.args.clone();
    a.push(prompt.into());
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
        let raw = pty::run_in_pty(
            &b.bin.display().to_string(),
            &b.args,
            &env.as_pairs(),
            &std::env::var("HOME").unwrap_or_else(|_| ".".into()),
            prompt,
            timeout,
        )
        .map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?;
        pty::clean_output(&raw)
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
}
