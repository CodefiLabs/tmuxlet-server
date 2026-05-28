# tmuxlet-server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a single static Rust binary that exposes an OpenAI-compatible `/v1/chat/completions` endpoint backed by a TOML-configured fallback chain of three backend types (`tmuxlet`, `api`, `cli`).

**Architecture:** Synchronous (no async/tokio). `tiny_http` accept loop with an N-worker thread pool over `Arc<Server>`; each worker resolves a chain from the request `model`, walks its backends in order, and returns OpenAI-shaped JSON or SSE. Backends are a closed runtime enum built from the parsed config. `rustls` (ring backend) powers the `api` backend's hand-rolled HTTPS client; `portable-pty` powers the `cli` backend.

**Tech Stack:** Rust 1.92 / edition 2024. Deps: `tiny_http 0.12`, `ctrlc 3.4`, `serde 1.0.228`, `serde_json 1.0.150`, `toml 1.1`, `rustls 0.23.40` (ring), `webpki-roots 1.0.7`, `portable-pty 0.9.0`.

---

## Pre-flight: verified facts, deviations from spec, and decisions

These were verified against the real `tmuxlet` 0.3.0 source, the machine environment, and live crate docs (2026-05-28). **The design spec is authoritative; the items below are corrections where the spec disagrees with reality, plus three operator decisions already made.**

### Spec deviations (apply these — do not implement the spec's literal text where it conflicts)

| # | Spec text | Reality (verified) | This plan does |
|---|-----------|--------------------|----------------|
| 1 | tmuxlet success = `status == "success"` | `tmuxlet`'s `result_json()` emits `{"id","target","status","output","cwd","tmuxSession","completionSource","elapsedMs"}`; `result_status_code()` treats **`status == "completed"`** as exit 0 | parse `status == "completed"` as success (Task 7) |
| 2 | agy at `/Users/kevnk/.local/bin/agy` | actual: `/Users/kk/.antigravity/antigravity/bin/agy` (a symlink) | example config + AGENTS.md use the correct path (Task 14) |
| 3 | env capture via `$SHELL -lc env` | agy's `PATH` is exported in `~/.zshrc`, which zsh sources only for **interactive** shells; `-lc` (login, non-interactive) misses it | env capture uses `$SHELL -ilc env` + tolerant line parsing that drops prompt/escape noise (Task 4) |
| 4 | agy is the primary backend | on this machine `agy` is a **broken symlink** → `/Applications/Antigravity.app/...` (app not installed) | plan + scaffold do not require agy; smoke test (Task 14) documents that agy must be reinstalled before it passes against the real binary |
| 5 | `request_timeout_secs = 120` global | `tmuxlet`'s own print-mode `--timeout` default is 1800s; coding turns run minutes | server-wide default **1800s**, plus optional per-backend `timeout_secs` override (Task 2); `api` backends set something low (Task 14 example) |

### Decisions (locked)

- **rustls `ring` backend**, not the default aws-lc-rs — avoids a C compiler + cmake at build time. Requires `CryptoProvider::install_default()` once at startup (Task 12).
- **No `regex` crate** — `cli`/PTY output cleanup uses a dependency-free byte-filter state machine (Task 10).
- **No `wait-timeout` crate** — process timeout uses a stdlib `try_wait()` polling watchdog (avoids `wait-timeout`'s global SIGCHLD handler); PTY timeout uses `portable-pty`'s `clone_killer()` (Task 10).
- **`Cargo.lock` is committed** (binary crate, matches `tmuxlet` convention). `.gitignore` = `/target` only.

### Streaming limitation (V1, documented — not a bug)

`tmuxlet`/`cli` backends only emit output after the child exits; `api` is a single non-streamed HTTP call. So when `stream=true`, V1 produces the full response, then emits the SSE frames (role-prime → one content frame → final → `[DONE]`) all at once. This matches the contract Hermes was built against. V2 wires true incremental streaming.

---

## File structure

```
tmuxlet-server/
├── Cargo.toml            # deps + [[bin]] metadata (Task 1)
├── Cargo.lock            # committed
├── README.md             # human-facing (Task 14)
├── AGENTS.md             # agent-facing install/verify script-in-prose (Task 14)
├── examples/
│   ├── server.toml       # canonical config (Task 14)
│   ├── launchagent.plist # macOS template (Task 14)
│   └── tmuxlet-server.service  # systemd template (Task 14)
├── scripts/
│   └── smoke.sh          # manual end-to-end check (Task 14)
├── .github/workflows/ci.yml   # fmt + clippy + test, macOS+Linux (Task 15)
├── tests/
│   ├── server_starts.rs  # (Task 13)
│   ├── routes.rs         # (Task 13)
│   ├── chain_fallback.rs # (Task 13)
│   ├── streaming.rs      # (Task 13)
│   └── fixtures/
│       ├── mock-tmuxlet  # shell script → canned tmuxlet JSON
│       ├── mock-cli      # shell script → echoes prompt
│       └── mock-api.rs   # tiny_http instance with canned chat completion
└── src/
    ├── main.rs           # CLI flags, startup, serve bootstrap (Tasks 1, 12)
    ├── config.rs         # TOML parse + validation + tilde (Task 2)
    ├── openai.rs         # OpenAI request/response/chunk types + helpers (Task 3)
    ├── env.rs            # login-shell env capture (Task 4)
    ├── router.rs         # chain/backend resolution (Task 5)
    ├── http.rs           # routing, SSE writer, handlers, serve loop (Task 11)
    ├── pty.rs            # PTY runner + output cleanup (Task 10)
    └── backend/
        ├── mod.rs        # runtime Backend enum + dispatch + error model (Task 6)
        ├── tmuxlet.rs    # spawn `tmuxlet -p ...`, parse JSON (Task 7)
        ├── api.rs        # forward to OpenAI-compatible HTTP (Task 8)
        └── cli.rs        # exec binary (PTY or plain), clean output (Task 9)
```

Each `src` file owns one concern and stays ~100–300 LOC. The runtime `backend::Backend` enum (name-bearing, used at dispatch) is distinct from `config::Backend` (serde, name comes from the map key); Task 6 defines the conversion.

---

## Shared type contract

Every task below uses these exact names/signatures. A mismatch between tasks is a bug — keep them identical.

**`config.rs`** (deserialized form):
```rust
pub struct Config { pub server: Server, pub backends: HashMap<String, Backend>, pub chains: HashMap<String, Chain> }
pub struct Server { pub listen: String, pub default_chain: String, pub request_timeout_secs: u64, pub env_source: String, pub log_level: String }
pub struct Chain { pub order: Vec<String> }
pub enum Backend {  // #[serde(tag="type", rename_all="lowercase")]
    Tmuxlet { target: String, target_args: Vec<String>, cwd: Option<String> },
    Api { base_url: String, model: String, api_key_env: Option<String>, extra_body: serde_json::Value, timeout_secs: Option<u64> },
    Cli { bin: String, args: Vec<String>, pty: bool },
}
```

**`backend/mod.rs`** (runtime form):
```rust
pub enum Backend { Tmuxlet(TmuxletBackend), Api(ApiBackend), Cli(CliBackend) }  // each carries `name`
pub struct DispatchResult { pub content: String, pub model_label: String }
pub enum BackendError { Timeout(String), Spawn(String,String), Exit(String,i32,String), Backend(String,String), Http(String,u16), Parse(String,String) }
impl Backend { pub fn name(&self) -> &str; pub fn timeout(&self, default: Duration) -> Duration;
    pub fn dispatch(&self, prompt:&str, raw_messages:&serde_json::Value, env:&Env, default_timeout:Duration) -> Result<DispatchResult, BackendError>; }
```

**`env.rs`**: `pub struct Env(Arc<HashMap<String,String>>); impl Env { pub fn capture(source:&str, shell:&str)->Env; pub fn get(&self,k:&str)->Option<&str>; pub fn as_pairs(&self)->Vec<(&str,&str)>; }`

**`openai.rs`**: `ChatRequest, ChatMessage, MessageContent, ContentPart, ChatCompletion, Choice, ResponseMessage, Usage, ChatChunk, ChunkChoice, Delta, ModelList, ModelInfo, ErrorEnvelope, ApiError` + `flatten_to_prompt(&[ChatMessage])->String`, `build_completion(id,model,content)->ChatCompletion`, `stream_frames(id,model,content)->Vec<String>`.

**`router.rs`**: `pub fn resolve<'a>(model:&str, cfg:&'a Config) -> Result<Vec<&'a str>, String>` (returns ordered backend names).

---

## Task 1: Project skeleton — Cargo.toml, main.rs flags, `--version`/`--validate`/`--help`

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/config.rs` (empty stub for now: `pub fn placeholder() {}` — fleshed out in Task 2)

- [ ] **Step 1: Write `Cargo.toml`**

```toml
[package]
name = "tmuxlet-server"
version = "0.1.0"
edition = "2024"
description = "OpenAI-compatible HTTP server fronting tmuxlet, CLI tools, and API upstreams via a configurable fallback chain"
license = "MIT"
repository = "https://github.com/CodefiLabs/tmuxlet-server"

[[bin]]
name = "tmuxlet-server"
path = "src/main.rs"

[dependencies]
tiny_http = "0.12"
ctrlc = { version = "3.4", features = ["termination"] }
serde = { version = "1.0.228", features = ["derive"] }
serde_json = "1.0.150"
toml = "1.1"
rustls = { version = "0.23.40", default-features = false, features = ["ring", "std", "tls12", "logging"] }
webpki-roots = "1.0.7"
portable-pty = "0.9.0"
```

- [ ] **Step 2: Write `src/main.rs` with flag parsing only (serve path stubbed)**

```rust
mod config;

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_CONFIG: &str = "~/.tmuxlet/server.toml";

struct Args { config: String, validate: bool }

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args { config: DEFAULT_CONFIG.to_string(), validate: false };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--version" | "-V" => { println!("tmuxlet-server {VERSION}"); return ExitCode::SUCCESS; }
            "--help" | "-h" => { print_help(); return ExitCode::SUCCESS; }
            "--validate" => args.validate = true,
            "--config" => { i += 1; match argv.get(i) { Some(p) => args.config = p.clone(), None => { eprintln!("tmuxlet-server: --config requires a path"); return ExitCode::FAILURE; } } }
            other => { eprintln!("tmuxlet-server: unknown argument: {other}"); return ExitCode::FAILURE; }
        }
        i += 1;
    }

    if args.validate {
        // Real in Task 2; for now just report the path we'd validate.
        eprintln!("validate: {} (not yet implemented)", args.config);
        return ExitCode::SUCCESS;
    }

    // Serve path wired in Task 12.
    eprintln!("serve: not yet implemented");
    ExitCode::SUCCESS
}

fn print_help() {
    println!("tmuxlet-server {VERSION}\n\nUsage:\n  tmuxlet-server [--config FILE]    Start the server\n  tmuxlet-server --validate         Validate config and exit\n  tmuxlet-server --version          Print version\n  tmuxlet-server --help             Print this help");
}
```

- [ ] **Step 3: Write the `config.rs` placeholder so it compiles**

```rust
// Replaced in Task 2.
#[allow(dead_code)]
pub fn placeholder() {}
```

- [ ] **Step 4: Build and verify version output**

Run: `cargo build && cargo run -- --version`
Expected: compiles (downloads ~25 crates including rustls/ring — confirms the dependency cluster resolves), prints `tmuxlet-server 0.1.0`.

- [ ] **Step 5: Verify `--help` and unknown-arg handling**

Run: `cargo run -- --help` (expect usage), then `cargo run -- --bogus` (expect `unknown argument` on stderr, exit 1: `echo $?` → 1).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs src/config.rs
git commit -m "scaffold project: Cargo.toml deps + CLI flag parsing"
```

---

## Task 2: `config.rs` — TOML parse, validation, tilde expansion

**Files:**
- Modify: `src/config.rs` (replace placeholder)
- Test: inline `#[cfg(test)]` in `src/config.rs`

- [ ] **Step 1: Write the failing test**

```rust
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
            Backend::Api { extra_body, timeout_secs, .. } => {
                assert_eq!(extra_body["think"], serde_json::json!(true));
                assert_eq!(*timeout_secs, Some(120));
            }
            _ => panic!("ollama-kimi should be an api backend"),
        }
        assert_eq!(cfg.chains["default"].order, vec!["agy", "ollama-kimi", "claude-thinking"]);
    }

    #[test]
    fn validation_rejects_undefined_backend_in_chain() {
        let bad = format!("{SAMPLE}\n[chains.broken]\norder = [\"ghost\"]\n");
        let cfg = parse(&bad).unwrap();
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("broken") && err.contains("ghost"), "got: {err}");
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
        // safety_assert: HOME is set in test env
        if let Ok(home) = std::env::var("HOME") {
            match &cfg.backends["claude-thinking"] {
                Backend::Tmuxlet { cwd: Some(c), .. } => assert_eq!(*c, home),
                _ => panic!(),
            }
        }
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib config 2>&1 | head -20`
Expected: FAIL — `parse`, `validate`, `Backend` not found.

- [ ] **Step 3: Implement `config.rs`**

```rust
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use serde::Deserialize;
use serde_json::Value as JsonValue;

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
    #[serde(default = "default_log_level")]
    pub log_level: String,
}
fn default_timeout() -> u64 { 1800 }            // deviation #5: align with tmuxlet
fn default_env_source() -> String { "shell".into() }
fn default_log_level() -> String { "info".into() }

#[derive(Debug, Deserialize)]
pub struct Chain { pub order: Vec<String> }

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Backend {
    Tmuxlet {
        target: String,
        #[serde(default)] target_args: Vec<String>,
        #[serde(default)] cwd: Option<String>,
    },
    Api {
        base_url: String,
        model: String,
        #[serde(default)] api_key_env: Option<String>,
        #[serde(default)] extra_body: JsonValue,   // self-describing toml deserialize_any
        #[serde(default)] timeout_secs: Option<u64>,
    },
    Cli {
        bin: String,
        #[serde(default)] args: Vec<String>,
        #[serde(default)] pty: bool,
    },
}

pub fn load(path: &Path) -> Result<Config, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
    let mut cfg = parse(&text).map_err(|e| format!("failed to parse config {}: {e}", path.display()))?;
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
                return Err(format!("chain '{cname}' references undefined backend '{b}'"));
            }
        }
    }
    if !cfg.chains.contains_key(&cfg.server.default_chain)
        && !cfg.backends.contains_key(&cfg.server.default_chain)
    {
        return Err(format!("server.default_chain '{}' is not a defined chain or backend", cfg.server.default_chain));
    }
    Ok(())
}

fn expand_tilde(s: &str) -> String {
    if s == "~" { return env::var("HOME").unwrap_or_else(|_| s.to_string()); }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") { return format!("{home}/{rest}"); }
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib config`
Expected: 4 passed.

- [ ] **Step 5: Wire `--validate` into `main.rs`**

Replace the `if args.validate { ... }` block:
```rust
    if args.validate {
        let path = config::tilde_path(&args.config);
        match config::load(&path) {
            Ok(cfg) => { println!("OK ({} backends, {} chains)", cfg.backends.len(), cfg.chains.len()); return ExitCode::SUCCESS; }
            Err(e) => { eprintln!("tmuxlet-server: {e}"); return ExitCode::FAILURE; }
        }
    }
```
Add to `config.rs` a small public helper so `main` can expand the config path:
```rust
pub fn tilde_path(s: &str) -> std::path::PathBuf { std::path::PathBuf::from(expand_tilde(s)) }
```

- [ ] **Step 6: Verify against a real file**

Run: `mkdir -p /tmp/tmtest && cp examples/server.toml /tmp/tmtest/ 2>/dev/null; cargo run -- --config /tmp/tmtest/server.toml --validate` (after Task 14 writes the example; for now use the SAMPLE saved to a temp file).
Expected: `OK (N backends, M chains)`.

- [ ] **Step 7: Commit**

```bash
git add src/config.rs src/main.rs
git commit -m "config: TOML parse, validation, tilde expansion, --validate"
```

---

## Task 3: `openai.rs` — request/response/chunk types and helpers

**Files:**
- Create: `src/openai.rs`
- Modify: `src/main.rs` (add `mod openai;`)
- Test: inline

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_and_array_content() {
        let s = r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"stream":true,"seed":42}"#;
        let p = r#"{"model":"x","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        let a: ChatRequest = serde_json::from_str(s).unwrap();
        let b: ChatRequest = serde_json::from_str(p).unwrap();
        assert!(a.stream);
        assert!(!b.stream);
        assert!(a.extra.contains_key("seed"));
    }

    #[test]
    fn flattens_messages_with_role_labels() {
        let req: ChatRequest = serde_json::from_str(
            r#"{"model":"x","messages":[{"role":"system","content":"S"},{"role":"user","content":"U"}]}"#
        ).unwrap();
        assert_eq!(flatten_to_prompt(&req.messages), "[System]: S\n\nUser: U");
    }

    #[test]
    fn final_stream_delta_serializes_empty() {
        let frames = stream_frames("id1", "agy", "Hello");
        // role-prime, content, final, [DONE]
        assert_eq!(frames.len(), 4);
        assert!(frames[0].contains(r#""role":"assistant""#));
        assert!(frames[1].contains(r#""content":"Hello""#));
        assert!(frames[2].contains(r#""delta":{}"#));
        assert!(frames[2].contains(r#""finish_reason":"stop""#));
        assert_eq!(frames[3], "data: [DONE]\n\n");
    }

    #[test]
    fn completion_has_usage_object() {
        let c = build_completion("id1".into(), "agy".into(), "hi".into());
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains(r#""object":"chat.completion""#));
        assert!(j.contains(r#""usage""#));
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --lib openai 2>&1 | head`. Expected: type/fn not found.

- [ ] **Step 3: Implement `openai.rs`**

```rust
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};

// ---------- Request ----------
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)] pub stream: bool,
    #[serde(flatten)] pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: MessageContent,
    #[serde(default)] pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MessageContent { Text(String), Parts(Vec<ContentPart>) }

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")] Text { text: String },
    #[serde(other)] Other,
}

fn content_to_text(c: &MessageContent) -> String {
    match c {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Parts(ps) => ps.iter().filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Other => None,
        }).collect::<Vec<_>>().join(""),
    }
}

pub fn flatten_to_prompt(messages: &[ChatMessage]) -> String {
    let mut blocks = Vec::with_capacity(messages.len());
    for m in messages {
        let label = match m.role.as_str() {
            "system" => "[System]".to_string(),
            "user" => "User".to_string(),
            "assistant" => "Assistant".to_string(),
            other => { let mut c = other.chars(); match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(), None => String::new() } }
        };
        blocks.push(format!("{}: {}", label, content_to_text(&m.content)));
    }
    blocks.join("\n\n")
}

// ---------- Non-streaming response ----------
#[derive(Debug, Serialize)]
pub struct ChatCompletion { pub id: String, pub object: &'static str, pub created: u64, pub model: String, pub choices: Vec<Choice>, pub usage: Usage }
#[derive(Debug, Serialize)]
pub struct Choice { pub index: u32, pub message: ResponseMessage, pub finish_reason: String }
#[derive(Debug, Serialize)]
pub struct ResponseMessage { pub role: &'static str, pub content: String }
#[derive(Debug, Serialize)]
pub struct Usage { pub prompt_tokens: u32, pub completion_tokens: u32, pub total_tokens: u32 }

fn unix_now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

pub fn build_completion(id: String, model: String, content: String) -> ChatCompletion {
    ChatCompletion { id, object: "chat.completion", created: unix_now(), model,
        choices: vec![Choice { index: 0, message: ResponseMessage { role: "assistant", content }, finish_reason: "stop".into() }],
        usage: Usage { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 } }
}

// ---------- Streaming ----------
#[derive(Debug, Serialize)]
pub struct ChatChunk { pub id: String, pub object: &'static str, pub created: u64, pub model: String, pub choices: Vec<ChunkChoice> }
#[derive(Debug, Serialize)]
pub struct ChunkChoice { pub index: u32, pub delta: Delta, pub finish_reason: Option<String> }
#[derive(Debug, Default, Serialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")] pub role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")] pub content: Option<String>,
}

/// V1 buffered streaming: role-prime, one content frame, final, then [DONE].
pub fn stream_frames(id: &str, model: &str, content: &str) -> Vec<String> {
    let created = unix_now();
    let frame = |c: ChunkChoice| {
        let chunk = ChatChunk { id: id.into(), object: "chat.completion.chunk", created, model: model.into(), choices: vec![c] };
        format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
    };
    vec![
        frame(ChunkChoice { index: 0, delta: Delta { role: Some("assistant"), content: Some(String::new()) }, finish_reason: None }),
        frame(ChunkChoice { index: 0, delta: Delta { role: None, content: Some(content.to_string()) }, finish_reason: None }),
        frame(ChunkChoice { index: 0, delta: Delta { role: None, content: None }, finish_reason: Some("stop".into()) }),
        "data: [DONE]\n\n".to_string(),
    ]
}

// ---------- /v1/models ----------
#[derive(Debug, Serialize)]
pub struct ModelList { pub object: &'static str, pub data: Vec<ModelInfo> }
#[derive(Debug, Serialize)]
pub struct ModelInfo { pub id: String, pub object: &'static str, pub created: u64, pub owned_by: String }

pub fn model_list(ids: impl Iterator<Item = String>) -> ModelList {
    let created = unix_now();
    ModelList { object: "list", data: ids.map(|id| ModelInfo { id, object: "model", created, owned_by: "tmuxlet-server".into() }).collect() }
}

// ---------- Errors ----------
#[derive(Debug, Serialize)]
pub struct ErrorEnvelope { pub error: ApiError }
#[derive(Debug, Serialize)]
pub struct ApiError { pub message: String, #[serde(rename = "type")] pub error_type: String, pub param: Option<String>, pub code: Option<String> }

impl ErrorEnvelope {
    pub fn new(message: impl Into<String>, error_type: &str, code: Option<&str>) -> Self {
        ErrorEnvelope { error: ApiError { message: message.into(), error_type: error_type.into(), param: None, code: code.map(|s| s.to_string()) } }
    }
    pub fn to_json(&self) -> String { serde_json::to_string(self).unwrap() }
}
```

- [ ] **Step 4: Run tests** — `cargo test --lib openai`. Expected: 4 passed.
- [ ] **Step 5: Commit** — `git add src/openai.rs src/main.rs && git commit -m "openai: request/response/chunk types + prompt flattening + sse frames"`

---

## Task 4: `env.rs` — login-shell env capture

**Files:** Create `src/env.rs`; add `mod env;` to `main.rs`; inline tests.

> **Deviation #3:** capture uses `$SHELL -ilc env` (interactive login) so `~/.zshrc`-exported PATH entries (e.g. agy) are present. The parser tolerates prompt/escape-code lines by keeping only well-formed `KEY=value` lines where KEY matches `[A-Za-z_][A-Za-z0-9_]*`.

- [ ] **Step 1: Failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_keyvalue_skips_noise() {
        let raw = "PATH=/usr/bin:/bin\nHOME=/Users/kk\n\x1b]7;file://x\x1b\\garbage line\nVAR_2=a=b=c\nbad key=nope\n";
        let m = parse_env(raw);
        assert_eq!(m.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
        assert_eq!(m.get("VAR_2").map(String::as_str), Some("a=b=c")); // value may contain '='
        assert!(!m.contains_key("garbage line"));
        assert!(!m.contains_key("bad key"));
    }
}
```

- [ ] **Step 2: Run, verify fail** — `cargo test --lib env 2>&1 | head`.

- [ ] **Step 3: Implement `env.rs`**

```rust
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::Arc;

#[derive(Clone)]
pub struct Env(Arc<HashMap<String, String>>);

impl Env {
    /// `source` is "shell" (run $SHELL -ilc env) or "process" (inherit current env).
    pub fn capture(source: &str, shell: &str) -> Env {
        let map = match source {
            "process" => std::env::vars().collect(),
            _ => capture_shell(shell).unwrap_or_else(|_| std::env::vars().collect()),
        };
        Env(Arc::new(map))
    }
    pub fn get(&self, k: &str) -> Option<&str> { self.0.get(k).map(String::as_str) }
    pub fn as_pairs(&self) -> Vec<(&str, &str)> { self.0.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect() }
}

fn capture_shell(shell: &str) -> std::io::Result<HashMap<String, String>> {
    // -i interactive (sources ~/.zshrc), -l login, -c env.
    let out = Command::new(shell).args(["-ilc", "env"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).output()?;
    Ok(parse_env(&String::from_utf8_lossy(&out.stdout)))
}

fn is_valid_key(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn parse_env(raw: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in raw.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if is_valid_key(k) { m.insert(k.to_string(), v.to_string()); }
        }
    }
    m
}
```

- [ ] **Step 4: Run tests** — `cargo test --lib env`. Expected: 1 passed.
- [ ] **Step 5: Commit** — `git add src/env.rs src/main.rs && git commit -m "env: interactive-login shell env capture + tolerant parser"`

---

## Task 5: `router.rs` — chain/backend resolution

**Files:** Create `src/router.rs`; add `mod router;`; inline tests.

Resolution order: (1) `model` matches a chain → that chain's `order`; (2) matches a backend → single-element chain; (3) otherwise → `server.default_chain` (which may itself be a chain or a single backend).

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;
    const C: &str = r#"
[server]
listen="127.0.0.1:3456"
default_chain="default"
[backends.a]
type="cli"
bin="/bin/echo"
[backends.b]
type="cli"
bin="/bin/echo"
[chains.default]
order=["a","b"]
"#;
    #[test] fn resolves_chain_name() { let cfg = parse(C).unwrap(); assert_eq!(resolve("default", &cfg).unwrap(), vec!["a","b"]); }
    #[test] fn resolves_backend_name_as_singleton() { let cfg = parse(C).unwrap(); assert_eq!(resolve("a", &cfg).unwrap(), vec!["a"]); }
    #[test] fn unknown_model_falls_back_to_default() { let cfg = parse(C).unwrap(); assert_eq!(resolve("zzz", &cfg).unwrap(), vec!["a","b"]); }
    #[test] fn empty_model_falls_back_to_default() { let cfg = parse(C).unwrap(); assert_eq!(resolve("", &cfg).unwrap(), vec!["a","b"]); }
}
```

- [ ] **Step 2: Run, verify fail.**

- [ ] **Step 3: Implement `router.rs`**

```rust
use crate::config::Config;

/// Returns the ordered list of backend names to try for the given request model.
pub fn resolve<'a>(model: &str, cfg: &'a Config) -> Result<Vec<&'a str>, String> {
    if let Some(chain) = cfg.chains.get(model) {
        return Ok(chain.order.iter().map(String::as_str).collect());
    }
    if cfg.backends.contains_key(model) {
        return Ok(vec![cfg.backends.get_key_value(model).unwrap().0.as_str()]);
    }
    // fall back to default_chain (chain or single backend)
    let dc = &cfg.server.default_chain;
    if let Some(chain) = cfg.chains.get(dc) {
        Ok(chain.order.iter().map(String::as_str).collect())
    } else if cfg.backends.contains_key(dc) {
        Ok(vec![cfg.backends.get_key_value(dc).unwrap().0.as_str()])
    } else {
        Err(format!("default_chain '{dc}' resolves to nothing"))
    }
}
```

- [ ] **Step 4: Run tests** — `cargo test --lib router`. Expected: 4 passed.
- [ ] **Step 5: Commit** — `git commit -m "router: model -> chain/backend resolution with default fallback"`

---

## Task 6: `backend/mod.rs` — runtime Backend enum, dispatch, error model

**Files:** Create `src/backend/mod.rs`; add `mod backend;`; inline tests for error formatting and config→runtime conversion.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_messages_name_the_backend() {
        assert!(BackendError::Timeout("agy".into()).to_string().contains("agy"));
        assert!(BackendError::Http("ollama".into(), 500).to_string().contains("500"));
    }
    #[test]
    fn builds_runtime_backend_from_config() {
        let cfg = crate::config::parse(r#"
[server]
listen="x"
default_chain="d"
[backends.t]
type="tmuxlet"
target="claude"
[chains.d]
order=["t"]
"#).unwrap();
        let b = Backend::from_config("t", &cfg.backends["t"]);
        assert_eq!(b.name(), "t");
        assert!(matches!(b, Backend::Tmuxlet(_)));
    }
}
```

- [ ] **Step 2: Run, verify fail.**

- [ ] **Step 3: Implement `backend/mod.rs`**

```rust
pub mod tmuxlet;
pub mod api;
pub mod cli;

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;
use crate::config;
use crate::env::Env;

pub struct DispatchResult { pub content: String, pub model_label: String }

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

pub struct TmuxletBackend { pub name: String, pub target: String, pub target_args: Vec<String>, pub cwd: PathBuf }
pub struct ApiBackend { pub name: String, pub base_url: String, pub model: String, pub api_key_env: Option<String>, pub extra_body: serde_json::Value }
pub struct CliBackend { pub name: String, pub bin: PathBuf, pub args: Vec<String>, pub pty: bool }

pub enum Backend { Tmuxlet(TmuxletBackend), Api(ApiBackend), Cli(CliBackend) }

impl Backend {
    pub fn from_config(name: &str, c: &config::Backend) -> Backend {
        match c {
            config::Backend::Tmuxlet { target, target_args, cwd } => Backend::Tmuxlet(TmuxletBackend {
                name: name.into(), target: target.clone(), target_args: target_args.clone(),
                cwd: PathBuf::from(cwd.clone().unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| ".".into()))),
            }),
            config::Backend::Api { base_url, model, api_key_env, extra_body, .. } => Backend::Api(ApiBackend {
                name: name.into(), base_url: base_url.clone(), model: model.clone(),
                api_key_env: api_key_env.clone(), extra_body: extra_body.clone(),
            }),
            config::Backend::Cli { bin, args, pty } => Backend::Cli(CliBackend {
                name: name.into(), bin: PathBuf::from(bin), args: args.clone(), pty: *pty,
            }),
        }
    }
    pub fn name(&self) -> &str { match self { Backend::Tmuxlet(b) => &b.name, Backend::Api(b) => &b.name, Backend::Cli(b) => &b.name } }

    pub fn dispatch(&self, prompt: &str, raw_messages: &serde_json::Value, env: &Env, timeout: Duration) -> Result<DispatchResult, BackendError> {
        match self {
            Backend::Tmuxlet(b) => tmuxlet::dispatch(b, prompt, env, timeout),
            Backend::Api(b) => api::dispatch(b, raw_messages, env),
            Backend::Cli(b) => cli::dispatch(b, prompt, env, timeout),
        }
    }
}
```

- [ ] **Step 4: Add module stubs so it compiles** — create `src/backend/tmuxlet.rs`, `api.rs`, `cli.rs` each with the dispatch signature returning `todo!()` (replaced in Tasks 7–9):
```rust
// tmuxlet.rs
use super::{TmuxletBackend, DispatchResult, BackendError};
use crate::env::Env; use std::time::Duration;
pub fn dispatch(_b:&TmuxletBackend,_p:&str,_e:&Env,_t:Duration)->Result<DispatchResult,BackendError>{ todo!() }
```
(analogous for `api.rs` with `(_b:&ApiBackend,_m:&serde_json::Value,_e:&Env)` and `cli.rs` with `(_b:&CliBackend,_p:&str,_e:&Env,_t:Duration)`)

- [ ] **Step 5: Run tests** — `cargo test --lib backend`. Expected: 2 passed.
- [ ] **Step 6: Commit** — `git commit -m "backend: runtime enum, error model, config->runtime conversion, dispatch routing"`

---

## Task 7: `backend/tmuxlet.rs` — spawn tmuxlet, parse JSON

**Files:** Replace `src/backend/tmuxlet.rs`; inline tests.

Invocation: `tmuxlet -p --target <t> --output-format json -C <cwd> --timeout <secs> [--target-arg A ...] <prompt>`. Parse the JSON; **success iff `status == "completed"`** (deviation #1). Non-JSON stdout → treat raw stdout as content.

- [ ] **Step 1: Failing tests** (pure functions: arg assembly + parse)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn b() -> TmuxletBackend { TmuxletBackend { name:"t".into(), target:"claude".into(), target_args:vec!["--effort".into(),"max".into()], cwd:PathBuf::from("/tmp") } }
    #[test]
    fn assembles_args_with_target_args_and_timeout() {
        let a = build_args(&b(), "hello", 1800);
        assert_eq!(a, vec!["-p","--target","claude","--output-format","json","-C","/tmp","--timeout","1800","--target-arg","--effort","--target-arg","max","hello"]);
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
```

- [ ] **Step 2: Run, verify fail.**

- [ ] **Step 3: Implement** (uses the stdlib `try_wait` polling watchdog from research; tmuxlet takes the prompt as final arg, not stdin)

```rust
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use super::{TmuxletBackend, DispatchResult, BackendError};
use crate::env::Env;

pub fn build_args(b: &TmuxletBackend, prompt: &str, timeout_secs: u64) -> Vec<String> {
    let mut a = vec!["-p".into(), "--target".into(), b.target.clone(),
        "--output-format".into(), "json".into(),
        "-C".into(), b.cwd.display().to_string(),
        "--timeout".into(), timeout_secs.to_string()];
    for ta in &b.target_args { a.push("--target-arg".into()); a.push(ta.clone()); }
    a.push(prompt.into());
    a
}

pub fn parse_output(name: &str, stdout: &str) -> Result<String, BackendError> {
    let trimmed = stdout.trim();
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => {
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if status == "completed" {
                Ok(v.get("output").and_then(|o| o.as_str()).unwrap_or("").to_string())
            } else {
                Err(BackendError::Backend(name.into(), format!("tmuxlet status={status}")))
            }
        }
        Err(_) => Ok(stdout.to_string()), // non-JSON fallback (matches prior proxy behavior)
    }
}

pub fn dispatch(b: &TmuxletBackend, prompt: &str, env: &Env, timeout: Duration) -> Result<DispatchResult, BackendError> {
    let args = build_args(b, prompt, timeout.as_secs());
    let mut child = Command::new("tmuxlet")
        .args(&args).env_clear().envs(env.as_pairs())
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?;

    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let oh = thread::spawn(move || { let mut s = Vec::new(); let _ = std::io::Read::read_to_end(&mut out, &mut s); s });
    let eh = thread::spawn(move || { let mut s = Vec::new(); let _ = std::io::Read::read_to_end(&mut err, &mut s); s });

    // server-side watchdog backstop (tmuxlet also self-limits via --timeout)
    let deadline = Instant::now() + timeout + Duration::from_secs(5);
    let status = loop {
        match child.try_wait().map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))? {
            Some(s) => break s,
            None => { if Instant::now() >= deadline { let _ = child.kill(); let _ = child.wait(); return Err(BackendError::Timeout(b.name.clone())); } thread::sleep(Duration::from_millis(50)); }
        }
    };
    let stdout = String::from_utf8_lossy(&oh.join().unwrap()).into_owned();
    let stderr = String::from_utf8_lossy(&eh.join().unwrap()).into_owned();
    if !status.success() && stdout.trim().is_empty() {
        return Err(BackendError::Exit(b.name.clone(), status.code().unwrap_or(-1), stderr.lines().last().unwrap_or("").to_string()));
    }
    let content = parse_output(&b.name, &stdout)?;
    Ok(DispatchResult { content, model_label: b.name.clone() })
}
```

- [ ] **Step 4: Run tests** — `cargo test --lib tmuxlet`. Expected: 4 passed.
- [ ] **Step 5: Commit** — `git commit -m "backend/tmuxlet: arg assembly, completed-status parse, dispatch with watchdog"`

---

## Task 8: `backend/api.rs` — OpenAI-compatible HTTP forwarder

**Files:** Replace `src/backend/api.rs`; add a small `src/http_client.rs` (hand-rolled HTTP/HTTPS); inline tests for body merge + URL.

Body merge order (spec §4): caller payload → shallow-merge `extra_body` (overriding dup top-level keys) → set `model` last (neither caller nor extra_body can override the pinned model). Then POST; re-extract `choices[0].message.content`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merges_extra_body_then_pins_model() {
        let caller = serde_json::json!({"model":"caller-wins?","messages":[{"role":"user","content":"hi"}],"think":false});
        let extra = serde_json::json!({"think":true,"foo":1});
        let body = build_body(&caller, &extra, "pinned-model");
        assert_eq!(body["model"], serde_json::json!("pinned-model")); // model pinned last
        assert_eq!(body["think"], serde_json::json!(true));           // extra_body overrides caller
        assert_eq!(body["foo"], serde_json::json!(1));
        assert!(body["messages"].is_array());                         // caller messages preserved
    }
    #[test]
    fn splits_base_url() {
        assert_eq!(split_url("http://127.0.0.1:11434/v1"), ("http".into(),"127.0.0.1".into(),11434,"/v1".into()));
        assert_eq!(split_url("https://openrouter.ai/api/v1"), ("https".into(),"openrouter.ai".into(),443,"/api/v1".into()));
    }
    #[test]
    fn extracts_content_from_completion() {
        let r = r#"{"choices":[{"message":{"content":"hello"}}]}"#;
        assert_eq!(extract_content("api", r).unwrap(), "hello");
    }
}
```

- [ ] **Step 2: Run, verify fail.**

- [ ] **Step 3: Implement `src/http_client.rs`** (lift verified research; ring CryptoProvider installed in Task 12)

```rust
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use rustls::pki_types::ServerName;

fn tls_config() -> Arc<ClientConfig> {
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth())
    }).clone()
}

fn request_bytes(host: &str, port: u16, path: &str, bearer: Option<&str>, body: &str) -> String {
    let auth = bearer.map(|b| format!("Authorization: Bearer {b}\r\n")).unwrap_or_default();
    format!("POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

fn parse_response(raw: &[u8]) -> io::Result<(u16, String)> {
    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..sep]);
    let body = String::from_utf8_lossy(&raw[sep + 4..]).into_owned();
    let status: u16 = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad status line"))?;
    Ok((status, body))
}

pub fn post_json(scheme: &str, host: &str, port: u16, path: &str, bearer: Option<&str>, body: &str) -> io::Result<(u16, String)> {
    let req = request_bytes(host, port, path, bearer, body);
    let mut raw = Vec::new();
    if scheme == "https" {
        let tcp = TcpStream::connect((host, port))?;
        let name = ServerName::try_from(host.to_string()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let conn = ClientConnection::new(tls_config(), name).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let mut tls = StreamOwned::new(conn, tcp);
        tls.write_all(req.as_bytes())?; tls.flush()?;
        tls.read_to_end(&mut raw)?;
    } else {
        let mut sock = TcpStream::connect((host, port))?;
        sock.write_all(req.as_bytes())?; sock.flush()?;
        sock.read_to_end(&mut raw)?;
    }
    parse_response(&raw)
}
```

- [ ] **Step 4: Implement `backend/api.rs`**

```rust
use super::{ApiBackend, DispatchResult, BackendError};
use crate::env::Env;
use crate::http_client;

pub fn split_url(base_url: &str) -> (String, String, u16, String) {
    let (scheme, rest) = base_url.split_once("://").unwrap_or(("http", base_url));
    let (authority, path) = match rest.find('/') { Some(i) => (&rest[..i], &rest[i..]), None => (rest, "") };
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(if scheme == "https" { 443 } else { 80 })),
        None => (authority.to_string(), if scheme == "https" { 443 } else { 80 }),
    };
    (scheme.to_string(), host, port, path.to_string())
}

pub fn build_body(caller: &serde_json::Value, extra: &serde_json::Value, model: &str) -> serde_json::Value {
    let mut body = caller.clone();
    if let (Some(obj), Some(ex)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in ex { obj.insert(k.clone(), v.clone()); }      // extra_body overrides caller
        obj.insert("model".into(), serde_json::json!(model));        // pin model last
        obj.remove("stream");                                        // V1 always non-streamed upstream
    }
    body
}

pub fn extract_content(name: &str, resp: &str) -> Result<String, BackendError> {
    let v: serde_json::Value = serde_json::from_str(resp).map_err(|e| BackendError::Parse(name.into(), e.to_string()))?;
    v.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("message")).and_then(|m| m.get("content")).and_then(|c| c.as_str())
        .map(|s| s.to_string()).ok_or_else(|| BackendError::Parse(name.into(), "no choices[0].message.content".into()))
}

pub fn dispatch(b: &ApiBackend, raw_messages: &serde_json::Value, env: &Env) -> Result<DispatchResult, BackendError> {
    let (scheme, host, port, base_path) = split_url(&b.base_url);
    let path = format!("{}/chat/completions", base_path.trim_end_matches('/'));
    let body = build_body(raw_messages, &b.extra_body, &b.model).to_string();
    let bearer = b.api_key_env.as_ref().and_then(|k| env.get(k).map(str::to_string));
    let (status, resp) = http_client::post_json(&scheme, &host, port, &path, bearer.as_deref(), &body)
        .map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?;
    if !(200..300).contains(&status) { return Err(BackendError::Http(b.name.clone(), status)); }
    let content = extract_content(&b.name, &resp)?;
    Ok(DispatchResult { content, model_label: b.model.clone() })
}
```

Add `mod http_client;` to `main.rs`. **Note:** `raw_messages` passed to `dispatch` must be the full original request JSON object (model+messages+...), built in Task 11.

- [ ] **Step 5: Run tests** — `cargo test --lib api`. Expected: 3 passed.
- [ ] **Step 6: Commit** — `git commit -m "backend/api: hand-rolled http/https client, body merge, content extraction"`

---

## Task 9: `backend/cli.rs` — exec binary (PTY or plain)

**Files:** Replace `src/backend/cli.rs`; inline tests for arg assembly.

`pty=false` → `Command::output()` with prompt as final arg, clean output. `pty=true` → `pty::run_in_pty` (Task 10), clean output.

- [ ] **Step 1: Failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*; use std::path::PathBuf;
    #[test]
    fn plain_args_append_prompt() {
        let b = CliBackend { name:"x".into(), bin:PathBuf::from("/bin/echo"), args:vec!["-n".into()], pty:false };
        assert_eq!(plain_args(&b, "hello"), vec!["-n".to_string(), "hello".to_string()]);
    }
}
```

- [ ] **Step 2: Run, verify fail.**

- [ ] **Step 3: Implement**

```rust
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use super::{CliBackend, DispatchResult, BackendError};
use crate::env::Env;
use crate::pty;

pub fn plain_args(b: &CliBackend, prompt: &str) -> Vec<String> {
    let mut a = b.args.clone(); a.push(prompt.into()); a
}

pub fn dispatch(b: &CliBackend, prompt: &str, env: &Env, timeout: Duration) -> Result<DispatchResult, BackendError> {
    let content = if b.pty {
        let raw = pty::run_in_pty(&b.bin.display().to_string(), &b.args, &env.as_pairs(),
            &std::env::var("HOME").unwrap_or_else(|_| ".".into()), prompt, timeout)
            .map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?;
        pty::clean_output(&raw)
    } else {
        let mut child = Command::new(&b.bin).args(plain_args(b, prompt))
            .env_clear().envs(env.as_pairs())
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?;
        let mut out = child.stdout.take().unwrap();
        let oh = thread::spawn(move || { let mut s = Vec::new(); let _ = std::io::Read::read_to_end(&mut out, &mut s); s });
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait().map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))? {
                Some(_) => break,
                None => { if Instant::now() >= deadline { let _ = child.kill(); let _ = child.wait(); return Err(BackendError::Timeout(b.name.clone())); } thread::sleep(Duration::from_millis(50)); }
            }
        }
        pty::clean_output(&oh.join().unwrap())
    };
    Ok(DispatchResult { content, model_label: b.name.clone() })
}
```

- [ ] **Step 4: Run tests** — `cargo test --lib cli`. Expected: 1 passed.
- [ ] **Step 5: Commit** — `git commit -m "backend/cli: plain + pty dispatch with output cleanup"`

---

## Task 10: `pty.rs` — PTY runner + output cleanup

**Files:** Create `src/pty.rs`; add `mod pty;`; inline tests for `clean_output`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_ansi_cr_and_control_keeps_newline_tab() {
        let raw = b"\x1b[1;32mHELLO\x1b[0m\r\nline2\ttab\x07\n";
        let out = clean_output(raw);
        assert_eq!(out, "HELLO\nline2\ttab\n");
    }
    #[test]
    fn strips_osc_sequences() {
        let raw = b"\x1b]0;title\x07visible";
        assert_eq!(clean_output(raw), "visible");
    }
}
```

- [ ] **Step 2: Run, verify fail.**

- [ ] **Step 3: Implement** (verified byte-filter state machine + PTY runner from research; macOS EIO-as-EOF handled)

```rust
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

pub fn run_in_pty(program: &str, args: &[String], env: &[(&str, &str)], cwd: &str, prompt: &str, timeout: Duration) -> anyhow_lite::Result<Vec<u8>> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).map_err(stringify)?;
    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    for (k, v) in env { cmd.env(k, v); }
    cmd.cwd(cwd);
    let mut reader = pair.master.try_clone_reader().map_err(stringify)?;
    let mut writer = pair.master.take_writer().map_err(stringify)?;
    let mut child = pair.slave.spawn_command(cmd).map_err(stringify)?;
    drop(pair.slave); // REQUIRED: else reader never sees EOF
    let mut killer = child.clone_killer();

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let rh = thread::spawn(move || {
        let mut collected = Vec::new(); let mut buf = [0u8; 8192];
        loop { match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => collected.extend_from_slice(&buf[..n]),
            Err(e) => { if e.raw_os_error() == Some(5) || e.kind() == std::io::ErrorKind::UnexpectedEof { break } else { break } }
        }}
        let _ = tx.send(collected);
    });

    let _ = write!(writer, "{prompt}"); let _ = writer.flush(); drop(writer);

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let wd = thread::spawn(move || { match done_rx.recv_timeout(timeout) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {},
        Err(mpsc::RecvTimeoutError::Timeout) => { let _ = killer.kill(); }
    }});
    let _ = child.wait().map_err(stringify)?;
    let _ = done_tx.send(()); let _ = wd.join();
    let _ = rh.join();
    let out = rx.recv().unwrap_or_default();
    drop(pair.master);
    Ok(out)
}

fn stringify<E: std::fmt::Display>(e: E) -> String { e.to_string() }
mod anyhow_lite { pub type Result<T> = std::result::Result<T, String>; }

/// Strip ANSI/OSC/escape sequences, CR, and C0 control bytes (keep \n and \t).
pub fn clean_output(raw: &[u8]) -> String {
    enum St { Normal, Esc, Csi, Osc }
    let mut state = St::Normal;
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    for &b in raw {
        match state {
            St::Normal => match b {
                0x1b => state = St::Esc,
                b'\r' => {}
                b'\n' | b'\t' => out.push(b),
                0x20..=0x7e => out.push(b),
                0x80.. => out.push(b),
                _ => {}
            },
            St::Esc => match b { b'[' => state = St::Csi, b']' => state = St::Osc, _ => state = St::Normal },
            St::Csi => if (0x40..=0x7e).contains(&b) { state = St::Normal },
            St::Osc => match b { 0x07 => state = St::Normal, 0x1b => state = St::Esc, _ => {} },
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
```

> Note: `run_in_pty` returns `Result<_, String>` (a tiny local alias) to avoid an `anyhow` dependency; `cli.rs` maps it into `BackendError::Spawn`.

- [ ] **Step 4: Run tests** — `cargo test --lib pty`. Expected: 2 passed.
- [ ] **Step 5: Commit** — `git commit -m "pty: portable-pty runner (macOS EIO-as-EOF) + dependency-free output cleanup"`

---

## Task 11: `http.rs` — routing, handlers, SSE writer, serve loop

**Files:** Create `src/http.rs`; add `mod http;`; inline tests for route matching + SSE body bytes.

V1 routes: `GET /health`, `GET /v1/models`, `POST /v1/chat/completions`. Reserved namespaces (`/`, `/ui/*`, `/api/...`) → `501`. Else `404`. All errors use the OpenAI error envelope.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_routes() {
        assert!(matches!(classify("GET","/health"), Route::Health));
        assert!(matches!(classify("GET","/v1/models"), Route::Models));
        assert!(matches!(classify("POST","/v1/chat/completions"), Route::Chat));
        assert!(matches!(classify("GET","/ui/index.html"), Route::Reserved));
        assert!(matches!(classify("GET","/api/sessions/1"), Route::Reserved));
        assert!(matches!(classify("GET","/"), Route::Reserved));
        assert!(matches!(classify("GET","/nope"), Route::NotFound));
    }
    #[test]
    fn sse_body_reads_all_frames_then_eof() {
        let mut body = SseBody::from_frames(vec!["data: a\n\n".into(), "data: [DONE]\n\n".into()]);
        let mut s = String::new();
        std::io::Read::read_to_string(&mut body, &mut s).unwrap();
        assert_eq!(s, "data: a\n\ndata: [DONE]\n\n");
    }
}
```

- [ ] **Step 2: Run, verify fail.**

- [ ] **Step 3: Implement `http.rs`**

```rust
use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Request, Response, Server};
use crate::backend::Backend;
use crate::config::Config;
use crate::env::Env;
use crate::{openai, router};

pub enum Route { Health, Models, Chat, Reserved, NotFound }

pub fn classify(method: &str, path: &str) -> Route {
    match (method, path) {
        ("GET", "/health") => Route::Health,
        ("GET", "/v1/models") => Route::Models,
        ("POST", "/v1/chat/completions") => Route::Chat,
        _ if path == "/" || path.starts_with("/ui") || path.starts_with("/api") => Route::Reserved,
        _ => Route::NotFound,
    }
}

pub struct SseBody { frames: VecDeque<Vec<u8>>, cursor: usize }
impl SseBody {
    pub fn from_frames(frames: Vec<String>) -> Self { SseBody { frames: frames.into_iter().map(String::into_bytes).collect(), cursor: 0 } }
}
impl Read for SseBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(front) = self.frames.front() else { return Ok(0) };
            let rem = &front[self.cursor..];
            if rem.is_empty() { self.frames.pop_front(); self.cursor = 0; continue; }
            let n = rem.len().min(buf.len());
            buf[..n].copy_from_slice(&rem[..n]); self.cursor += n; return Ok(n);
        }
    }
}

pub struct State { pub cfg: Config, pub env: Env }

fn header(name: &str, value: &str) -> Header { Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap() }

fn respond_json(req: Request, status: u16, body: String) {
    let _ = req.respond(Response::from_string(body).with_header(header("Content-Type", "application/json")).with_status_code(status));
}
fn respond_err(req: Request, status: u16, msg: &str, etype: &str, code: Option<&str>) {
    respond_json(req, status, openai::ErrorEnvelope::new(msg, etype, code).to_json());
}

pub fn handle(mut req: Request, state: &Arc<State>) {
    let method = req.method().as_str().to_string();
    let path = req.url().split('?').next().unwrap_or("").to_string();
    match classify(&method, &path) {
        Route::Health => respond_json(req, 200, format!("{{\"status\":\"ok\",\"backends\":{},\"chains\":{}}}", state.cfg.backends.len(), state.cfg.chains.len())),
        Route::Models => {
            let ids = state.cfg.chains.keys().chain(state.cfg.backends.keys()).cloned();
            respond_json(req, 200, serde_json::to_string(&openai::model_list(ids)).unwrap());
        }
        Route::Reserved => respond_err(req, 501, "not implemented in V1", "server_error", Some("not_implemented")),
        Route::NotFound => respond_err(req, 404, "no such route", "invalid_request_error", Some("unknown_route")),
        Route::Chat => {
            let mut raw = String::new();
            if req.as_reader().read_to_string(&mut raw).is_err() { return respond_err(req, 400, "could not read body", "invalid_request_error", Some("read_error")); }
            handle_chat(req, &raw, state);
        }
    }
}

fn handle_chat(req: Request, raw: &str, state: &Arc<State>) {
    let parsed: openai::ChatRequest = match serde_json::from_str(raw) {
        Ok(p) => p, Err(e) => return respond_err(req, 400, &format!("parse error: {e}"), "invalid_request_error", Some("parse_error")),
    };
    if parsed.messages.is_empty() { return respond_err(req, 400, "messages must not be empty", "invalid_request_error", Some("missing_messages")); }

    let names = match router::resolve(&parsed.model, &state.cfg) { Ok(n) => n, Err(e) => return respond_err(req, 400, &e, "invalid_request_error", Some("bad_model")) };
    let prompt = openai::flatten_to_prompt(&parsed.messages);
    let raw_value: serde_json::Value = serde_json::from_str(raw).unwrap_or(serde_json::json!({}));
    let default_to = Duration::from_secs(state.cfg.server.request_timeout_secs);

    let mut errors = Vec::new();
    for name in &names {
        let cfg_backend = &state.cfg.backends[*name];
        let timeout = match cfg_backend { crate::config::Backend::Api { timeout_secs: Some(t), .. } => Duration::from_secs(*t), _ => default_to };
        let backend = Backend::from_config(name, cfg_backend);
        match backend.dispatch(&prompt, &raw_value, &state.env, timeout) {
            Ok(result) => {
                eprintln!("[ok] {name}");
                let id = format!("chatcmpl-{}", &result.model_label);
                if parsed.stream {
                    let frames = openai::stream_frames(&id, &result.model_label, &result.content);
                    let resp = Response::new(200.into(), vec![header("Content-Type","text/event-stream"), header("Cache-Control","no-cache")], SseBody::from_frames(frames), None, None);
                    let _ = req.respond(resp);
                } else {
                    respond_json(req, 200, serde_json::to_string(&openai::build_completion(id, result.model_label, result.content)).unwrap());
                }
                return;
            }
            Err(e) => { eprintln!("[skip] {e}"); errors.push(e.to_string()); }
        }
    }
    let detail = serde_json::to_string(&errors).unwrap_or_default();
    respond_err(req, 503, &format!("all backends failed: {detail}"), "server_error", Some("all_backends_failed"));
}

pub fn serve(server: Arc<Server>, state: Arc<State>, workers: usize) {
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let server = Arc::clone(&server); let state = Arc::clone(&state);
        handles.push(std::thread::spawn(move || loop {
            match server.recv() { Ok(req) => handle(req, &state), Err(_) => break }
        }));
    }
    for h in handles { let _ = h.join(); }
}
```

- [ ] **Step 4: Run tests** — `cargo test --lib http`. Expected: 2 passed.
- [ ] **Step 5: Commit** — `git commit -m "http: routing, handlers, error envelope, SSE writer, worker pool serve loop"`

---

## Task 12: `main.rs` serve wiring — startup, env capture, signal handler

**Files:** Modify `src/main.rs`.

- [ ] **Step 1: Replace the serve stub with real startup**

```rust
mod config; mod openai; mod env; mod router; mod backend; mod pty; mod http; mod http_client;

use std::sync::Arc;
use std::process::ExitCode;
use tiny_http::Server;

// ... Args / flag parsing unchanged ...

fn run_serve(config_path: &str) -> ExitCode {
    // 1. rustls ring CryptoProvider — REQUIRED before any TLS use (api backend).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 2. Load + validate config.
    let path = config::tilde_path(config_path);
    let cfg = match config::load(&path) { Ok(c) => c, Err(e) => { eprintln!("tmuxlet-server: {e}"); return ExitCode::FAILURE; } };

    // 3. Capture environment for backends.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let environment = env::Env::capture(&cfg.server.env_source, &shell);
    for (_, b) in &cfg.backends { if let config::Backend::Api { api_key_env: Some(k), .. } = b { if environment.get(k).is_none() { eprintln!("[warn] api_key_env {k} is unset"); } } }

    // 4. Bind.
    let listen = cfg.server.listen.clone();
    let server = match Server::http(&listen) { Ok(s) => Arc::new(s), Err(e) => { eprintln!("tmuxlet-server: failed to bind {listen}: {e}"); return ExitCode::FAILURE; } };
    eprintln!("tmuxlet-server {VERSION} listening on http://{listen} ({} backends, {} chains)", cfg.backends.len(), cfg.chains.len());

    // 5. Signal handler → unblock workers.
    { let server = Arc::clone(&server); let _ = ctrlc::set_handler(move || { eprintln!("shutting down..."); server.unblock(); }); }

    let state = Arc::new(http::State { cfg, env: environment });
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    http::serve(server, state, workers);
    ExitCode::SUCCESS
}
```

Change the serve branch in `main()` to `return run_serve(&args.config);`.

- [ ] **Step 2: Build** — `cargo build`. Expected: clean.

- [ ] **Step 3: Manual smoke (local)** — start with a config that has only an `api` backend pointing at the live Ollama (`http://127.0.0.1:11434/v1`), then:
```bash
cargo run -- --config /tmp/tmtest/server.toml &
sleep 1
curl -s http://127.0.0.1:3456/health
curl -s -X POST http://127.0.0.1:3456/v1/chat/completions -H 'Content-Type: application/json' -d '{"model":"default","messages":[{"role":"user","content":"say READY"}]}'
kill %1
```
Expected: health JSON; a chat completion with non-empty content from Ollama.

- [ ] **Step 4: Commit** — `git commit -m "main: serve wiring, ring crypto provider, env capture, signal-based shutdown"`

---

## Task 13: Integration tests

**Files:** `tests/server_starts.rs`, `tests/routes.rs`, `tests/chain_fallback.rs`, `tests/streaming.rs`, `tests/fixtures/{mock-tmuxlet,mock-cli,mock-api.rs}`. Each test spawns the built binary against a temp config on an ephemeral port.

- [ ] **Step 1: Shared helper + `server_starts.rs`**

```rust
// tests/common/mod.rs — spawn the binary, return (child, base_url)
use std::io::Write; use std::process::{Child, Command, Stdio}; use std::net::TcpListener; use std::time::Duration;
pub fn free_port() -> u16 { TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() }
pub fn start(config_toml: &str) -> (Child, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfgp = dir.path().join("server.toml");
    std::fs::File::create(&cfgp).unwrap().write_all(config_toml.as_bytes()).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_tmuxlet-server")).arg("--config").arg(&cfgp).stderr(Stdio::null()).spawn().unwrap();
    let port = config_toml.split("listen = \"127.0.0.1:").nth(1).unwrap().split('"').next().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    std::thread::sleep(Duration::from_millis(800));
    (child, base, dir)
}
```
Add `tempfile = "3"` and a tiny blocking HTTP get helper (reuse `http_client` is internal, so use a 10-line raw `TcpStream` GET in the test, or add `ureq` as a dev-dependency). **Decision:** add `ureq = "2"` as a `[dev-dependencies]` entry to keep test HTTP simple — dev-deps don't affect the shipped binary's dependency budget.

`server_starts.rs`: start with a minimal config (one `cli` backend = `/bin/echo`), GET `/health`, assert `status":"ok"`.

- [ ] **Step 2: `routes.rs`** — assert `/v1/models` lists backend+chain names; an unknown path returns 404 with an `error` envelope; `/ui/x` returns 501.

- [ ] **Step 3: `chain_fallback.rs`** — config with a chain `["broken","good"]` where `broken` is a `cli` backend whose `bin` is `/usr/bin/false` (exits non-zero, empty stdout → error) and `good` is `/bin/echo`. POST a chat request; assert it succeeds via `good`. Then a chain of only `broken`; assert `503` + `all_backends_failed`.

- [ ] **Step 4: `streaming.rs`** — POST with `stream:true` against an echo `cli` backend; read the response and assert it contains `chat.completion.chunk`, a `"role":"assistant"` frame, and ends with `data: [DONE]`.

- [ ] **Step 5: Run** — `cargo test --test '*'`. Expected: all pass (macOS).
- [ ] **Step 6: Commit** — `git commit -m "tests: integration suite (start, routes, fallback, streaming) with fixtures"`

---

## Task 14: Docs + examples + smoke script

**Files:** `README.md`, `AGENTS.md`, `examples/server.toml`, `examples/launchagent.plist`, `examples/tmuxlet-server.service`, `scripts/smoke.sh`.

- [ ] **Step 1: `examples/server.toml`** — the canonical config using verified machine facts:
```toml
[server]
listen = "127.0.0.1:3456"
default_chain = "default"
request_timeout_secs = 1800
env_source = "shell"
log_level = "info"

[backends.agy]
type = "cli"
bin = "/Users/kk/.antigravity/antigravity/bin/agy"   # deviation #2: correct path
args = ["-p"]
pty = true

[backends.ollama-kimi]
type = "api"
base_url = "http://127.0.0.1:11434/v1"
model = "kimi-k2.6:cloud"
extra_body = { think = true }
timeout_secs = 120                                     # api can fail fast

[backends.openrouter-sonnet]
type = "api"
base_url = "https://openrouter.ai/api/v1"
model = "anthropic/claude-sonnet-4.6"
api_key_env = "OPENROUTER_API_KEY"
timeout_secs = 120

[backends.claude-thinking]
type = "tmuxlet"
target = "claude"
target_args = ["--effort", "max"]
cwd = "~"

[chains.default]
order = ["agy", "ollama-kimi", "claude-thinking"]

[chains.thinking]
order = ["claude-thinking", "agy"]
```

- [ ] **Step 2: `AGENTS.md`** — the agent-facing install/verify runbook (spec §8): prerequisites (`cargo`, `tmux`, target CLIs), `cargo install --git https://github.com/CodefiLabs/tmuxlet-server`, write config heredoc, `tmuxlet-server --validate`, LaunchAgent/systemd install with verification commands, `/health` + chat smoke, downstream wiring (Hermes/Continue/Cursor), troubleshooting. **Must include deviation #4 note:** if `agy` is a broken symlink, reinstall Antigravity before relying on the `agy` backend.

- [ ] **Step 3: `examples/launchagent.plist`** — macOS template. Because env capture uses `$SHELL -ilc env`, the plist needs only `SHELL` and a minimal PATH in `<EnvironmentVariables>`; document that the server enriches env at startup. Redirect stderr to `~/Library/Logs/tmuxlet-server.log`.

- [ ] **Step 4: `examples/tmuxlet-server.service`** — systemd user unit (`ExecStart`, `Restart=on-failure`, `Environment=`).

- [ ] **Step 5: `scripts/smoke.sh`** — start the server, hit `/health`, run a non-stream chat against `default`, run a `stream:true` chat, assert `[DONE]`. Documents that the agy leg requires Antigravity installed.

- [ ] **Step 6: `README.md`** — elevator pitch, install summary, link to AGENTS.md, the V1 streaming-limitation note, and the dependency budget.

- [ ] **Step 7: Commit** — `git commit -m "docs: README, agent-facing AGENTS.md, example config + service templates, smoke script"`

---

## Task 15: CI

**Files:** `.github/workflows/ci.yml`.

- [ ] **Step 1: Write the workflow** — matrix `{macos-latest, ubuntu-latest}`; steps: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`. (No live backends in CI; integration tests use only `/bin/echo`, `/usr/bin/false`, and a mock-api — all present on both runners.) Run `yamllint` locally before committing per workspace convention.

- [ ] **Step 2: Verify locally** — `cargo fmt --check && cargo clippy -- -D warnings && cargo test`. Expected: clean.
- [ ] **Step 3: Commit** — `git commit -m "ci: fmt + clippy + test on macOS and Linux"`

---

## Self-review

**1. Spec coverage** (each spec section → task):
- §3 process shape / worker pool → Tasks 11, 12. §3 logging (`[ok]`/`[skip]`) → Task 11. §3 error model (config errors at startup; runtime OpenAI envelope; 503 `all_backends_failed`) → Tasks 2, 11.
- §4 config schema, validation, tilde, `api_key_env` (name not key), `extra_body` shallow merge, model pinned last → Tasks 2, 8.
- §5 routes (`/health`, `/v1/models`, `/v1/chat/completions`), reserved 501, 404 envelope, request flow, SSE shape, streaming limitation, no-auth/127.0.0.1 → Task 11.
- §6 backend types + `Backend`/`DispatchResult`/`BackendError` → Tasks 6–10; env capture → Task 4.
- §7 unit tests per module + integration suite + smoke script → Tasks 2–13, 14. §8 AGENTS.md → Task 14. CI → Task 15.
- **Gap check:** spec's per-backend timeout was implicit; covered via `timeout_secs` (Tasks 2, 11). The `--validate` startup checks (§4) are in Task 2's `validate()`; the `api`-key-unset *warn* is in Task 12. ✔

**2. Placeholder scan:** the Task 6 backend submodule `todo!()` stubs are explicitly replaced in Tasks 7–9 (called out in-step), not left as placeholders. No "TBD"/"add error handling"/"similar to Task N" remain. ✔

**3. Type consistency:** `DispatchResult{content, model_label}`, `BackendError` variants, `Env::{capture,get,as_pairs}`, `Backend::{from_config,name,dispatch}`, `config::Backend` variants, `openai::{flatten_to_prompt,build_completion,stream_frames,model_list,ErrorEnvelope::new}`, `router::resolve`, `http::{classify,State,SseBody,serve}`, `pty::{run_in_pty,clean_output}` — names match across all tasks. `dispatch` signature `(&self, prompt, raw_messages, env, timeout)` is consistent in Tasks 6, 7, 9 (api ignores `timeout`/`prompt`, uses `raw_messages`). ✔

**Decision recorded:** `ureq` added as a **dev-dependency only** (Task 13) for test-side HTTP — does not affect the shipped binary's dependency budget.
