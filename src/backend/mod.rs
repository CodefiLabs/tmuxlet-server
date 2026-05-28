pub mod api;
pub mod cli;
pub mod tmuxlet;

use crate::config;
use crate::env::Env;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

pub struct DispatchResult {
    pub content: String,
    pub model_label: String,
}

#[derive(Debug)]
pub enum BackendError {
    Timeout(String),
    Spawn(String, String),
    Exit(String, i32, String),
    Backend(String, String),
    Http(String, u16),
    Parse(String, String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Timeout(n) => write!(f, "[{n}] timed out"),
            BackendError::Spawn(n, e) => write!(f, "[{n}] spawn failed: {e}"),
            BackendError::Exit(n, c, s) => write!(f, "[{n}] exited with {c}: {s}"),
            BackendError::Backend(n, m) => write!(f, "[{n}] backend error: {m}"),
            BackendError::Http(n, s) => write!(f, "[{n}] HTTP {s}"),
            BackendError::Parse(n, m) => write!(f, "[{n}] parse error: {m}"),
        }
    }
}

pub struct TmuxletBackend {
    pub name: String,
    pub target: String,
    pub target_args: Vec<String>,
    pub cwd: PathBuf,
}

pub struct ApiBackend {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub extra_body: serde_json::Value,
}

pub struct CliBackend {
    pub name: String,
    pub bin: PathBuf,
    pub args: Vec<String>,
    pub pty: bool,
}

pub enum Backend {
    Tmuxlet(TmuxletBackend),
    Api(ApiBackend),
    Cli(CliBackend),
}

impl Backend {
    pub fn from_config(name: &str, c: &config::Backend) -> Backend {
        match c {
            config::Backend::Tmuxlet {
                target,
                target_args,
                cwd,
            } => Backend::Tmuxlet(TmuxletBackend {
                name: name.into(),
                target: target.clone(),
                target_args: target_args.clone(),
                cwd: PathBuf::from(
                    cwd.clone()
                        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| ".".into())),
                ),
            }),
            config::Backend::Api {
                base_url,
                model,
                api_key_env,
                extra_body,
                ..
            } => Backend::Api(ApiBackend {
                name: name.into(),
                base_url: base_url.clone(),
                model: model.clone(),
                api_key_env: api_key_env.clone(),
                extra_body: extra_body.clone(),
            }),
            config::Backend::Cli { bin, args, pty } => Backend::Cli(CliBackend {
                name: name.into(),
                bin: PathBuf::from(bin),
                args: args.clone(),
                pty: *pty,
            }),
        }
    }

    /// The backend's configured name. Exercised by tests; redundant with the
    /// router-resolved name in production logging.
    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        match self {
            Backend::Tmuxlet(b) => &b.name,
            Backend::Api(b) => &b.name,
            Backend::Cli(b) => &b.name,
        }
    }

    pub fn dispatch(
        &self,
        prompt: &str,
        raw_messages: &serde_json::Value,
        env: &Env,
        timeout: Duration,
    ) -> Result<DispatchResult, BackendError> {
        match self {
            Backend::Tmuxlet(b) => tmuxlet::dispatch(b, prompt, env, timeout),
            Backend::Api(b) => api::dispatch(b, raw_messages, env),
            Backend::Cli(b) => cli::dispatch(b, prompt, env, timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_name_the_backend() {
        assert!(
            BackendError::Timeout("agy".into())
                .to_string()
                .contains("agy")
        );
        assert!(
            BackendError::Http("ollama".into(), 500)
                .to_string()
                .contains("500")
        );
    }

    #[test]
    fn builds_runtime_backend_from_config() {
        let cfg = crate::config::parse(
            r#"
[server]
listen = "x"
default_chain = "d"

[backends.t]
type = "tmuxlet"
target = "claude"

[chains.d]
order = ["t"]
"#,
        )
        .unwrap();
        let b = Backend::from_config("t", &cfg.backends["t"]);
        assert_eq!(b.name(), "t");
        assert!(matches!(b, Backend::Tmuxlet(_)));
    }
}
