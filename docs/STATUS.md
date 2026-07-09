# tmuxlet-server — Status & Next Steps

_Last updated: 2026-07-09_

## TL;DR

**V1 plus the optimization spec (Phases 1–4) is complete and verified.** The
`2026-07-09-optimization-spec.md` roadmap is implemented end-to-end on top of
v0.1.1; CI gates are green.

- **Repo:** https://github.com/CodefiLabs/tmuxlet-server
- **Branch:** `feat/optimization-spec` (based on `origin/main` = v0.1.1)
- **Tests:** 93 passing (65 unit + 28 integration across 8 binaries)
- **Gates:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, full suite — all clean
- **Spec:** [`docs/specs/2026-07-09-optimization-spec.md`](specs/2026-07-09-optimization-spec.md)

## What it does

A standalone synchronous Rust binary (edition 2024, no async) that exposes an
OpenAI-compatible `/v1/chat/completions` HTTP service backed by a TOML-configured
fallback chain over three backend types: `tmuxlet`, `api` (OpenAI-compatible HTTP
upstream), and `cli` (exec a binary, optionally in a PTY). Endpoints: `/health`,
`/v1/models`, `/v1/backends`, streaming + non-streaming `/v1/chat/completions`,
plus a TOML-driven auto-router (`§5`) that classifies a request and routes it to a
chain/backend.

## Optimization spec — implemented

**Phase 1 (correctness):** F-1 timeout→error (no silent truncation), F-4 PATH
resolution to an absolute path (executable-bit checked), F-5 Content-Length-aware
body read, F-8 PTY write-error propagation, U-6 empty-output-as-failure with an
`allow_empty` escape hatch, U-15 multi-part content join.

**Phase 2 (security):** S-1 bearer auth (constant-time compare; token from env or
a 0600 `~/.tmuxlet/token`), S-2 loopback-bind policy (refuse non-loopback without
auth), S-3 env allowlist (glob, base vars always passed), S-4 stdin-prompt for
CLI/tmuxlet, S-5 response-byte cap, S-7 `env -0` NUL parsing, S-9 env-capture
watchdog.

**Phase 3 (headline UX/perf):** P-1 worker sizing (`max(16, cores)`, clamped ≥1),
P-6 chain time budget, P-9 failure-aware cooldown, U-2 SSE keepalive, U-3 strict
models, U-8 leveled logging + request ids, U-13 `/v1/backends`, U-20 per-backend
concurrency cap (enforced on chain legs **and** the router classifier), U-4/U-5
config linting (unknown-key + did-you-mean), U-7/U-23 error detail, and the
**auto-router** (`§5`).

**Phase 4 (opt-in correctness/features):** U-17 (HEAD mirrors GET; wrong method →
405 + `Allow`), U-19 (bracketed IPv6 `base_url`), U-14 (per-backend
`prompt_mode = transcript | last_user`; unified `System:` label), U-10 (optional
CORS via `[server] cors_origins`), U-24 (per-backend `ca_file` custom-CA TLS, no
blanket insecure flag).

All new keys are opt-in — an existing pre-spec config still loads with zero new
required keys (UX invariant 1). `--validate` is strict; `serve` is forgiving
(config smells warn and continue). Every refusal names its one-line fix.

## Verification (done)

A 5-dimension adversarial subagent fan-out (security, concurrency/perf,
correctness/dispatch, protocol/HTTP, config/validation) reviewed the full
Phase 1–4 diff; each candidate finding was independently refuted before being
accepted. **5 confirmed findings — all fixed with regression tests:**

- **S-1:** `open(2)`'s `.mode(0600)` is ignored for a pre-existing token file, so
  a stale 0644 `~/.tmuxlet/token` left the generated secret world-readable — now
  forced to 0600 via `set_permissions`.
- **U-20:** the router classifier dispatch bypassed `max_concurrent` — now gated
  with the same acquire/release as chain legs (degrades to `fallback_class` at
  capacity; routing never fails the request).
- **S-5:** a hostile `Content-Length` near `usize::MAX` overflowed the body-framing
  offset (debug panic / release corruption) — now `checked_add`.
- **P-1:** `workers = 0` bound the socket, printed "listening", then exited 0 —
  now clamped to ≥1 with a lint warning.
- **F-4:** `resolve_program` accepted the first `is_file()` PATH match, so a
  non-executable shadow masked the real binary — now requires the exec bit and
  keeps scanning (execvp semantics).

The earlier 6-lens V1 review (18 issues, all fixed) remains in git history.

## Deferred to V2 (by spec, not bugs)

- **True token streaming passthrough** (supersedes the current buffered SSE, which
  completes the upstream call then re-emits frames with 15s keepalives).
- **P-7 client-disconnect detection + cancel propagation** to the backend child
  (folds into the streaming work).
- **U-22 SIGHUP config reload** and **P-4 HTTP connection reuse.**
- **P-9 `Retry-After`** is not yet honored for 429 (uses `cooldown_secs`);
  documented in `cooldown_for`.

## Next steps (suggested)

1. **Open the PR** from `feat/optimization-spec` into `main` and let CI run the
   gates on Linux + macOS.
2. **Live-smoke** a real `api` upstream (Ollama / OpenRouter) and, if reachable, a
   `tmuxlet` backend end-to-end.
3. **Exercise the new opt-in paths** manually: auth on (`token` file), CORS from a
   browser client, a `last_user` prompt backend, and a self-signed `ca_file`
   upstream.
4. **Tag a release** once the PR merges.
