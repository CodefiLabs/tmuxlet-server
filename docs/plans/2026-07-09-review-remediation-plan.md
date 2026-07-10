# Post-Implementation Review & Remediation Plan

**Date:** 2026-07-09
**Scope reviewed:** the full `feat/optimization-spec` diff vs v0.1.1 (`e71b918..HEAD`, 10 commits, ~3,400 added lines, 17 files) — including the fix commit `ecf7dc7` and docs commit `7a384e7`, which landed after the previous verification pass and had never been reviewed.
**Method:** 6-dimension adversarial review workflow (70 agents: 6 finders → severity-scaled refuter panels; majors faced 3 independent lenses — code-trace, reproduction, design-intent), cross-checked against an independent manual pass. 44 candidates → 43 confirmed, 1 refuted. Key spec-divergence claims re-verified by hand against the spec text and code.

**Nothing in this plan is implemented yet.** The branch remains as reviewed: 93 tests green, clippy/fmt clean.

---

## TL;DR

The implementation is fundamentally sound — no confirmed finding invalidates a shipped feature, and the previous session's five fixes all hold on their tested paths. But the review surfaced:

1. **Two real behavioral gaps** the earlier fixes missed: the S-1 permission repair never runs on the *token-reuse* path (the most persistent variant of the original exposure), and the U-20 concurrency counters leak permanently on a dispatch panic — one panic can brick a `max_concurrent = 1` backend until restart.
2. **A documentation cliff:** ~19 new config keys, a new endpoint, auth, and new HTTP semantics exist only in the spec and source comments. README, AGENTS.md, and the example config were never touched (AGENTS.md now states the false claim "auth is not checked in V1").
3. **Five silent spec divergences** (route header, router log, request log fields, Busy-vs-cooldown, `cooling_secs` naming) — each needs either a small implementation or an explicit spec amendment; none should stay silent.
4. **A test-coverage debt:** several complete features (U-20 caps, S-1 auth gate wiring, strict_models, /v1/backends, S-6 redaction) have zero tests — each could be reverted with CI staying green.

Suggested order: Phase A (behavior/security fixes) → Phase B (spec-compliance decisions) → Phase C (test hardening) → Phase D (docs) → Phase E (optional polish). A–D are recommended before opening the PR; E is discretionary.

---

## Phase A — Behavior & security fixes (must-fix)

### A1. S-1: repair permissions on the token-reuse path — **major**
`src/auth.rs:60-65` returns any existing non-empty token file as the live bearer secret without ever checking its mode; the 0600 enforcement lives only in `write_token()`, which the reuse path never reaches. A user-provisioned token (`echo tok > ~/.tmuxlet/token` under umask 022) or a backup-restored file stays world-readable forever, silently — and the fix comment at `auth.rs:93-94` plus STATUS.md currently overstate coverage. Confirmed by all three refuter lenses.
**How:** in `resolve()`'s reuse branch (unix), stat the file; if `mode & 0o077 != 0`, `fs::set_permissions(0o600)` and log one `[warn]` naming what was repaired (forgiving-serve philosophy).
**Guard:** test that writes a **non-empty** 0644 token file, calls `resolve()`, asserts the same token comes back and the mode is now 0600. This test fails on the current tree (the existing regression test seeds an *empty* file, which masks exactly this gap).

### A2. RAII guard for the `state.active` concurrency counters — **major**
Both `run_chain` (`http.rs:599-607` / `621-626`) and `classify_task` (`http.rs:747-765`) increment under the cap and decrement only on the straight-line path. The server is *designed* to survive dispatch panics (F-3 `catch_unwind` at `http.rs:806-814`), but a panic between increment and decrement leaks the slot permanently: with `max_concurrent = 1` (the natural tmuxlet setting), one panic makes every future request Busy / fallback-classed until restart. A concrete in-tree panic source exists: `Instant::now() + timeout` in `cli.rs:137` / `tmuxlet.rs:104` overflows for absurd-but-accepted `request_timeout_secs`.
**How:** add a small `ActiveSlot<'a>` struct in `http.rs` (holds the mutex ref + backend name; `Drop` decrements with `saturating_sub`), with a `try_acquire(...) -> Option<ActiveSlot>` constructor. Replace both manual acquire/release pairs — this also deletes the duplication (subsumes quality finding C17).
**Guard:** unit test that drops the guard inside `std::panic::catch_unwind` and asserts the counter returns to 0.

### A3. Streaming dispatch thread: recover panics in-band — **minor but nasty failure mode**
`stream_with_keepalive` runs `run_chain` on a bare `thread::spawn` (`http.rs:475-487`) with no unwind protection. A panic there ends the already-committed 200 SSE stream at clean EOF — zero data frames, no error, no `data: [DONE]` — indistinguishable from an empty success.
**How:** wrap the spawned `run_chain` call in `catch_unwind(AssertUnwindSafe(...))`; map a panic to `Err(vec![BackendError::Backend("chain", "internal panic: …")])` so the coordinator emits the existing in-band 503 body + `[DONE]`, plus a `log::error`. Also clamp or `checked_add` the `Instant` arithmetic for `chain_budget_secs` / huge timeouts (removes the known panic source feeding A2/A3).
**Guard:** integration test: chain whose dispatch panics with `stream: true` yields SSE containing `all_backends_failed` and `data: [DONE]`.

### A4. Logging: never panic, never log under the health lock — **minor, outage-class**
`record_failure` (`http.rs:661-676`) calls `log::info` while holding `state.health`; `log::line` uses `eprintln!`, which blocks on a stalled stderr pipe (stalling every worker that touches the health lock) and **panics** on EPIPE — and a panic at that call site poisons `state.health`, killing every subsequent request permanently.
**How:** (1) end the guard's scope before the log call in `record_failure`; (2) in `src/log.rs`, replace `eprintln!` with `let _ = writeln!(std::io::stderr().lock(), …)` so no log call can panic. Fold in A5 (same function).
**Guard:** unit-test the sanitizer/formatter directly; scoped-block by review for the lock ordering.

### A5. Sanitize control characters at the log sink — **minor (log forging)**
Client-controlled strings (e.g. `model`) are interpolated raw into stderr lines (`http.rs:405`); `\n` + ANSI escapes forge well-formed log lines (CWE-117) or manipulate the operator's terminal.
**How:** sanitize once in `log::line()` — replace control chars before writing. Covers all current and future call sites.
**Guard:** unit test: a message containing `\n` and `\x1b` produces one line with no raw control bytes.

### A6. `write_token`: new inode instead of in-place truncate — **minor (S-1 residual)**
In-place truncate + fchmod (`auth.rs:86-95`) reuses the inode, so a pre-held fd on the old world-readable file can read the freshly generated token.
**How:** if the path exists, `fs::remove_file` then open with `.create_new(true).mode(0o600)` (new inode, 0600 from birth). Existing tests pass unchanged.

---

## Phase B — Spec-compliance decisions

Each item below is a silent divergence from `docs/specs/2026-07-09-optimization-spec.md`. Resolve each by **implementing** or by **amending the spec + STATUS** — never by leaving it silent. Recommendations:

### B1. `x-tmuxlet-route` missing the winning-backend segment — **major** → implement
Spec §5.2(5): `x-tmuxlet-route: class/chain/backend`. Code emits only `class/chain` (`http.rs:391`), and the winner is unrecoverable from the body (api backends report the upstream model id). This is the segment that tells you which fallback actually answered.
**How:** return the winning backend name from `run_chain` (add to `DispatchResult` or the Ok tuple); emit `{class}/{target}/{winner}` on the non-streaming path. Streaming sends headers before dispatch — keep two segments there and document it (or emit the winner as a leading SSE comment).
**Guard:** integration test asserting three segments, third names the backend.

### B2. Router log line hard-codes `auto` — **minor** → implement (one line)
`http.rs:388` logs the literal `route auto` regardless of the router's actual name; a `[routers.smart]` config becomes undiagnosable. Change to `route {model}`.

### B3. U-8 closing request log line — **minor** → implement
Spec: one info line per request with `model= route= backend= status=ok|fallback|fail`. Code logs success-only, without model/route/status, and a fully-failed request logs **no** request-level line (`http.rs:637-649`).
**How:** emit one closing line in `handle_chat` / stream completion: `{reqid} model={model} route={label|-} elapsed_ms={total} backend={winner|-} status={ok|fallback|fail}`.

### B4. Busy rejections never feed P-9 cooldown — **minor** → amend spec (recommended)
Spec U-20 says Busy "counts as failure for P-9 only after M consecutive rejections"; code never records Busy at all (`http.rs:603`). But cooling a saturated backend is arguably *wrong* for this design: a saturated backend recovers the instant a slot frees, and rejection costs microseconds — cooldown would only delay recovery.
**Recommended:** amend the spec + STATUS to state Busy is deliberately excluded from cooldown, and fix the real symptom instead: `/v1/backends` should report `state: "busy"` when `active == max_concurrent` (currently misreports "ok"). Guard with a /v1/backends assertion (folds into C-batch test for U-13).

### B5. `/v1/backends` emits `cooling_secs`, spec says `cooling_until` — **minor** → amend spec
`cooling_secs` (relative seconds) is the better API — `Instant` has no wall-clock form and relative seconds are self-contained for a localhost tool. Record the deliberate divergence in the spec §U-13 + STATUS, and document the endpoint's actual JSON shape in README (Phase D).

### B6. `strict_models` 404 doesn't name its fix — **minor** → implement (invariant 3)
`http.rs:400`: `unknown model '{model}'` names neither `GET /v1/models` nor `strict_models = false`. Every other new refusal in the diff carries its remedy; this is the one that doesn't.
**How:** `"unknown model '{model}' — list valid ids at GET /v1/models, or set strict_models = false to fall back to default_chain"`.

### B7. `cors_origins` gets no lint — **minor** → implement
`--validate` accepts entries that can never match: `"*"` (no wildcard support — matching is exact equality at `http.rs:189`), trailing slash, missing scheme, uppercase, and the attacker-controllable `"null"`. prompt_mode and ca_file both got the strict-validate/forgiving-serve treatment; cors_origins got nothing.
**How:** lint() arm flagging each bad shape with the corrected value in the message (e.g. `"*" is not supported — list each origin explicitly, e.g. http://localhost:5173`). Unit tests mirroring `lint_flags_missing_ca_file`.

### B8. `log_level` drift: example lists levels the logger doesn't have — **minor** → implement
`examples/server.toml:12` says `# trace | info | warn | error`; `src/log.rs` implements `error|warn|info|debug` (`trace` silently coerces to info; `debug` — the useful one — is undiscoverable). Also: lint warns on unknown `env_source` but not unknown `log_level`.
**How:** fix the example comment to `# error | warn | info | debug`; add the matching lint arm + unit test.

---

## Phase C — Test hardening

Each of these guards an implemented behavior that can currently be reverted with CI green. Roughly in value order:

| # | Test | Where | Guards |
|---|------|-------|--------|
| C1 | Auth gate wiring: no-token → 401 (`unauthorized` envelope) on /v1/models, /v1/chat/completions, /v1/backends; valid Bearer → 200; **/health stays open** | new `tests/auth.rs` (hermetic via `auth_token_env` + existing `start_with_env`) | S-1 gate route set |
| C2 | U-20: saturated leg falls through to next; all-saturated → 503 `busy`; a third request after completion succeeds (guards the decrement); classifier at capacity degrades to fallback_class (also first end-to-end router test) | new `tests/concurrency.rs` | U-20 + ecf7dc7 classifier fix (currently zero tests) |
| C3 | `build_503_body` redaction: redact=true hides upstream text, keeps `name (class)` + `all_backends_failed` | unit, `src/http.rs` | S-6 (network-exposed binds) |
| C4 | strict_models both modes: unknown model → default chain (off), → 404 `model_not_found` (on) + remedy text from B6 | `tests/routes.rs` | U-3, UX invariant 1 |
| C5 | `/v1/backends` shape + transitions: unknown → ok/cooling after a fallthrough request; field names incl. `cooling_secs` (B5) and `busy` state (B4) | `tests/routes.rs` | U-13 + P-9 wiring |
| C6 | CORS edges: ACAO+Vary on 404/405 error responses; ACAO on the SSE response; preflight succeeds with auth enabled and no token; two configured origins; `"*"` is not a wildcard | `tests/routes.rs` | U-10 beyond the happy path |
| C7 | 413 body cap: 16 MiB + 1 → 413 `payload_too_large`; just-under parses | `tests/routes.rs` | boundary + `take(+1)` logic |
| C8 | prompt_mode smells: invalid value lint message; prompt_mode on an api backend = unknown key; serve-time fallback to Transcript | `src/config.rs` + `src/backend/mod.rs` units | U-14 invariant-2 halves |
| C9 | TLS success path: committed self-signed PEM fixture (`tests/fixtures/test-ca.pem`, cert only, generated offline) → `tls_config_for(Some(path))` Ok twice (parse + cache hit) | unit, `src/http_client.rs` | U-24 happy path (currently failure-only) |
| C10 | `resolve_program` keep-scanning: PATH = dirA(0644 shadow):dirB(0755) resolves to dirB | unit, `src/backend/mod.rs` | the actual F-4 fix line (current test only covers the helper) |
| C11 | `is_executable_file` follows symlinks (Homebrew-style PATHs) | extend existing unit | metadata-vs-symlink_metadata regression |
| C12 | F-3 panic survival: extract the worker recv/catch_unwind body into a testable fn; a panicking handler doesn't kill the worker | unit, `src/http.rs` | spec §6 F-3's explicitly required test |
| C13 | RAII slot guard releases on unwind (from A2) | unit, `src/http.rs` | A2 |
| C14 | S-1 reuse-path perms repair (from A1) | unit, `src/auth.rs` | A1 |

---

## Phase D — Documentation

### D1. Example config reference block — **major**
Spec invariant 4 prescribed "new knobs go in a commented reference block" in the example config; it never materialized. ~19 new keys ([server] auth, auth_token_env, env_pass, max_response_bytes, workers, chain_budget_secs, cooldown, cooldown_secs, strict_models, cors_origins, env_capture_timeout_secs; per-backend prompt_mode, ca_file, stdin_prompt, max_concurrent, allow_empty, pty_size…; the whole [routers.*] section) are documented nowhere a user looks.
**How:** append `# ── Reference (all optional keys, defaults shown) ──` to `examples/server.toml`, fully commented, happy path untouched (invariant 4 preserved). Include the S-4 ps-visibility caveat where argv is retained.
**Guard:** a test that loads `examples/server.toml` through `config::parse` + `lint()` and asserts zero warnings — catches key-name drift between the reference block and the `*_KEYS` arrays forever.

### D2. README refresh — **major**
`HTTP surface (V1)` table: add `GET /v1/backends`; "everything else returns 404" is now false (405 + `Allow` on known paths, HEAD mirrors GET); bearer auth unmentioned; streaming section omits the immediate-headers + 15s `: keepalive` behavior; spec U-16's resolution ("document the usage zeros in the README") was never done. Update all five.

### D3. AGENTS.md corrections — **minor**
Line 108 "auth is not checked in V1" is now false; line 117 `detail` → `error.details`; line 95 /health shape now includes `"version"`. Optionally add a 401 troubleshooting row pointing at `~/.tmuxlet/token`.

### D4. STATUS.md TL;DR precision — **minor**
"Phases 1–4 is complete" contradicts the spec's own Phase-4 contents (which include the deferred V2 items) and STATUS's own deferred list. Reword to "Phases 1–3 plus Phase 4's opt-in items (U-10/U-14/U-17/U-19/U-24) complete; Phase 4's V2 items (streaming, P-7, U-22, P-4) deferred." Also record the B4/B5 spec amendments here, and soften the S-1 claim until A1 lands.

---

## Phase E — Optional polish (discretionary)

- **E1.** `Vary: Origin` on *all* responses when CORS is enabled (currently only on matched origins — cache-correctness quirk behind proxies, fail-closed); let the OPTIONS preflight reuse `apply_cors` for ACAO+Vary so origin-echo lives in one place.
- **E2.** Deduplicate `backend_type` (main.rs) / `backend_type_str` (http.rs) into `Backend::type_str()` in config.rs; deduplicate `is_executable` (main.rs) into `backend::is_executable_file` — both pairs are user-visible labels/predicates that can drift.
- **E3.** Type the max_response_bytes over-cap error as `io::ErrorKind::FileTooLarge` instead of substring-matching the English message across files (`http_client.rs:155` ↔ `api.rs:116`).
- **E4.** `cli.rs` non-PTY path runs `substitute_prompt` twice; reuse the first result.
- **E5.** Allocation micro-churn in handle_chat/run_chain (clone of model, Vec round-trips, per-leg health lock) — batch-fix only when touching that code.
- **E6.** Doc-comment the F-4 exec-bit deviation (mode-bits check accepts files a non-root server user can't execute; exact access(2) semantics not worth the plumbing).
- **E7.** `respond_head` sends `Content-Length: 0` while GET would return a body (RFC 9110 nicety); 401 lacks `WWW-Authenticate` (OpenAI-style APIs skip it too — fine to skip).

## Explicit no-action items

- **thread_local CORS design** — adversarially assessed and endorsed as-is: workers serve one request to completion, the value is unconditionally re-set at the top of `handle()`, and the explicit-parameter migration touches ~6 signatures for zero behavior change. Keep.
- **/v1/models grouping** (routers+chains merged in one sorted group) — the single refuted finding; the spec doesn't actually require routers listed first.
- **V2 deferrals** unchanged: true token streaming, P-7 disconnect+cancel, U-22 SIGHUP reload, P-4 connection reuse, P-9 Retry-After.

---

## Execution notes

- Phases are ordered by risk: A (behavior) → B (spec decisions, several one-liners) → C (tests, parallelizable) → D (docs, depends on B's amend-vs-implement outcomes). Keep CI green at each phase boundary, same as the implementation run.
- A2+C13, A1+C14, B4+B5+C5, B6+C4, B7/B8+their lint tests are natural single commits (fix + guard together).
- Phase C is the one place a worktree fan-out pays off (independent test files, no hub-file contention); A/B/D route through `http.rs`/`config.rs`/docs and should stay sequential.
- Estimated effort: A ≈ 5 small fixes + 4 tests; B ≈ 6 small implementations + 2 spec amendments; C ≈ 14 tests; D ≈ 4 doc files. No new dependencies anywhere.
