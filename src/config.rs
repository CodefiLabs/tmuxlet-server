use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: Server,
    #[serde(default)]
    pub backends: HashMap<String, Backend>,
    #[serde(default)]
    pub chains: HashMap<String, Chain>,
}

#[derive(Debug, Deserialize)]
pub struct Server {
    pub listen: String,
    pub default_chain: String,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_env_source")]
    pub env_source: String,
    // Accepted for forward-compat; V1 logging is unleveled (plain eprintln).
    #[serde(default = "default_log_level")]
    #[allow(dead_code)]
    pub log_level: String,
}

fn default_timeout() -> u64 {
    1800
} // deviation #5: align with tmuxlet's own print-mode default
fn default_env_source() -> String {
    "shell".into()
}
fn default_log_level() -> String {
    "info".into()
}

#[derive(Debug, Deserialize)]
pub struct Chain {
    pub order: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Backend {
    Tmuxlet {
        target: String,
        #[serde(default)]
        target_args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    Api {
        base_url: String,
        model: String,
        #[serde(default)]
        api_key_env: Option<String>,
        #[serde(default)]
        extra_body: JsonValue,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    Cli {
        bin: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        pty: bool,
    },
}

pub fn load(path: &Path) -> Result<Config, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
    let mut cfg =
        parse(&text).map_err(|e| format!("failed to parse config {}: {e}", path.display()))?;
    expand_paths(&mut cfg);
    validate(&cfg)?;
    Ok(cfg)
}

pub fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

pub fn validate(cfg: &Config) -> Result<(), String> {
    for (cname, chain) in &cfg.chains {
        if chain.order.is_empty() {
            return Err(format!("chain '{cname}' has an empty order"));
        }
        for b in &chain.order {
            if !cfg.backends.contains_key(b) {
                return Err(format!(
                    "chain '{cname}' references undefined backend '{b}'"
                ));
            }
        }
    }
    if !cfg.chains.contains_key(&cfg.server.default_chain)
        && !cfg.backends.contains_key(&cfg.server.default_chain)
    {
        return Err(format!(
            "server.default_chain '{}' is not a defined chain or backend",
            cfg.server.default_chain
        ));
    }
    Ok(())
}

fn expand_tilde(s: &str) -> String {
    if s == "~" {
        return env::var("HOME").unwrap_or_else(|_| s.to_string());
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Ok(home) = env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    s.to_string()
}

fn expand_paths(cfg: &mut Config) {
    for b in cfg.backends.values_mut() {
        match b {
            Backend::Tmuxlet { cwd: Some(c), .. } => *c = expand_tilde(c),
            Backend::Cli { bin, .. } => *bin = expand_tilde(bin),
            _ => {}
        }
    }
}

pub fn tilde_path(s: &str) -> PathBuf {
    PathBuf::from(expand_tilde(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[server]
listen = "127.0.0.1:3456"
default_chain = "default"
request_timeout_secs = 1800
env_source = "shell"
log_level = "info"

[backends.agy]
type = "cli"
bin = "/Users/kk/.antigravity/antigravity/bin/agy"
args = ["-p"]
pty = true

[backends.ollama-kimi]
type = "api"
base_url = "http://127.0.0.1:11434/v1"
model = "kimi-k2.6:cloud"
extra_body = { think = true }
timeout_secs = 120

[backends.claude-thinking]
type = "tmuxlet"
target = "claude"
target_args = ["--effort", "max"]
cwd = "~"

[chains.default]
order = ["agy", "ollama-kimi", "claude-thinking"]
"#;

    #[test]
    fn parses_all_three_backend_types_and_extra_body() {
        let cfg = parse(SAMPLE).expect("sample must parse");
        assert_eq!(cfg.server.request_timeout_secs, 1800);
        assert_eq!(cfg.backends.len(), 3);
        match &cfg.backends["ollama-kimi"] {
            Backend::Api {
                extra_body,
                timeout_secs,
                ..
            } => {
                assert_eq!(extra_body["think"], serde_json::json!(true));
                assert_eq!(*timeout_secs, Some(120));
            }
            _ => panic!("ollama-kimi should be an api backend"),
        }
        assert_eq!(
            cfg.chains["default"].order,
            vec!["agy", "ollama-kimi", "claude-thinking"]
        );
    }

    #[test]
    fn validation_rejects_undefined_backend_in_chain() {
        let bad = format!("{SAMPLE}\n[chains.broken]\norder = [\"ghost\"]\n");
        let cfg = parse(&bad).unwrap();
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.contains("broken") && err.contains("ghost"),
            "got: {err}"
        );
    }

    #[test]
    fn validation_rejects_unknown_default_chain() {
        let mut cfg = parse(SAMPLE).unwrap();
        cfg.server.default_chain = "missing".into();
        assert!(validate(&cfg).unwrap_err().contains("default_chain"));
    }

    #[test]
    fn tilde_expands_to_home() {
        let cfg = parse(SAMPLE).unwrap();
        if let Ok(home) = std::env::var("HOME") {
            // expand_paths runs inside load(); call it directly here.
            let mut cfg = cfg;
            expand_paths(&mut cfg);
            match &cfg.backends["claude-thinking"] {
                Backend::Tmuxlet { cwd: Some(c), .. } => assert_eq!(*c, home),
                _ => panic!(),
            }
        }
    }
}
