# tmuxlet-server Optimization Spec — Security, Performance, UX

_Date: 2026-07-09 · Basis: full function-by-function audit of `main` @ 96ac076 (every function in `src/` read and evaluated) · Scope: security, performance, UX; out-of-scope findings adjudicated via `tmuxlet --target grok` (Section 6) · Rev 2: UX review pass over every proposed change (Section 8)_

## 0. Method & coverage

Every function in the package was read and evaluated (59 functions/methods across 12 source files, excluding `#[cfg(test)]` items). The audit table in Section 1 is the coverage proof: each function is listed with either **OK** (no finding) or the IDs of the findings it produced. Findings are specified in Sections 2–4 (security / performance / UX), the user-requested **auto-router** feature is specified in Section 5, out-of-scope adjudication is in Section 6, the prioritized roadmap is in Section 7, and the rev-2 UX review log is in Section 8.

### UX invariants (rev 2 — govern every change below)

The heart of this project is: one static binary, one small TOML file, a zero-setup localhost daemon that any OpenAI-shaped client can point at. Every change in this spec was re-evaluated against that (Section 8 has the per-item log). Four invariants bind the implementation:

1. **Existing configs keep working.** No new required keys; every new knob defaults to current behavior or to a strictly-better drop-in (the only intentional behavior changes are itemized at the end of Section 8).
2. **`--validate` is strict; `serve` is forgiving.** Config smells (unknown keys, name shadowing, weak settings) are errors under `--validate` but warnings at serve time, so an upgrade never bricks a running service over a nit.
3. **Every refusal names its fix.** Any error that stops startup or a request must state the one-line remedy (config key, flag, or file path) in the message itself.
4. **The example config stays minimal.** New knobs go in a commented reference block, not the happy path; `cp examples/server.toml` + edit two lines must remain the whole setup.

## 1. Function audit table

| File | Function | Verdict |
|---|---|---|
| `main.rs` | `main` | U-18 (help/default-path), U-5 (validate depth) |
| `main.rs` | `run_serve` | S-2 (bind policy), S-3 (env exposure), P-1 (worker sizing), U-22 (no reload) |
| `main.rs` | `print_help` | U-18 |
| `config.rs` | `Config`/`Server`/`Chain`/`Backend` (serde) | U-4 (unknown fields ignored), U-8 (log_level unused) |
| `config.rs` | `default_timeout` / `default_env_source` / `default_log_level` | OK |
| `config.rs` | `load` | OK |
| `config.rs` | `parse` | OK |
| `config.rs` | `validate` | U-5 (missing checks: listen addr, bin existence, name shadowing, `timeout=0`, router refs) |
| `config.rs` | `expand_tilde` | OK (no `~user`; accepted) |
| `config.rs` | `expand_paths` | OK |
| `config.rs` | `tilde_path` | OK |
| `env.rs` | `Env::capture` | U-8a (silent fallback unlogged), U-5 (unknown `env_source` value accepted) |
| `env.rs` | `Env::get` / `Env::as_pairs` | OK (per-dispatch alloc negligible) |
| `env.rs` | `capture_shell` | S-9 (no timeout → startup hang), S-7 (multi-line values corrupt parse) |
| `env.rs` | `is_valid_key` / `parse_env` | S-7 |
| `router.rs` | `resolve` | U-3 (silent default fallback), U-5 (chain shadows backend) |
| `openai.rs` | `content_to_text` | U-15 (parts joined with `""`), U-15a (non-text parts dropped silently) |
| `openai.rs` | `flatten_to_prompt` | U-14 (label inconsistency; no raw-prompt option; injection note in S-6a) |
| `openai.rs` | `unix_now` | OK |
| `openai.rs` | `build_completion` | U-16 (usage always 0) |
| `openai.rs` | `stream_frames` | OK (V1 contract) |
| `openai.rs` | `model_list` | U-9 (unsorted, order changes per run) |
| `openai.rs` | `ErrorEnvelope::new` / `to_json` | OK |
| `http.rs` | `classify` | U-17 (HEAD → 404; no 405), U-10 (no CORS/OPTIONS) |
| `http.rs` | `SseBody::from_frames` / `Read::read` | OK |
| `http.rs` | `header` / `respond_json` / `respond_err` | OK |
| `http.rs` | `handle` | S-1 (no auth), U-8 (no request logging/timing); body cap ✅ good |
| `http.rs` | `handle_chat` | P-2 (double parse), P-3 (per-request backend build), P-6 (no chain budget), P-9 (no 429 cooldown), S-6 (error detail leak), U-2 (no SSE keepalive), U-7 (via api errors), U-23 (double-encoded 503 detail), P-7 (no disconnect detection) |
| `http.rs` | `serve` | F-3 (panic guard — adjudicated) |
| `http_client.rs` | `tls_config` | U-24 (no custom CA / insecure knob) |
| `http_client.rs` | `request_bytes` | OK (header-injection surface is config/env-owned; noted S-6a) |
| `http_client.rs` | `connect` | F-6 (first-addr only — adjudicated) |
| `http_client.rs` | `read_body` | S-5 (unbounded response read) |
| `http_client.rs` | `parse_response` | F-5 (ignores Content-Length — adjudicated) |
| `http_client.rs` | `dechunk` | OK (lenient by design) |
| `http_client.rs` | `post_json` | P-4 (no connection reuse; accepted for V1) |
| `pty.rs` | `stringify` | OK |
| `pty.rs` | `run_in_pty` | F-1 (timeout → partial output as success — adjudicated), F-8 (write errors ignored — adjudicated), U-12 (80-col wrap mangling), S-3 (full env to child) |
| `pty.rs` | `clean_output` | F-2 (DCS/8-bit CSI leak — adjudicated) |
| `backend/mod.rs` | `BackendError::fmt` | OK |
| `backend/mod.rs` | `Backend::from_config` | P-3 |
| `backend/mod.rs` | `Backend::name` | OK (test-only) |
| `backend/mod.rs` | `Backend::dispatch` | OK |
| `backend/api.rs` | `split_url` | U-19 (IPv6 literal unsupported) |
| `backend/api.rs` | `build_body` | OK (model pin + stream drop correct) |
| `backend/api.rs` | `extract_content` | OK (V1 contract; tool_calls/null content out of scope) |
| `backend/api.rs` | `dispatch` | U-7 (upstream error body discarded), P-9 |
| `backend/cli.rs` | `plain_args` | S-4 (prompt in argv: ps-visible + E2BIG) |
| `backend/cli.rs` | `dispatch` | F-1 (PTY path checks neither timeout nor exit status), U-11 (no `cwd`; PTY=HOME vs plain=server-cwd inconsistency), U-6 (empty output = success) |
| `backend/tmuxlet.rs` | `build_args` | S-4 (prompt in argv) |
| `backend/tmuxlet.rs` | `parse_output` | OK (lenient non-JSON fallback is deliberate) |
| `backend/tmuxlet.rs` | `dispatch` | F-4 (bare-name spawn — adjudicated), U-6 |

## 2. Security findings & spec

### S-1: Add optional bearer-token auth (HIGH)
Any process or user on the machine can call the server and spend the owner's API credits (the `api` backend attaches `api_key_env` secrets to caller-controlled bodies) or drive CLI backends that run with the owner's full environment.
**Spec:** `[server] auth = true` → on first start the server generates a random 32-byte token into `~/.tmuxlet/token` (mode 0600) and logs its location; `POST /v1/chat/completions`, `/v1/models`, and `/v1/backends` then require `Authorization: Bearer <token>` (constant-time compare; 401 with OpenAI envelope on mismatch). `/health` stays open. Every 401 also logs a server-side hint ("token mismatch — expected token is in ~/.tmuxlet/token") so misconfiguration is one glance to diagnose, not a curl-guessing session. Alternative for env-var setups: `auth_token_env = "NAME"` (read at startup; wins over the file). Default: off (current behavior) — but see S-2. Token-file over env-var is deliberate: env delivery under launchd/systemd is this project's known failure mode (AGENTS.md troubleshooting has a row for exactly this), while clients already have an API-key field — so server setup is one config line and client setup stays copy-paste (`cat ~/.tmuxlet/token`).

### S-2: Bind-address policy (HIGH)
`listen` is honored verbatim; `0.0.0.0:3456` silently creates an unauthenticated plaintext proxy to paid APIs and local CLI execution.
**Spec:** if the bind address is non-loopback **and** S-1 auth is off, refuse to start — and the error must contain both remedies verbatim ("set `auth = true` in server.toml" / "or pass `--allow-remote-unauthenticated`"). `--validate` reports the same condition, so it is caught before deploy rather than at a 2am service restart. Log a prominent warning whenever bound non-loopback even with auth on. Docs: recommend SSH tunnel for remote use (tiny_http has no server-side TLS). With S-1's one-line setup, the refusal path costs seconds, not a configuration project.

### S-3: Per-backend environment allowlist (MEDIUM)
`env_clear().envs(all captured vars)` hands **every** secret in the login shell (AWS keys, tokens, etc.) to every backend child, PTY children included.
**Spec:** optional per-backend `env_pass = ["PATH", "HOME", "TERM", "ANTHROPIC_*"]` (glob-capable allowlist) and server-level `[server] env_pass` default. When absent, current behavior is kept for compatibility; `--validate` prints one informational note, and serve time never nags — a running service must not accumulate warning spam for a deliberate default. Always pass a minimal base set (`PATH`, `HOME`, `SHELL`, `TERM`, `LANG`) in addition to the allowlist, so a tight allowlist cannot produce the "works in my terminal, fails in the server" class of bug.

### S-4: Keep prompts out of argv (MEDIUM)
`cli` (plain) and `tmuxlet` backends pass the flattened prompt as a positional argument: visible to every local user via `ps`, and exec fails with E2BIG for large transcripts (macOS argv limit ≈256 KiB, well under the 16 MiB body cap).
**Spec:** `cli` backend: optional `stdin_prompt = true` (write prompt to child stdin, close). `tmuxlet` backend: use tmuxlet's stdin form (`tmuxlet -p … -`) when available — probe support once at startup (version/help check), cache the result, and fall back to argv when unsupported; a probe failure must only select argv mode, never fail a request. Document the ps-visibility caveat wherever argv is retained.

### S-5: Cap upstream response size (MEDIUM)
`read_body` reads to EOF unbounded; a misbehaving/hostile upstream can exhaust memory.
**Spec:** cap at 64 MiB (configurable `[server] max_response_bytes`); on overflow, return a `Parse`-class backend error so the chain advances.

### S-6: Redact internal detail in client-facing errors (LOW)
The 503 `all_backends_failed` message embeds every backend's error (binary paths, spawn errno text). Fine for loopback-only; leaky if ever remote.
**Spec:** redact only when the server is bound non-loopback: per-backend detail replaced with backend names + error class. On loopback the full detail stays in the 503 **even with auth enabled** — the local operator debugging with curl is the same person who reads the server log, and the AGENTS.md troubleshooting flow ("read the detail array") depends on it. Full detail always goes to the server log regardless.

### S-6a: Prompt-role spoofing note (INFORMATIONAL)
`flatten_to_prompt` merges roles into one text block; a user message containing `Assistant: …` can spoof turns. Inherent to the flattening design — document it; the U-14 raw-prompt option reduces exposure for single-message use.

### S-7: Robust env capture parsing (LOW)
Multi-line env values corrupt `parse_env` line-splitting and can inject bogus keys into the env passed to backends.
**Spec:** use `env -0` when available (`$SHELL -ilc 'command env -0'`), split on NUL; fall back to line parsing. (Grok verdict: F7 — see Section 6.)

### S-9: Timeout on shell env capture (MEDIUM)
`capture_shell` has no timeout; a blocking `~/.zshrc` hangs startup forever (service manager sees a silent non-starting unit).
**Spec:** 15 s watchdog (configurable); on expiry kill the shell, log a warning naming the fix (`env_source = "process"`), fall back to process env.

## 3. Performance findings & spec

### P-1: Decouple worker count from CPU cores (HIGH)
Workers = `available_parallelism()` but the workload is blocking I/O: one 30-minute tmuxlet turn pins a worker; ~8 concurrent long calls silently queue all other requests — including instant `/health` checks — behind multi-minute waits.
**Spec:** `[server] workers = N` (default `max(16, cores)`), documented as "max concurrent in-flight requests". Long-term (V2 note): dedicated small pool for `/health`+`/v1/models` or thread-per-request.

### P-2: Parse the request body once (LOW)
`handle_chat` parses `raw` twice (typed `ChatRequest` + `serde_json::Value`).
**Spec:** parse to `Value` once, then `ChatRequest::deserialize(&value)`.

### P-3: Build runtime backends once at startup (LOW)
`Backend::from_config` clones names/URLs/`extra_body` per backend per request.
**Spec:** construct `HashMap<String, Backend>` once in `run_serve`, share via `State`.

### P-4: Connection reuse (DEFERRED)
New TCP+TLS handshake per api call. Accepted for V1 (single-digit ms vs multi-second completions); revisit with V2 streaming.

### P-6: Chain-level time budget (MEDIUM)
Worst case today: each chain leg burns its full timeout serially (3 × 1800 s = 90 min for one request).
**Spec:** `[server] chain_budget_secs` (default: unset = current behavior; recommended 2× request_timeout). Each leg gets `min(leg_timeout, remaining_budget)`; when the budget is exhausted, remaining legs are skipped and reported as `[skipped: budget]` in the error detail.

### P-7: Client-disconnect detection (DEFERRED → V2)
An abandoned request still runs its backend to completion, pinning a worker for up to the full timeout. Not cleanly detectable with buffered tiny_http responses; fold into V2 streaming (writes to a dead socket fail fast, letting the worker cancel the child).

### P-9: Failure-aware backend cooldown (HIGH — also feeds the auto-router)
A backend that just returned 429/5xx/timeout is retried at full cost on the very next request; chains repeatedly wait out a dead leg's whole timeout.
**Spec:** in-memory per-backend health record `{consecutive_failures, cooling_until, last_error, last_latency_ms}`. Cooldowns scale with what a wasted retry costs: HTTP 429 → `Retry-After` if present, else `cooldown_secs` (default 60); timeout-class failures (a retry burns the whole timeout) → exponential 30 s → 300 s cap; connect-refused (a retry fails in milliseconds, and the "I just restarted Ollama" case must recover instantly) → flat 5 s. Chain walk skips cooling backends **only when at least one non-cooling leg remains** (never skip the last candidate — availability beats freshness). Skips are never mysterious: each shows as `[skipped: cooling 42s]` in the error detail, transitions are logged (U-8), and live state is visible via U-13. Kill switch: `[server] cooldown = false` (default true).

## 4. UX findings & spec

### U-2: SSE keepalive during long backend runs (HIGH)
With `stream: true`, clients see **zero bytes** until the backend finishes (minutes); most OpenAI clients idle-timeout and abort long before.
**Spec:** for streaming requests, respond immediately with SSE headers and emit `: keepalive\n\n` comment frames every 15 s while dispatch runs on a separate thread (SseBody already implements `Read` — back it with a channel). Emit the existing 4-frame sequence on completion. This is V1-compatible; true token streaming remains V2 (noted in roadmap).

### U-3: Optional strict model matching (MEDIUM)
Unknown `model` silently falls back to `default_chain` — a typo (`thinkng`) silently gets the wrong/expensive chain.
**Spec:** `[server] strict_models = false` (default). When true: unknown model → 404 `model_not_found` (OpenAI envelope). When false: keep fallback but log `[warn] unknown model 'X' → default chain`.

### U-4: Reject unknown config keys (HIGH, trivial)
`timeout_sec = 120` (typo) parses fine and is silently ignored.
**Spec:** unknown keys are **errors under `--validate`** and **warnings at serve time** (log the exact key and, when close, its nearest valid neighbor: `unknown key 'timeout_sec' — did you mean 'timeout_secs'?`). Serde's `deny_unknown_fields` is error-only, so implement by diffing the parsed TOML table against the known key set. Rationale: hard-failing at serve would brick a running service on upgrade and break configs shared across binary versions — strict-validate / forgiving-serve catches the typo without the fragility (UX invariant 2).

### U-5: Deepen `--validate` + add `--check-backends` (HIGH)
Validation gaps found: unparseable `listen` (only fails at bind), chain name shadowing a backend name (router silently prefers the chain), `request_timeout_secs = 0` (means "no deadline" for api but "instant timeout" for cli/tmuxlet), unknown `env_source` values silently treated as "shell", missing `cli` bin, missing `tmuxlet` binary.
**Spec:** `--validate` additionally checks: listen parses as a socket addr; non-loopback listen without auth (S-2); chain/backend name collisions; `request_timeout_secs >= 1` (with "omit for default" hint); `env_source ∈ {shell, process}`. Per UX invariant 2, all of these are errors under `--validate` but warnings at serve time — except conditions that make serving impossible (unparseable listen) or unsafe (S-2). New `--check-backends` flag (network/filesystem-touching, so separate from pure `--validate`): per backend prints PASS/FAIL — cli: bin exists+executable; tmuxlet: binary resolvable (F-4 path) + `tmuxlet --version`; api: TCP connect to host:port. Exit non-zero on any FAIL.

### U-6: Treat empty backend output as failure (MEDIUM)
A backend returning empty/whitespace content is a "success" that ends the chain — the client gets an empty message while working fallbacks sit unused (reachable: PTY grace-window loss, tmuxlet empty `output`, echo-style misconfig).
**Spec:** after dispatch, `content.trim().is_empty()` → `BackendError::Backend(name, "empty output")` → chain advances. Escape hatch: `allow_empty = true` per backend.

### U-7: Surface upstream error bodies (HIGH, small)
`api` non-2xx discards the response body — the user sees `[openrouter] HTTP 401` but not OpenRouter's actual "invalid key / out of credits / bad model" message.
**Spec:** include first 300 chars of the upstream body (control chars stripped) in `BackendError::Http`; flows into logs and 503 detail (subject to S-6 redaction).

### U-8: Minimal leveled logging with timing (MEDIUM)
`log_level` is parsed but unused; logs lack timestamps, durations, and request correlation; env-capture fallback (U-8a) is silent.
**Spec:** tiny internal logger (no new deps): `ts level msg` to stderr, honoring `server.log_level`. Per request: one `info` line `reqid model=X route=chain elapsed_ms=N backend=Y status=ok|fallback|fail`; per skipped leg: `warn` with error class; `debug` adds body sizes and per-leg latency. Log env-capture fallback and cooldown transitions.

### U-9: Sort `/v1/models` (trivial)
HashMap order → listing order changes every restart. **Spec:** sort ids; chains first, then backends, each alphabetical.

### U-10: Optional CORS (LOW)
Browser-based clients can't call the server (no OPTIONS handling, no CORS headers).
**Spec:** `[server] cors_origins = ["http://localhost:5173"]` (default empty = no CORS headers, current behavior). When set: answer `OPTIONS` preflight 204 with allow-headers `authorization, content-type`; echo matching origin.

### U-11: `cwd` for cli backends + consistent default (MEDIUM, small)
`cli` has no `cwd` option; today PTY runs in `$HOME` while plain runs in the server's cwd — two different implicit behaviors in one backend type.
**Spec:** add optional `cwd` (tilde-expanded, like tmuxlet's); default both modes to `$HOME`.

### U-12: Wider PTY columns (MEDIUM, small)
24×80 PTY hard-wraps long lines; `clean_output` keeps the injected newlines → returned content is mangled mid-word (code lines especially).
**Spec:** default 200 cols × 50 rows; optional per-backend `pty_size = [cols, rows]`. 200 rather than 500: TUIs render width-proportional chrome (horizontal rules, padded banners), and at 500 cols the cleaned output would carry 500-character decoration lines — 200 fixes mid-word wrapping for realistic content without amplifying chrome.

### U-13: Backend status endpoint (MEDIUM)
`/health` is static; no way to see which chain legs are healthy, cooling, or how slow.
**Spec:** `GET /v1/backends` (auth-gated per S-1) → JSON array: `{name, type, state: ok|cooling|unknown, consecutive_failures, last_error, last_latency_ms, cooling_until}` from the P-9 health records. Add `"version"` to `/health`.

### U-14: Prompt shaping options (MEDIUM)
`[System]:` vs `User:` label inconsistency; no way to send a raw prompt — coding CLIs receive `User: <text>` even for single-message requests, which changes their behavior vs. direct invocation.
**Spec:** per-backend `prompt_mode = "transcript" | "last_user"` (default `transcript` = current). `last_user` sends the final user message verbatim (system messages, if any, prepended plainly). Unify labels: `System:` (drop the brackets).

### U-15: Part-joining and dropped-part visibility (LOW)
Multi-part text content is joined with `""` (words glued); non-text parts (images) are dropped silently.
**Spec:** join text parts with `\n`; if any non-text part was dropped, log `warn` (and in strict mode U-3, still accept — dropping is documented).

### U-16: Usage token reporting (RESOLVED — keep the zeros)
`usage: 0/0/0` renders as "0 tokens" in client UIs. **Resolution (UX review):** keep the zeros. Fabricated `len/4` estimates would feed cost-tracking tools plausible-looking wrong numbers; zeros are honestly "no data". Document the zeros in the README instead. No work item.

### U-17: HEAD + 405 correctness (LOW)
`HEAD /health` → 404; wrong-method on known paths → 404. **Spec:** HEAD mirrors GET (empty body); known path + wrong method → 405 with `Allow` header.

### U-18: Help shows defaults (trivial)
**Spec:** `--help` prints the default config path (`~/.tmuxlet/server.toml`) and default listen address; support `--config=FILE` syntax alongside the two-token form.

### U-19: IPv6 base_url support (LOW)
`split_url` breaks on `http://[::1]:11434/v1`. **Spec:** handle bracketed IPv6 literals in authority parsing.

### U-20: Per-backend concurrency cap (MEDIUM)
Two simultaneous requests to the same tmuxlet target spawn two tmux sessions/TUIs (hundreds of MB each; tmuxlet's own machine cap may queue or reject).
**Spec:** optional per-backend `max_concurrent = N`; a saturated backend returns a `Busy` error class immediately so the chain advances (counts as failure for P-9 only after M consecutive rejections).

### U-22: Config reload (DEFERRED, adjudicated F12)
SIGHUP-triggered reload (re-load + validate + swap `Arc<State>`; in-flight requests finish on the old config). Deferred to the roadmap tail per grok's verdict (Section 6).

### U-23: Readable 503 detail (trivial)
`all backends failed: ["[a] …","[b] …"]` is JSON-inside-a-string. **Spec:** `message` becomes a compact human summary — `all backends failed: agy (spawn), ollama-kimi (timeout), claude-thinking (exit 1)` — so clients that surface only `message` (most chat UIs) keep one-glance debuggability; the full per-leg strings move to a structured `error.details` array (the OpenAI envelope tolerates extra fields); keep `code: all_backends_failed`.

### U-24: Custom CA / insecure TLS knob (LOW)
LAN upstreams with self-signed certs can't be used. **Spec:** per-backend `ca_file = "path.pem"`; explicitly **no** blanket `insecure` flag (footgun) — a self-signed cert can be added as a CA instead.

## 5. Auto-router (user-requested feature)

A fast classifier model routes each request to a task-appropriate chain: best models (with API-limit-aware fallbacks) for research/planning/code-review; cheaper/faster targets for execution/commit work.

### 5.1 Config

```toml
[routers.auto]                        # name "auto" becomes a routable model id
classifier = "ollama-qwen-fast"       # any defined backend; should answer in <2s
classifier_timeout_secs = 5           # classifier leg only; on expiry -> fallback_class
classifier_max_chars = 4000           # tail-truncate transcript fed to classifier
fallback_class = "execution"          # used when classifier fails/times out/answers garbage

[routers.auto.routes]                 # class -> chain or backend (validated)
research    = "best"
planning    = "best"
code_review = "best"
execution   = "fast"
commit      = "fast"

[chains.best]
order = ["openrouter-opus", "openrouter-sonnet", "claude-thinking"]   # P-9 cooldown makes
                                                                      # rate-limited legs skip fast
[chains.fast]
order = ["ollama-kimi", "agy"]
```

### 5.2 Mechanics

1. Request with `model = "auto"` (any router name) enters routing; all other models bypass it entirely.
2. Classification prompt (built-in template, overridable via `classifier_prompt`): the class list with one-line definitions + the **last user message** (truncated to `classifier_max_chars`, tail-biased) + "Answer with exactly one label."
3. Dispatch to `classifier` with `classifier_timeout_secs`. Parse: exact case-insensitive match on the class set; else first class label contained in the reply; else `fallback_class`. Classifier failure/timeout → `fallback_class` (never fail the request because routing failed).
4. Resolve `routes[class]` through the existing router (`chain or single backend`) and walk it with the existing fallback + P-9 cooldown semantics — this is what delivers "best models with fallbacks when API limits are hit."
5. Observability: log `[route] auto class=code_review chain=best classifier_ms=800`; response includes header `x-tmuxlet-route: code_review/best/openrouter-opus`; `model` in the response body stays the winning backend's label (current behavior).

### 5.3 Validation & guardrails

- `--validate`: `classifier` must be a defined backend (not a router); every `routes` value must resolve to a chain or backend; `fallback_class` must be a key of `routes`; router names must not collide with chain/backend names.
- Router names appear in `/v1/models` (sorted first, per U-9).
- No router-to-router routing (flat, one hop) — keeps resolution non-recursive.
- Cost bound: classifier input truncation + a tight 5 s default timeout caps routing overhead — a hop whose failure falls back safely must not be allowed to double a fast request's latency; with a local Ollama model the added-latency target is <1 s.
- V1.5 option (deferred): per-conversation classification cache keyed on a hash of the first user message, so multi-turn conversations classify once.

## 6. Out-of-scope findings — grok adjudication

Findings outside security/performance/UX were submitted to `tmuxlet --target grok` for an include/exclude verdict, per the goal directive.

Verdicts (grok, via `tmuxlet -p --target grok`, run `18c0b77ecec8e8c0-16b59-0`, 2026-07-09):

| ID | Finding | Verdict | Rationale (grok, condensed) |
|---|---|---|---|
| F-1 | PTY timeout/kill returns partial output as success | **INCLUDE** | Silent success disables fallback and ships truncated answers; cheap to flip to a hard error in the same timeout path |
| F-2 | ANSI stripper misses DCS / 8-bit CSI | EXCLUDE | Real but niche; small correctness follow-up unless the stripper is rewritten anyway |
| F-3 | No `catch_unwind` worker panic guard | **INCLUDE** | Panics can silently zero capacity; one-line guard protects every other optimization |
| F-4 | `tmuxlet` spawned by bare name (ambiguous PATH semantics) | **INCLUDE** | Absolute-path resolution at startup/validate makes spawn failures deterministic; nearly free alongside env work |
| F-5 | HTTP client ignores Content-Length (EOF-only framing) | **INCLUDE** | False read-timeouts cause unnecessary fallbacks and would mis-measure any api-backend perf work |
| F-6 | Connect uses first DNS address only | EXCLUDE | Low value for a localhost-centric daemon; revisit if remote upstreams become first-class |
| F-7 | Multi-line env values corrupt `parse_env` | **INCLUDE** | Security-adjacent, cheap (`env -0`); env-hardening (S-3) shouldn't rest on a brittle parser |
| F-8 | PTY prompt-write errors silently ignored | **INCLUDE** | Empty-prompt runs look like random breakage; trivial check |
| F-9 | No `/metrics` endpoint | EXCLUDE | New surface, separate ops epic (U-13 status endpoint covers the tuning need) |
| F-10 | CI/local Rust toolchain skew | EXCLUDE | Release hygiene → release-ops checklist, not this spec |
| F-11 | No tagged release (`cargo install --git` unpinned) | EXCLUDE | Pure release-ops; do in the same release train if convenient |
| F-12 | No SIGHUP config reload | EXCLUDE | Operability feature work; restart is acceptable for a localhost V1 daemon |

**Included items — spec:**

- **F-1:** `run_in_pty` returns `Err(Timeout)` when the watchdog fired (track the kill via a flag/channel), and `cli::dispatch` (PTY path) checks child exit status like the plain path does (non-zero + empty output → `Exit` error). Chain then advances as designed.
- **F-3:** wrap `handle(req, &state)` in `std::panic::catch_unwind` (request moved in; on panic, log the payload and continue the worker loop). Add a test that a panicking route doesn't kill the worker.
- **F-4:** at startup, resolve `tmuxlet` to an absolute path using the **captured** env's PATH (`env.get("PATH")`), store it in the runtime backend (P-3 makes this natural); `--check-backends` (U-5) reports the resolved path or FAIL.
- **F-5:** parse `Content-Length` when present and stop reading at that many body bytes; keep EOF/close as fallback framing. Chunked path unchanged.
- **F-7:** implemented as S-7 (`env -0`, NUL-split, line-split fallback).
- **F-8:** propagate prompt write/flush errors from `run_in_pty` as `Err` (spawn-class), so the chain advances instead of returning confusing output.

**Excluded items** are tracked outside this spec: F-2 (correctness follow-up), F-6 (revisit with remote upstreams), F-9 (ops epic), F-10/F-11 (release-ops checklist — see `docs/STATUS.md` next steps), F-12 (operability backlog; U-22 stays deferred).

**Additional risk grok flagged (accepted):** client disconnect / request cancel never kills the running backend child (tmuxlet/CLI/PTY or in-flight HTTP); orphaned long runs hold workers and defeat concurrency gains. This strengthens P-7: V2 streaming work **must** pair disconnect detection with cancel propagation to the child process (kill on write-failure), plus the bounded worker queue from P-1/U-20.

## 7. Prioritized roadmap

**Phase 1 — correctness & trivial wins (small PRs, no behavior risk):**
U-4 deny_unknown_fields · U-9 sort models · U-7 upstream error bodies · U-23 readable 503 · U-18 help text · P-2 single parse · P-3 prebuilt backends · U-15 part joining · F-1 PTY timeout=error · F-3 panic guard · F-4 absolute tmuxlet path · F-5 Content-Length framing · F-8 PTY write errors · U-11 cli cwd · U-12 PTY size · U-6 empty-output failure · S-9 env-capture timeout · S-7/F-7 env -0 · U-5 deeper validate.

**Phase 2 — security posture:**
S-1 bearer auth · S-2 bind policy · S-5 response cap · S-3 env allowlist · S-4 prompts off argv · S-6 error redaction.

**Phase 3 — headline UX/perf (the daily-driver upgrades):**
P-9 cooldown + U-13 status endpoint · U-2 SSE keepalive · P-1 worker sizing · P-6 chain budget · U-8 leveled logging · U-3 strict models · **Section 5 auto-router** (depends on P-9 + U-8) · U-20 per-backend concurrency · U-5 --check-backends.

**Phase 4 — V2 & deferred:**
True token streaming passthrough (supersedes buffered SSE) · P-7 disconnect detection + cancel propagation (falls out of streaming) · U-22 SIGHUP reload · P-4 connection reuse · U-10 CORS · U-14 prompt modes · U-17 HEAD/405 · U-19 IPv6 · U-24 custom CA · release-ops items per Section 6 verdicts.

## 8. UX review pass (rev 2, 2026-07-09)

Every proposed change was re-evaluated against the project's heart — one binary, one TOML, zero-friction localhost daemon — with three things to preserve simultaneously: the project's character, the day-to-day UX, and each change's original intent. Amendments were applied in place above; this is the log.

| Item | Verdict | Adjustment / rationale |
|---|---|---|
| S-1 auth | **Amended** | The env-var token was the fickle part (env delivery under launchd/systemd is the project's documented failure mode). Now `auth = true` auto-generates `~/.tmuxlet/token` (0600), 401s log where the token lives, `auth_token_env` remains as an alternative. Client side unchanged — every OpenAI client already has a key field. |
| S-2 bind policy | **Amended** | Refusal prints both remedies verbatim; also surfaced by `--validate` so it never appears first at a service restart. |
| S-3 env allowlist | **Amended** | Informational note confined to `--validate`; no runtime nagging for a deliberate default. Always-passed base env set prevents "works in terminal, fails in server" bugs. |
| S-4 prompts off argv | **Amended** | tmuxlet stdin support probed once at startup and cached; a probe failure selects argv mode, never fails a request. |
| S-5 response cap | Keep | Invisible until a >64 MiB response; the error names the config knob. |
| S-6 redaction | **Amended** | Redact only on non-loopback binds. Loopback keeps full 503 detail even with auth on — the troubleshooting flow depends on reading it. |
| S-7 / S-9 | Keep | Invisible robustness; the S-9 warning names its fix. |
| P-1 / P-2 / P-3 / P-6 | Keep | Invisible or opt-in; defaults preserve current behavior. |
| P-9 cooldown | **Amended** | Cooldown now scales with retry cost: connect-refused = flat 5 s (the "just restarted Ollama" case recovers instantly), timeout-class 30→300 s, 429 honors Retry-After. Skips always visible as `[skipped: cooling Ns]`. |
| U-2 keepalive | Keep | Pure win; SSE comment frames are spec-legal and industry practice (OpenRouter emits them). |
| U-3 strict models | Keep | Default off preserves behavior; the warn log covers the typo case for free. |
| U-4 unknown keys | **Amended** | `deny_unknown_fields` would hard-fail running services on upgrade and break configs shared across versions. Now: error in `--validate`, warning (with did-you-mean) at serve. |
| U-5 validate depth | **Amended** | Same strict-validate / forgiving-serve split applied to every new check (except unsafe/impossible conditions). |
| U-6 empty output | Keep | Fallback legs may add spend, but producing an answer is what chains are for; `allow_empty` escape hatch + logged transition. |
| U-7 / U-8 / U-9 / U-13 / U-17 / U-18 / U-19 | Keep | Pure wins with zero setup surface. |
| U-10 / U-14 / U-20 / U-24 | Keep | Opt-in only; absent from the example config happy path (invariant 4). |
| U-11 cli cwd | Keep | Documented behavior change (plain-cli default cwd → `$HOME`): relying on a service's inherited cwd is fragile, and the new `cwd` key is the escape hatch. |
| U-12 PTY size | **Amended** | 500 cols would amplify TUI chrome (full-width rules become 500-char lines). Default 200×50. |
| U-16 usage estimates | **Cut** | Fabricated token counts feed cost trackers plausible wrong numbers; honest zeros win. Removed from the roadmap. |
| U-23 503 detail | **Amended** | Compact summary stays in `message` (clients that only render `message` keep one-glance debuggability); full strings move to `error.details`. |
| Auto-router (§5) | **Amended** | Fully opt-in (no `[routers.*]` block = zero change anywhere). Classifier timeout default tightened 10 s → 5 s. Misroutes stay diagnosable via `x-tmuxlet-route` + the route log line. |
| F-1 / F-3 / F-4 / F-5 / F-7 / F-8 | Keep | Invisible correctness; F-1 making timeouts advance the chain is the fix, not a regression. |

**Net intentional behavior changes** (everything else is default-compatible): U-6 empty output advances the chain · U-11 plain-cli default cwd becomes `$HOME` · U-12 PTY size 80×24 → 200×50 · U-15 text parts joined with `\n` · F-1 PTY timeout is an error · U-7/U-23 error payloads enriched/restructured. Each strictly improves the returned content or error; none requires touching an existing config.
