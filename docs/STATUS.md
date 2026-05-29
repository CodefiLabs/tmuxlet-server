# tmuxlet-server — Status & Next Steps

_Last updated: 2026-05-29_

## TL;DR

**V1 is complete, verified, and pushed.** All 15 plan tasks are implemented;
CI is green on macOS + Linux.

- **Repo:** https://github.com/CodefiLabs/tmuxlet-server (public, `main`)
- **CI:** `fmt + clippy + test` ✅ on `ubuntu-latest` and `macos-latest`
- **Tests:** 52 passing (31 unit + 21 integration across 8 binaries)
- **Gates:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, full suite — all clean
- **Plan:** [`docs/superpowers/plans/2026-05-28-tmuxlet-server.md`](superpowers/plans/2026-05-28-tmuxlet-server.md)

## What it does

A standalone synchronous Rust binary (edition 2024, no async) that wraps the
`tmuxlet` CLI in an OpenAI-compatible `/v1/chat/completions` HTTP service, with a
TOML-configured fallback chain over three backend types: `tmuxlet`, `api`
(OpenAI-compatible HTTP upstream), and `cli` (exec a binary, optionally in a PTY).

Implemented and exercised end-to-end: `/health`, `/v1/models`, streaming +
non-streaming `/v1/chat/completions`, reserved-route 501s, 404 envelope,
per-backend timeout override (1800s default), graceful SIGTERM shutdown.

## Verification (done)

A 6-lens adversarial review (concurrency, dispatch, HTTP protocol, spec-contract,
robustness/security, test gaps) surfaced **18 real issues — all fixed**, notably:

- **Critical:** per-backend `timeout_secs`/default was dropped on the `api` path
  and `http_client` had no socket timeouts → a hung upstream could wedge a worker
  forever and defeat fallback. Now timeout is threaded through
  `api::dispatch` → `post_json` (connect + read/write timeouts).
- HTTP client: decode `Transfer-Encoding: chunked`; tolerate unclean TLS close
  (`UnexpectedEof`).
- PTY watchdog armed before the (blockable) stdin write.
- Bounded reader-thread joins (grandchild-pipe hang).
- 16 MiB request-body cap.
- `cli` non-zero exit + empty stdout now errors (so the chain advances).

## Known limitations / deferred (not bugs)

- **`agy` backend unverified live.** `/Users/kk/.antigravity/antigravity/bin/agy`
  is a broken symlink (Antigravity.app not installed). Code path is covered by
  test doubles; reinstall Antigravity before relying on it.
- **Loopback bind is convention, not enforced** — by spec (no auth, 127.0.0.1).
- **No worker panic-guard.** No reachable panic in the request path today; a
  `catch_unwind` around `handle()` would harden against future regressions.
- **Streaming is buffered (V1).** The upstream call completes, then the response
  is re-emitted as SSE frames — not token-by-token passthrough. Documented.
- **Toolchain skew.** CI uses `dtolnay/rust-toolchain@stable` (currently 1.96);
  local dev was 1.92. A 1.96 clippy lint (`while_let_loop`) was caught only by CI.
  To lock them, pin a version in CI or `rustup update` locally.

## Next steps (suggested, roughly prioritized)

1. **Tag a release.** Cut `v0.1.0` (annotated tag + GitHub release) so
   `cargo install --git … --tag v0.1.0` is reproducible.
2. **Verify the install flow.** Run `cargo install --git https://github.com/CodefiLabs/tmuxlet-server`
   on a clean machine and walk `AGENTS.md` end-to-end.
3. **Live-smoke a real `api` upstream** (Ollama or OpenRouter) to confirm the
   chunked/close_notify handling against a production server, not just the mock.
4. **Reinstall Antigravity.app**, then smoke the `agy` `cli` (pty=true) backend
   via `scripts/smoke.sh`.
5. **Optional hardening:** `catch_unwind` worker guard; pin CI Rust version;
   bump `actions/checkout` to a Node24-compatible release (CI deprecation notice).
6. **Optional features (post-V1):** true streaming passthrough; structured
   leveled logging wired to `server.log_level` (currently parsed but unused).
