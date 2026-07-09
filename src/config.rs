use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::ToSocketAddrs;
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
    /// Watchdog for shell env capture (S-9): a blocking `~/.zshrc` must not hang
    /// startup forever. On expiry the capture is abandoned and the process env
    /// is used instead.
    #[serde(default = "default_env_capture_timeout")]
    pub env_capture_timeout_secs: u64,
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
fn default_env_capture_timeout() -> u64 {
    15
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
        /// U-6: treat empty output as failure so the chain advances. Set true to
        /// accept an empty completion as success.
        #[serde(default)]
        allow_empty: bool,
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
        #[serde(default)]
        allow_empty: bool,
    },
    Cli {
        bin: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        pty: bool,
        /// U-11: working directory for the child (both plain and PTY modes).
        /// Tilde-expanded. Defaults to `$HOME`.
        #[serde(default)]
        cwd: Option<String>,
        /// U-12: PTY dimensions as `[cols, rows]`. Defaults to `[200, 50]`.
        #[serde(default)]
        pty_size: Option<Vec<u16>>,
        #[serde(default)]
        allow_empty: bool,
    },
}

pub fn load(path: &Path) -> Result<(Config, Vec<String>), String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
    let mut cfg =
        parse(&text).map_err(|e| format!("failed to parse config {}: {e}", path.display()))?;
    expand_paths(&mut cfg);
    validate(&cfg)?;
    let warnings = lint(&cfg, &text);
    Ok((cfg, warnings))
}

pub fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Hard errors: conditions under which the server cannot (or must not) serve.
/// These fail both `--validate` and `serve`.
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
    // U-5: an unresolvable listen address can never bind — catch it here rather
    // than at a 2am service restart.
    if cfg.server.listen.as_str().to_socket_addrs().is_err() {
        return Err(format!(
            "server.listen '{}' is not a valid address:port (e.g. \"127.0.0.1:3456\")",
            cfg.server.listen
        ));
    }
    Ok(())
}

/// Soft issues (U-4 unknown keys, U-5 depth): errors under `--validate`,
/// warnings at serve time (UX invariant 2 — an upgrade must never brick a
/// running service over a nit). Returns human-readable messages, each naming
/// its fix (UX invariant 3).
pub fn lint(cfg: &Config, text: &str) -> Vec<String> {
    let mut out = Vec::new();

    // U-4: unknown keys (serde silently ignores them). Diff the parsed TOML
    // table against the known key set; suggest the nearest neighbor.
    for (key, suggestion) in unknown_keys(text) {
        match suggestion {
            Some(s) => out.push(format!("unknown config key '{key}' — did you mean '{s}'?")),
            None => out.push(format!("unknown config key '{key}'")),
        }
    }

    // U-5: chain/backend name collision — the router silently prefers the chain.
    for name in cfg.chains.keys() {
        if cfg.backends.contains_key(name) {
            out.push(format!(
                "'{name}' is both a chain and a backend; the chain shadows the backend in routing — rename one"
            ));
        }
    }

    // U-5: request_timeout_secs = 0 means "no deadline" for api but "instant
    // timeout" for cli/tmuxlet.
    if cfg.server.request_timeout_secs == 0 {
        out.push(
            "server.request_timeout_secs = 0 means 'no deadline' for api backends but 'instant timeout' for cli/tmuxlet — omit the key for the default (1800)"
                .into(),
        );
    }

    // U-5: unknown env_source value is silently treated as "shell".
    if !matches!(cfg.server.env_source.as_str(), "shell" | "process") {
        out.push(format!(
            "server.env_source '{}' is neither 'shell' nor 'process' — it will be treated as 'shell'",
            cfg.server.env_source
        ));
    }

    // U-12: pty_size must be [cols, rows].
    for (bname, b) in &cfg.backends {
        if let Backend::Cli {
            pty_size: Some(ps), ..
        } = b
            && ps.len() != 2
        {
            out.push(format!(
                "backends.{bname}.pty_size must be [cols, rows] (2 elements), got {} — using the default [200, 50]",
                ps.len()
            ));
        }
    }

    out
}

// ---- U-4 unknown-key detection ----

const TOP_KEYS: &[&str] = &["server", "backends", "chains"];
const SERVER_KEYS: &[&str] = &[
    "listen",
    "default_chain",
    "request_timeout_secs",
    "env_source",
    "log_level",
    "env_capture_timeout_secs",
];
const CHAIN_KEYS: &[&str] = &["order"];
const TMUXLET_KEYS: &[&str] = &["type", "target", "target_args", "cwd", "allow_empty"];
const API_KEYS: &[&str] = &[
    "type",
    "base_url",
    "model",
    "api_key_env",
    "extra_body",
    "timeout_secs",
    "allow_empty",
];
const CLI_KEYS: &[&str] = &[
    "type",
    "bin",
    "args",
    "pty",
    "cwd",
    "pty_size",
    "allow_empty",
];

/// Walk the parsed TOML table one level deep per known section and report keys
/// not in the known set, with the nearest known key as a suggestion.
fn unknown_keys(text: &str) -> Vec<(String, Option<String>)> {
    use toml::Value;
    let mut out = Vec::new();
    let table: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(_) => return out, // parse errors are surfaced elsewhere
    };
    for (k, _) in &table {
        if !TOP_KEYS.contains(&k.as_str()) {
            out.push((k.clone(), nearest(k, TOP_KEYS)));
        }
    }
    if let Some(Value::Table(server)) = table.get("server") {
        for (k, _) in server {
            if !SERVER_KEYS.contains(&k.as_str()) {
                out.push((format!("server.{k}"), nearest(k, SERVER_KEYS)));
            }
        }
    }
    if let Some(Value::Table(chains)) = table.get("chains") {
        for (cname, cval) in chains {
            if let Value::Table(c) = cval {
                for (k, _) in c {
                    if !CHAIN_KEYS.contains(&k.as_str()) {
                        out.push((format!("chains.{cname}.{k}"), nearest(k, CHAIN_KEYS)));
                    }
                }
            }
        }
    }
    if let Some(Value::Table(backends)) = table.get("backends") {
        for (bname, bval) in backends {
            if let Value::Table(b) = bval {
                let ty = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let allowed: &[&str] = match ty {
                    "tmuxlet" => TMUXLET_KEYS,
                    "api" => API_KEYS,
                    "cli" => CLI_KEYS,
                    _ => &["type"],
                };
                for (k, _) in b {
                    if !allowed.contains(&k.as_str()) {
                        out.push((format!("backends.{bname}.{k}"), nearest(k, allowed)));
                    }
                }
            }
        }
    }
    out
}

/// Nearest known key by Levenshtein distance, if within a small threshold.
fn nearest(key: &str, candidates: &[&str]) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        if *c == "type" {
            continue; // never suggest the discriminant
        }
        let d = levenshtein(key, c);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, c));
        }
    }
    // Only suggest when the typo is close (<= a third of the key length, min 2).
    best.and_then(|(d, c)| {
        let threshold = (key.len() / 3).max(2);
        (d <= threshold).then(|| c.to_string())
    })
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
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
            Backend::Cli { bin, cwd, .. } => {
                *bin = expand_tilde(bin);
                if let Some(c) = cwd {
                    *c = expand_tilde(c);
                }
            }
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
        assert_eq!(cfg.server.env_capture_timeout_secs, 15);
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
    fn validation_rejects_unparseable_listen() {
        let mut cfg = parse(SAMPLE).unwrap();
        cfg.server.listen = "not-an-address".into();
        assert!(validate(&cfg).unwrap_err().contains("listen"));
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

    #[test]
    fn lint_flags_unknown_key_with_suggestion() {
        let bad = SAMPLE.replace("request_timeout_secs = 1800", "request_timeout_sec = 1800");
        let cfg = parse(&bad).unwrap();
        let warnings = lint(&cfg, &bad);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("request_timeout_sec") && w.contains("request_timeout_secs")),
            "expected a did-you-mean suggestion, got: {warnings:?}"
        );
    }

    #[test]
    fn lint_flags_unknown_backend_key() {
        let bad = SAMPLE.replace("pty = true", "pty = true\nptysize = [100, 40]");
        let cfg = parse(&bad).unwrap();
        let warnings = lint(&cfg, &bad);
        assert!(
            warnings.iter().any(|w| w.contains("backends.agy.ptysize")),
            "got: {warnings:?}"
        );
    }

    #[test]
    fn lint_flags_name_collision_and_zero_timeout() {
        let bad = format!("{SAMPLE}\n[chains.agy]\norder = [\"ollama-kimi\"]\n")
            .replace("request_timeout_secs = 1800", "request_timeout_secs = 0");
        let cfg = parse(&bad).unwrap();
        let warnings = lint(&cfg, &bad);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("both a chain and a backend")),
            "collision: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("request_timeout_secs = 0")),
            "zero timeout: {warnings:?}"
        );
    }

    #[test]
    fn clean_config_lints_clean() {
        let cfg = parse(SAMPLE).unwrap();
        assert!(lint(&cfg, SAMPLE).is_empty(), "{:?}", lint(&cfg, SAMPLE));
    }

    #[test]
    fn cli_cwd_tilde_expands() {
        let src = SAMPLE.replace("pty = true", "pty = true\ncwd = \"~\"");
        let mut cfg = parse(&src).unwrap();
        expand_paths(&mut cfg);
        if let Ok(home) = std::env::var("HOME") {
            match &cfg.backends["agy"] {
                Backend::Cli { cwd: Some(c), .. } => assert_eq!(*c, home),
                _ => panic!(),
            }
        }
    }
}
