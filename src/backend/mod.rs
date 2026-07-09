pub mod api;
pub mod cli;
pub mod tmuxlet;

use crate::config;
use crate::env::Env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug)]
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
    /// name, status code, and the first chars of the upstream error body (U-7).
    Http(String, u16, String),
    Parse(String, String),
    /// U-20: backend at its max_concurrent cap; the chain advances.
    Busy(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Timeout(n) => write!(f, "[{n}] timed out"),
            BackendError::Spawn(n, e) => write!(f, "[{n}] spawn failed: {e}"),
            BackendError::Exit(n, c, s) => write!(f, "[{n}] exited with {c}: {s}"),
            BackendError::Backend(n, m) => write!(f, "[{n}] backend error: {m}"),
            BackendError::Http(n, s, d) => {
                if d.is_empty() {
                    write!(f, "[{n}] HTTP {s}")
                } else {
                    write!(f, "[{n}] HTTP {s}: {d}")
                }
            }
            BackendError::Parse(n, m) => write!(f, "[{n}] parse error: {m}"),
            BackendError::Busy(n) => write!(f, "[{n}] busy (max_concurrent reached)"),
        }
    }
}

impl BackendError {
    /// The backend name this error is attributed to (for U-23 error grouping).
    pub fn backend_name(&self) -> &str {
        match self {
            BackendError::Timeout(n)
            | BackendError::Spawn(n, _)
            | BackendError::Exit(n, _, _)
            | BackendError::Backend(n, _)
            | BackendError::Http(n, _, _)
            | BackendError::Parse(n, _)
            | BackendError::Busy(n) => n,
        }
    }

    /// A short error class for the U-23 compact 503 summary.
    pub fn class(&self) -> &'static str {
        match self {
            BackendError::Timeout(_) => "timeout",
            BackendError::Spawn(_, _) => "spawn",
            BackendError::Exit(_, _, _) => "exit",
            BackendError::Backend(_, _) => "backend error",
            BackendError::Http(_, s, _) => match s {
                429 => "rate limited",
                _ => "http error",
            },
            BackendError::Parse(_, _) => "parse error",
            BackendError::Busy(_) => "busy",
        }
    }
}

pub struct TmuxletBackend {
    pub name: String,
    /// F-4: `tmuxlet` resolved to an absolute path from the captured env PATH at
    /// startup, so spawn failures are deterministic rather than PATH-dependent.
    pub bin: PathBuf,
    pub target: String,
    pub target_args: Vec<String>,
    pub cwd: PathBuf,
    pub allow_empty: bool,
    /// S-3: resolved env allowlist (per-backend or server default).
    pub env_pass: Option<Vec<String>>,
    /// S-4: use tmuxlet's stdin form (`-p ... -`) when the installed tmuxlet
    /// supports it (probed once at startup); otherwise argv.
    pub use_stdin: bool,
    /// U-20: max simultaneous dispatches (None = unlimited).
    pub max_concurrent: Option<usize>,
    /// U-14: prompt shaping mode.
    pub prompt_mode: crate::openai::PromptMode,
}

pub struct ApiBackend {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub extra_body: serde_json::Value,
    pub timeout: Option<Duration>,
    pub allow_empty: bool,
    /// S-5: cap on the upstream response body (server-level).
    pub max_response_bytes: u64,
    /// U-20: max simultaneous dispatches (None = unlimited).
    pub max_concurrent: Option<usize>,
}

pub struct CliBackend {
    pub name: String,
    pub bin: PathBuf,
    pub args: Vec<String>,
    pub pty: bool,
    pub cwd: PathBuf,
    /// U-12: (cols, rows).
    pub pty_size: (u16, u16),
    pub allow_empty: bool,
    /// S-3: resolved env allowlist (per-backend or server default).
    pub env_pass: Option<Vec<String>>,
    /// S-4: write the prompt to stdin instead of argv (plain mode).
    pub stdin_prompt: bool,
    /// U-20: max simultaneous dispatches (None = unlimited).
    pub max_concurrent: Option<usize>,
    /// U-14: prompt shaping mode.
    pub prompt_mode: crate::openai::PromptMode,
}

pub enum Backend {
    Tmuxlet(TmuxletBackend),
    Api(ApiBackend),
    Cli(CliBackend),
}

/// Resolve a program name to an absolute path using the captured env's PATH
/// (F-4). A name already containing a path separator is used verbatim. Falls
/// back to the bare name (spawn will then surface a deterministic failure).
pub fn resolve_program(program: &str, env: &Env) -> PathBuf {
    if program.contains('/') {
        return PathBuf::from(program);
    }
    if let Some(path) = env.get("PATH") {
        for dir in path.split(':').filter(|d| !d.is_empty()) {
            let candidate = Path::new(dir).join(program);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(program)
}

/// Server-level values threaded into each runtime backend at build time.
pub struct ServerDefaults {
    pub max_response_bytes: u64,
    /// S-3 server-level env allowlist (used when a backend defines none).
    pub env_pass: Option<Vec<String>>,
    /// S-4: whether the installed tmuxlet supports the stdin form.
    pub tmuxlet_stdin: bool,
}

impl Backend {
    pub fn from_config(
        name: &str,
        c: &config::Backend,
        env: &Env,
        defaults: &ServerDefaults,
    ) -> Backend {
        let home = || std::env::var("HOME").unwrap_or_else(|_| ".".into());
        // S-3: a per-backend env_pass overrides the server-level default.
        let resolve_env_pass =
            |own: &Option<Vec<String>>| own.clone().or_else(|| defaults.env_pass.clone());
        match c {
            config::Backend::Tmuxlet {
                target,
                target_args,
                cwd,
                allow_empty,
                env_pass,
                max_concurrent,
                prompt_mode,
            } => Backend::Tmuxlet(TmuxletBackend {
                name: name.into(),
                bin: resolve_program("tmuxlet", env),
                target: target.clone(),
                target_args: target_args.clone(),
                cwd: PathBuf::from(cwd.clone().unwrap_or_else(home)),
                allow_empty: *allow_empty,
                env_pass: resolve_env_pass(env_pass),
                use_stdin: defaults.tmuxlet_stdin,
                max_concurrent: *max_concurrent,
                prompt_mode: prompt_mode
                    .as_deref()
                    .and_then(crate::openai::PromptMode::parse)
                    .unwrap_or_default(),
            }),
            config::Backend::Api {
                base_url,
                model,
                api_key_env,
                extra_body,
                timeout_secs,
                allow_empty,
                max_concurrent,
            } => Backend::Api(ApiBackend {
                name: name.into(),
                base_url: base_url.clone(),
                model: model.clone(),
                api_key_env: api_key_env.clone(),
                extra_body: extra_body.clone(),
                timeout: timeout_secs.map(Duration::from_secs),
                allow_empty: *allow_empty,
                max_response_bytes: defaults.max_response_bytes,
                max_concurrent: *max_concurrent,
            }),
            config::Backend::Cli {
                bin,
                args,
                pty,
                cwd,
                pty_size,
                allow_empty,
                env_pass,
                stdin_prompt,
                max_concurrent,
                prompt_mode,
            } => Backend::Cli(CliBackend {
                name: name.into(),
                bin: PathBuf::from(bin),
                args: args.clone(),
                pty: *pty,
                cwd: PathBuf::from(cwd.clone().unwrap_or_else(home)),
                pty_size: pty_size
                    .as_ref()
                    .filter(|v| v.len() == 2)
                    .map(|v| (v[0], v[1]))
                    .unwrap_or((200, 50)),
                allow_empty: *allow_empty,
                env_pass: resolve_env_pass(env_pass),
                stdin_prompt: *stdin_prompt,
                max_concurrent: *max_concurrent,
                prompt_mode: prompt_mode
                    .as_deref()
                    .and_then(crate::openai::PromptMode::parse)
                    .unwrap_or_default(),
            }),
        }
    }

    /// The backend's configured name. Exercised by tests; production logging
    /// uses the router-resolved name directly.
    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        match self {
            Backend::Tmuxlet(b) => &b.name,
            Backend::Api(b) => &b.name,
            Backend::Cli(b) => &b.name,
        }
    }

    /// U-6: whether an empty completion is acceptable as success.
    pub fn allow_empty(&self) -> bool {
        match self {
            Backend::Tmuxlet(b) => b.allow_empty,
            Backend::Api(b) => b.allow_empty,
            Backend::Cli(b) => b.allow_empty,
        }
    }

    /// A per-backend timeout override (api backends only), else None.
    pub fn timeout_override(&self) -> Option<Duration> {
        match self {
            Backend::Api(b) => b.timeout,
            _ => None,
        }
    }

    /// U-20: max simultaneous dispatches to this backend (None = unlimited).
    pub fn max_concurrent(&self) -> Option<usize> {
        match self {
            Backend::Tmuxlet(b) => b.max_concurrent,
            Backend::Api(b) => b.max_concurrent,
            Backend::Cli(b) => b.max_concurrent,
        }
    }

    /// U-14: prompt shaping mode (api backends pass raw JSON, so Transcript).
    pub fn prompt_mode(&self) -> crate::openai::PromptMode {
        match self {
            Backend::Tmuxlet(b) => b.prompt_mode,
            Backend::Cli(b) => b.prompt_mode,
            Backend::Api(_) => crate::openai::PromptMode::Transcript,
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
            Backend::Api(b) => api::dispatch(b, raw_messages, env, timeout),
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
            BackendError::Http("ollama".into(), 500, String::new())
                .to_string()
                .contains("500")
        );
    }

    #[test]
    fn http_error_includes_body_detail() {
        let e = BackendError::Http("or".into(), 401, "invalid key".into());
        assert!(e.to_string().contains("invalid key"));
        assert_eq!(e.class(), "http error");
        assert_eq!(
            BackendError::Http("or".into(), 429, String::new()).class(),
            "rate limited"
        );
    }

    #[test]
    fn builds_runtime_backend_from_config() {
        let cfg = crate::config::parse(
            r#"
[server]
listen = "127.0.0.1:3456"
default_chain = "d"

[backends.t]
type = "tmuxlet"
target = "claude"

[chains.d]
order = ["t"]
"#,
        )
        .unwrap();
        let env = crate::env::Env::capture("process", "", 5);
        let defaults = ServerDefaults {
            max_response_bytes: 1024,
            env_pass: None,
            tmuxlet_stdin: false,
        };
        let b = Backend::from_config("t", &cfg.backends["t"], &env, &defaults);
        assert_eq!(b.name(), "t");
        assert!(matches!(b, Backend::Tmuxlet(_)));
    }
}
