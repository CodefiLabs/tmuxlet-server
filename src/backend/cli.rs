use super::{BackendError, CliBackend, DispatchResult};
use crate::env::Env;
use std::time::Duration;

/// For a non-PTY CLI: the configured args with the prompt appended as the
/// final positional argument.
pub fn plain_args(b: &CliBackend, prompt: &str) -> Vec<String> {
    let mut a = b.args.clone();
    a.push(prompt.into());
    a
}

/// Run the CLI backend (PTY or plain) and return cleaned output. STUB —
/// implemented in Task 9 (see docs/superpowers/plans/2026-05-28-tmuxlet-server.md).
pub fn dispatch(
    _b: &CliBackend,
    _prompt: &str,
    _env: &Env,
    _timeout: Duration,
) -> Result<DispatchResult, BackendError> {
    todo!(
        "Task 9: pty::run_in_pty (pty=true) or Command::output (pty=false), then pty::clean_output"
    )
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
