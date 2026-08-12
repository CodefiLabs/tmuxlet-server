# Phase 0 implementation plan — correctness

**Status:** ready to implement — O1 and O5 settled 2026-08-12, no open questions remain
**Date:** 2026-08-11 (decisions applied 2026-08-12)
**Spec:** [`docs/specs/2026-08-11-v2-routing-architecture.md`](../specs/2026-08-11-v2-routing-architecture.md) §4 (Phase 0 only)
**Branch this plan was written on:** `plan/phase-0-correctness` (docs only; nothing under `src/` touched)

Phase 0 fixes three defects shipped in v0.2.0. It ships alone: no decision
layer, no module split, no `route/` or `exec/` directories. Everything below
lands in `src/http.rs`, `src/backend/mod.rs`, `src/backend/api.rs`, and
`src/config.rs`.

---

## 0. Baseline

Recorded on `main` at `4fed5a7` before any edits, on a clean tree.

| Command | Result |
|---|---|
| `cargo build` | **clean** — `Finished dev profile in 20.71s`, zero warnings |
| `cargo test` | **129 passed, 0 failed, 0 ignored** across 11 binaries |
| `cargo clippy --all-targets` | **clean**, zero warnings |

Per-binary test counts (useful for spotting an accidental regression later):

| Binary | Tests |
|---|---|
| `unittests src/main.rs` | 82 |
| `tests/api_backend.rs` | 3 |
| `tests/auth.rs` | 4 |
| `tests/chain_fallback.rs` | 2 |
| `tests/cli_flags.rs` | 5 |
| `tests/cli_paths.rs` | 2 |
| `tests/concurrency.rs` | 5 |
| `tests/routes.rs` | 23 |
| `tests/server_starts.rs` | 1 |
| `tests/streaming.rs` | 1 |
| `tests/tmuxlet_backend.rs` | 1 |

**No pre-existing failures.** Nothing to work around, and any red after this
work is ours.

One baseline fact that constrains a design decision below: `.github/workflows/ci.yml:26`
runs `cargo clippy --all-targets -- -D warnings`. Dead code fails CI.

---

## 1. Verification

The spec's line numbers were written against an earlier tree. Current locations
and findings:

### 1.1 §4.1 — `parse_class` nondeterminism — **CONFIRMED**, spec line number exact

`src/http.rs:934-947`:

```rust
fn parse_class(reply: &str, routes: &HashMap<String, String>, fallback: &str) -> String {
    let r = reply.trim().to_ascii_lowercase();
    for k in routes.keys() {                      // pass 1: exact
        if k.to_ascii_lowercase() == r { return k.clone(); }
    }
    for k in routes.keys() {                      // pass 2: substring
        if r.contains(&k.to_ascii_lowercase()) { return k.clone(); }
    }
    fallback.to_string()
}
```

`routes` is `HashMap<String, String>` (`src/config.rs:119`) with the default
`RandomState`, reseeded per process. Confirmed as described.

One refinement the spec does not make, worth knowing while reading the diff:
**pass 1 is already deterministic** — map keys are unique, so at most one can
compare equal to `r`; iteration order cannot change the outcome. Only pass 2 is
order-dependent. The `keys.sort_unstable()` in the spec's replacement is
therefore belt-and-braces for pass 2 (and the spec says so in a comment); it is
not load-bearing for pass 1. Keep it anyway — it is what makes the "exactly one
match" count meaningful and costs nothing at classifier cadence.

The existing unit test `parse_class_matches_exact_then_contains_then_fallback`
(`src/http.rs:1069`) passes under both the old and new rules: its substring case
(`"I think this is execution work"` against classes `research`/`execution`)
matches exactly one class, so it stays green. It needs a mechanical update only
for the new tuple return type, not a semantic one.

### 1.2 §4.2 — classifier is shown the wrong text — **CONFIRMED**, spec line number exact

`src/http.rs:892-932` (`classify_task`) takes `last_user: &str` and does
`truncate_tail(last_user, router_cfg.classifier_max_chars)` at `src/http.rs:899`.
`truncate_tail` (`src/http.rs:949-955`) keeps the **last** N chars.

The caller is `src/http.rs:470-471`, inside the router branch of `handle_chat`:

```rust
let last_user = openai::last_user_text(&parsed.messages);
let (class, classifier_ms) = classify_task(state, router_cfg, &last_user);
```

`openai::last_user_text` (`src/openai.rs:93-101`) returns exactly the last
`role == "user"` message. So on turn 12 of an agent run the classifier sees
`"yes, continue"`. Confirmed.

Note the shadowing at `src/http.rs:500`: a *second* `let last_user =
openai::last_user_prompt(&parsed.messages)` binds later in the same function for
the U-14 chain prompt. These are different values in different scopes; the
Phase-0 change removes only the first.

### 1.3 §4.3 — overflow cools a healthy backend — **CONFIRMED**, spec line numbers exact

`record_failure` (`src/http.rs:822-845`) increments
`e.consecutive_failures` unconditionally for every `BackendError`, then sets
`e.cooling_until` whenever `state.cfg.server.cooldown` is on. `cooldown_for`
(`src/http.rs:848-863`) has no overflow arm; a `BackendError::Http(_, 400, _)`
falls into the catch-all `_ => Duration::from_secs(cooldown_secs)`. Confirmed.

**Two corrections / clarifications to the spec's framing:**

1. **"The chain advances immediately (no retry on the same leg)" is already
   true.** `run_chain` (`src/http.rs:704-811`) dispatches each leg exactly once
   and `continue`s on `Err`. There is no same-leg retry anywhere in the tree.
   This bullet requires **zero code**; do not go looking for a retry loop to
   suppress. What genuinely changes is only the health bookkeeping and the
   final status code.

2. **Detection is `api`-backend-only, and the spec never says so.** The narrow
   rule keys on an HTTP status, and only `Backend::Api` produces one
   (`BackendError::Http` is constructed at `src/backend/api.rs:124`). A
   `tmuxlet` or `cli` backend that overflows its context surfaces as
   `BackendError::Exit` or `::Backend` with no status, and will keep cooling
   exactly as today. That is an acceptable Phase-0 scope, but the doc comment
   on `is_context_overflow` should state it so nobody later assumes local
   backends are covered. See §6, risk R3.

---

## 2. Change list

### 2.1 §4.1 — deterministic `parse_class`

**`src/http.rs`**

- **New enum `DecisionSource`** — minimal, two variants only:

  ```rust
  /// Provenance of a routing decision. Phase 0 needs only the two classifier
  /// outcomes; §3.1 of the v2 spec widens this to nine variants and moves it to
  /// route/mod.rs in Phase 1.
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  enum DecisionSource {
      Classifier,
      FallOpen,
  }

  impl DecisionSource {
      fn as_str(self) -> &'static str {
          match self {
              DecisionSource::Classifier => "classifier",
              DecisionSource::FallOpen => "fall_open",
          }
      }
  }
  ```

  **Do not** pre-declare the full nine-variant §3.1 enum. CI runs
  `clippy --all-targets -- -D warnings`; seven unconstructed variants are a hard
  CI failure, and `#[allow(dead_code)]` to paper over it defeats the point.
  Widening it in Phase 1 is a one-line-per-variant edit.

- **Rewrite `parse_class`** (`src/http.rs:934`) to the spec's §4.1 body.
  Signature changes:
  `fn parse_class(reply, routes, fallback) -> (String, DecisionSource)`.

- **`classify_task`** (`src/http.rs:892`) return type widens to
  `(String, DecisionSource, u64)`. Its two early returns (classifier backend
  missing at `:911`; concurrency cap reached at `:919`) and its `Err(_)` arm
  (`:930`) all yield `DecisionSource::FallOpen` — they are precisely the "we
  tried to be smart and could not" case the spec distinguishes from `Default`.

- **`handle_chat`** (`src/http.rs:470`) destructures the third element and adds
  the source to the existing route log line at `src/http.rs:475`:

  ```rust
  log::info(&format!(
      "{reqid} route {model} class={class} target={target} source={} classifier_ms={classifier_ms}",
      source.as_str()
  ));
  ```

  This is the only user-visible surface for the new provenance in Phase 0 — the
  `x-tmuxlet-decision` header is Phase 1 (§5.1). Log-only is deliberate: it
  makes the fall-open rate greppable today without widening the HTTP contract
  ahead of the decision layer.

**Ripple:** none outside `http.rs`. `parse_class` and `classify_task` are both
private and each has exactly one call site.

### 2.2 §4.2 — `classifier_input`

**`src/http.rs`**

- **New `fn truncate_head(s: &str, max: usize) -> String`** — mirror image of the
  existing `truncate_tail` (`:949`), keeping the **first** `max` chars. Same
  char-based (not byte-based) slicing, so it cannot split a UTF-8 sequence.

- **New `fn classifier_input(messages: &[ChatMessage], max: usize, include_opening: bool) -> String`:**

  - `include_opening == false` → `truncate_tail(&last_user_text(messages), max)`.
    Byte-for-byte today's behavior; this is the opt-out escape hatch.
  - Fewer than two `role == "user"` messages → same as above (whole budget,
    tail-truncated). Tail, not head, so a single-message conversation is
    unchanged from v0.2.0.
  - Otherwise: opening = first `role == "user"` message, head-truncated to
    `head_budget`; latest = last `role == "user"` message, tail-truncated to
    `tail_budget`; joined by a fixed separator.

  Budget arithmetic, stated concretely because the spec only says "60/40":

  ```rust
  const SEP: &str = "\n\n[latest message]\n";
  let body = max.saturating_sub(SEP.chars().count());
  let head_budget = body * 60 / 100;
  let tail_budget = body - head_budget;
  ```

  Reserving the separator from the budget keeps the guarantee
  `classifier_input(..).chars().count() <= max` intact, which is what
  `classifier_max_chars` means to a user reading the example config. Note the
  separator also does real work: without a marker the classifier sees two
  concatenated asks with no cue about which is current.

  **Recommended addition (small, not in the spec):** when the opening message is
  shorter than `head_budget`, hand the unused remainder to the tail rather than
  discarding it. Two lines, strictly more signal, never exceeds `max`. Flagged
  in §7 as O2 in case a reviewer wants the fixed split instead.

- **`classify_task`** signature changes from `last_user: &str` to
  `messages: &[openai::ChatMessage]`, and `src/http.rs:899` becomes:

  ```rust
  let truncated = classifier_input(
      messages,
      router_cfg.classifier_max_chars,
      state.cfg.server.classifier_include_opening,
  );
  ```

  Note the flag comes off `state.cfg.server`, not `router_cfg` — it is
  server-wide (O5). `classify_task` already takes `state: &State`, so this needs
  no new parameter and no plumbing. See §3.

  Everything downstream (`{input}` substitution in `classifier_prompt`, the
  default prompt at `:903`) is unchanged — it already consumes a `String`.

- **`handle_chat`:** delete the now-unused
  `let last_user = openai::last_user_text(&parsed.messages);` at
  `src/http.rs:470` and pass `&parsed.messages` instead. The later
  `last_user_prompt` binding at `:500` stays untouched.

**Ripple:** `openai::last_user_text` keeps its only other caller
(`openai::last_user_prompt`, `src/openai.rs:133`) and stays `pub`. No signature
change in `src/openai.rs`.

### 2.3 §4.3 — `BackendError::Overflow`

**`src/backend/mod.rs`**

- **New variant:**

  ```rust
  /// Prompt exceeds this backend's context window. Not a health signal:
  /// the backend is fine, the prompt does not fit.
  Overflow(String, String),   // backend name, upstream detail snippet
  ```

  **Deviation from the spec, deliberate:** §4.3 writes `Overflow(String)`. With
  one field, either `backend_name()` cannot return a name or the upstream detail
  is thrown away — and that detail is what makes the new 400 body actionable
  ("maximum context length is 32768 tokens, requested 41002"). Two fields match
  the shape of the neighbouring `Http(name, status, detail)` and cost nothing.
  Flagged as O3 in §7 if a reviewer prefers the literal spec form.

- `backend_name()` (`:52`) gains `Overflow(n, _) => n` in the existing or-pattern.
- `class()` (`:65`) gains `Overflow(_, _) => "context overflow"`.
- `Display` (`:31`) gains
  `Overflow(n, d) => write!(f, "[{n}] context overflow: {d}")`, with the same
  empty-detail branch `Http` uses so a detail-less overflow renders cleanly.

- **New `pub fn is_context_overflow(status: u16, body: &str) -> bool`** — the
  spec's §4.3 body verbatim (400 only; the four substrings). Lives in
  `backend/mod.rs` rather than `api.rs` so it is testable independently of a
  socket and so a future `tmuxlet`-side detector can reuse it. Doc comment must
  state (a) why 413/422 are excluded and (b) that only `api` backends can reach
  it today (§1.3 finding 2).

**`src/backend/api.rs`**

- In `dispatch`, at the non-2xx branch (`src/backend/api.rs:123-129`), test
  before constructing the `Http` error:

  ```rust
  if !(200..300).contains(&status) {
      let detail = error_snippet(&resp);
      if super::is_context_overflow(status, &resp) {
          return Err(BackendError::Overflow(b.name.clone(), detail));
      }
      return Err(BackendError::Http(b.name.clone(), status, detail));
  }
  ```

  Match against the **full** `resp`, not `error_snippet(&resp)`:
  `error_snippet` caps at 300 chars (`src/backend/api.rs:81`), and an upstream
  that wraps its message in a verbose envelope could push the marker past the
  cap. The snippet is still what we *store*.

**`src/http.rs`**

- **`record_failure`** (`:822`) short-circuits at the top:

  ```rust
  if let BackendError::Overflow(..) = err {
      return record_overflow(state, name, err, latency_ms);
  }
  ```

- **New `fn record_overflow(state, name, err, latency_ms)`** — updates
  `last_error` and `last_latency_ms` so `/v1/backends` still shows what happened,
  and leaves `consecutive_failures` and `cooling_until` **untouched**. It must
  not reset `consecutive_failures` either: an overflow is not evidence of health
  any more than of ill-health, and zeroing the counter would let a chatty client
  launder a genuinely failing backend back into rotation. (Flagged as O4 — the
  spec is silent on `last_error`.)

- **`cooldown_for`** (`:848`) needs **no change**. It is only reached from
  `record_failure` after the short-circuit, so `Overflow` never gets there. Add
  no arm; adding an unreachable one invites a future reader to route through it.

- **New `fn chain_error_response(errors: &[BackendError], redact: bool) -> (u16, String)`:**

  ```rust
  fn all_overflow(errors: &[BackendError]) -> bool {
      !errors.is_empty() && errors.iter().all(|e| matches!(e, BackendError::Overflow(..)))
  }
  ```

  - `all_overflow` → `(400, build_overflow_body(errors, redact))`, an
    `ErrorEnvelope::with_details` with `error_type = "invalid_request_error"` and
    `code = Some("context_length_exceeded")`, following the redaction discipline
    `build_503_body` already uses (`src/http.rs:865-888`): full per-leg strings
    normally, `name (class)` only when `redact_errors` is set.
  - otherwise → `(503, build_503_body(errors, redact))`, unchanged.

- **Two call sites** replace their direct `build_503_body` calls:
  - `handle_chat`'s `Err(errors)` arm (`src/http.rs:565`) →
    `let (status, body) = chain_error_response(&errors, state.redact_errors);
    respond_json(req, status, body);`
  - `stream_with_keepalive`'s `Err(errors)` arm (`src/http.rs:672`) → same
    helper, but the transport status is already committed to 200 (headers were
    written before the chain ran). Emit the **body** from the helper and ignore
    its status code, so the in-band `data:` frame carries
    `code: "context_length_exceeded"` even though the SSE stream cannot carry a
    400. Add a one-line comment saying exactly that — it is the kind of asymmetry
    that reads as a bug six months later.

**Interaction with the existing skip paths.** `run_chain` pushes synthetic
`BackendError::Backend(name, "skipped: cooling Ns")` (`:750`) and
`"skipped: budget"` (`:743`) errors, and `BackendError::Busy` (`:769`), into the
same `errors` vec. Under the strict `all(...)` rule, a chain where one leg
overflowed and another was skipped as cooling returns 503, not 400. That is the
conservative reading of "when every leg overflows" and it is what the plan
implements. **Confirmed as the decision** — see O1 in §7.

---

## 3. Config surface

One new key: `classifier_include_opening`.

**Home: `[server]`, not `[routers.<name>]`** (O5, decided 2026-08-12). The key
exists to let an operator pin v0.2.0 classifier behavior once for a whole
server; a per-router home would mean editing five router tables in
`examples/server-router-framework.toml` to do it. It is the only classifier key
living outside `[routers.<name>]` — that asymmetry is deliberate and needs a
comment at the declaration, because the neighbouring `classifier_max_chars`
*is* per-router.

Three edits:

1. **`src/config.rs:22-70`** — add to `struct Server`, after `cors_origins`:

   ```rust
   /// §4.2: include the opening user task in the classifier input, not just
   /// the latest turn. Server-wide — unlike the per-router `classifier_*`
   /// keys — so one line pins v0.2.0 behavior for every router. Opt-out.
   #[serde(default = "default_true")]
   pub classifier_include_opening: bool,
   ```

   `default_true` already exists at `src/config.rs:87` **and is already used by
   `Server::cooldown` at `:58`**, so this is an established pattern in this exact
   struct. Reuse it; do not add a `default_classifier_include_opening`.

2. **`src/config.rs:424-441`** — add `"classifier_include_opening"` to
   `SERVER_KEYS` (**not** `ROUTER_KEYS`). Without this, the unknown-key lint at
   `src/config.rs:502` reports it as `server.classifier_include_opening` with a
   "did you mean" suggestion, and `--validate` fails on any config that sets it.
   This is the single easiest step to forget; it has its own test (T11).

3. **`validate()`** (`src/config.rs:255-283`) — **no change**. A `bool` has no
   invalid value and no cross-reference to check.

**Threading: none required — this is where server-wide is cheaper than
per-router.** `classify_task` already receives `state: &State`
(`src/http.rs:892-896`), and `state.cfg.server` is reachable from it — the same
access `handle_chat` already makes for `state.cfg.server.strict_models`
(`src/http.rs:487`). The per-router variant would have read the flag off
`router_cfg`; server-wide reads it off `state.cfg.server` at the same call site.
No signature change beyond the `last_user: &str` → `messages: &[ChatMessage]`
swap §2.2 already makes.

**Example configs.** One file needs an edit:

- **`examples/server.toml:4-12`** — document the key in the `[server]` block,
  where an operator looking to pin behavior will actually go:

  ```toml
  # classifier_include_opening = true  # §4.2: show the classifier the opening
  #                                    # task + the latest turn (60/40 split),
  #                                    # not just the latest turn. Server-wide;
  #                                    # false = v0.2.0 behavior.
  ```

  Separately, fix the now-misleading description of `classifier_max_chars` in
  the `[routers.smart]` comment block (`examples/server.toml:114`) — it is now a
  budget over the *combined* classifier input, not over the latest turn alone.

- **`examples/server-router-framework.toml`** — **no edit needed.** Its
  `[server]` block (`:39-47`) already defers to `examples/server.toml` for "the
  full reference block of optional keys", and its five routers inherit the `true`
  default, which is the intended behavior for exactly the agent traffic that file
  targets. Both example files are asserted parseable+valid by the `config.rs`
  tests, so a stray key there would fail CI immediately.

- **`README.md`** — no edit. It does not document individual classifier keys
  (only `README.md:86` mentions the auto-router at all).

- **`docs/STATUS.md`** — add a Phase-0 line to the changes list when the work
  lands, matching the existing `U-20`/`A4` entry style. Not blocking.

**Back-compat:** an existing `~/.tmuxlet/server.toml` parses unchanged and gets
`true`. Its classifier behavior *does* change — that is the point of the fix, and
why the spec makes it opt-out rather than silent.

---

## 4. Test list

17 tests, T1–T17. The spec estimates ~14; the three extra are the `SERVER_KEYS`
lint test (T11, cheap insurance against the easiest-to-forget edit), the
mixed-failure 503 pin (T16), and the streaming overflow-body test (T17, which
covers a path the spec's §4.3 does not mention).

Unit tests go in the existing `#[cfg(test)] mod tests` of the file under test.
Integration tests go in `tests/routes.rs`, which already has a working
single-class router harness at `tests/routes.rs:278-302` to copy.

### §4.1 — `parse_class` (unit, `src/http.rs`)

| # | Name | Asserts |
|---|---|---|
| T1 | `parse_class_exact_match_wins_over_substring` | Reply `"code"` with classes `{code, code-review}` returns `code` via `DecisionSource::Classifier` — the exact match short-circuits before the substring pass, where both would hit. |
| T2 | `parse_class_single_substring_match_is_classifier` | `"I think this is execution work"` over `{research, execution}` → `("execution", Classifier)`. This is the updated form of today's `parse_class_matches_exact_then_contains_then_fallback` (`src/http.rs:1069`) — retype for the tuple, keep the assertions. |
| T3 | **`parse_class_ambiguous_reply_falls_open_across_100_maps`** | *(named in the spec)* `"this is a code task, not a write task"` over `{code, write}` returns `(fallback, FallOpen)` — built and asserted inside a `for _ in 0..100` loop over freshly constructed `HashMap`s, so each gets a distinct `RandomState`. Loop, not one map: reseeding is per-`HashMap`-construction, and a single map is a coin flip that passes ~50% of the time against the old code. This test **must fail on `main`**; run it against the unpatched `parse_class` once to prove it. |
| T4 | `parse_class_no_match_falls_open` | `"nonsense"` → `(fallback, FallOpen)`, and the source is `FallOpen`, not `Classifier` — the distinction §3.1 insists on. |
| T5 | `parse_class_is_case_and_whitespace_insensitive` | `"  RESEARCH \n"` → `("research", Classifier)`; guards the `trim().to_ascii_lowercase()` that the rewrite must preserve. |

### §4.2 — `classifier_input` (unit, `src/http.rs`)

| # | Name | Asserts |
|---|---|---|
| T6 | `classifier_input_combines_opening_and_latest` | 12-message conversation opening `"Refactor the auth module to use JWTs"` and ending `"yes, continue"`. Result contains **both** substrings. This is the defect, stated as an assertion. |
| T7 | `classifier_input_respects_the_budget_and_the_60_40_split` | With a tiny `max` and both messages far over budget: total char count `<= max`; the opening contributes its **first** `head_budget` chars (head-truncated) and the latest its **last** `tail_budget` chars (tail-truncated). Asserts on the actual boundary chars, not just lengths — a head/tail swap is exactly the bug this fix exists to prevent. |
| T8 | `classifier_input_single_user_message_gets_the_whole_budget` | One user message longer than `max` → identical to `truncate_tail(msg, max)`; no separator appears. v0.2.0 behavior preserved for the single-turn case. |
| T9 | `classifier_include_opening_false_reproduces_v0_2_0` | Same 12-message fixture as T6 with `include_opening = false` → exactly `truncate_tail(last_user_text(..), max)`. The opt-out is byte-for-byte, which is the promise the key makes. |
| T10 | `classifier_input_is_char_safe_on_multibyte_input` | A budget-exceeding string of 4-byte emoji through both the head and tail paths does not panic and yields valid UTF-8. `truncate_head` is new code doing index arithmetic; this is where a byte-slice mistake would land. |

### §4.2 — config (unit, `src/config.rs`)

| # | Name | Asserts |
|---|---|---|
| T11 | `classifier_include_opening_defaults_true_and_lints_clean` | Three cases. (a) A `[server]` block omitting the key parses with `classifier_include_opening == true`. (b) A `[server]` block *setting* it to `false` parses to `false` **and** produces no `lint()` output — the `SERVER_KEYS` regression guard (§3 edit 2). (c) The key under `[routers.x]` **does** lint, as an unknown router key. Case (c) pins the server-wide home (O5) against a future well-meaning move back to per-router, which would otherwise silently ignore the operator's `[server]` setting. |

### §4.3 — overflow (unit)

| # | Name | Asserts |
|---|---|---|
| T12 | **`is_context_overflow_requires_400_plus_a_body_match`** | *(named in the spec)* Table-driven, in `src/backend/mod.rs`. Hits: 400 × each of the four substrings, plus a mixed-case body (the rule lowercases). Misses: **413** and **422** with an identical matching body; 400 with an unrelated body (`"invalid api key"`); 500 with a matching body; 200 with a matching body. The 413/422 rows are the point — they encode a deliberate decision that a future "helpful" widening would silently undo. |
| T13 | `overflow_error_classifies_and_names_its_backend` | In `src/backend/mod.rs`: `Overflow("agy", "…").class() == "context overflow"`, `.backend_name() == "agy"`, and `Display` contains both the name and the detail. Mirrors the existing `http_error_includes_body_detail` (`src/backend/mod.rs:372`). |
| T14 | `overflow_does_not_cool_or_count_against_health` | In `src/http.rs`: build a `State` with `cooldown = true`, call `record_failure` with an `Overflow`, and assert the health entry has `consecutive_failures == 0` and `cooling_until == None` — then call it with a `Timeout` on the same backend and assert both **do** move, proving the test would catch a short-circuit that swallowed every error class. |

### §4.3 — overflow end-to-end (integration, `tests/routes.rs`)

| # | Name | Asserts |
|---|---|---|
| T15 | **`all_legs_overflow_returns_400_context_length_exceeded`** | Two `api` backends in one chain, both pointed at mock upstreams returning `400 {"error":{"message":"This model's maximum context length is 8192 tokens"}}`. Copy the `mock_upstream` helper from `tests/api_backend.rs:13-52` (it answers one request, so give each leg its own listener). Assert: HTTP status **400**, `error.code == "context_length_exceeded"`, `error.type == "invalid_request_error"` — and that `GET /v1/backends` afterwards reports both legs `state: "ok"` with `consecutive_failures: 0`, which is the health half of the fix observed from outside. |
| T16 | `a_mixed_overflow_and_real_failure_stays_503` | Same shape, but leg 2's mock returns `500`. Status is **503** with `code == "all_backends_failed"`. Pins the `all_overflow` rule so a later refactor to "any overflow → 400" has to break a test to happen. |
| T17 | `streaming_all_legs_overflow_carries_the_overflow_code_in_band` | Same config as T15 with `"stream": true`. The SSE transport status is 200 (headers already committed), and the in-band `data:` frame's body carries `code: "context_length_exceeded"`. Covers the §2.3 asymmetry the spec does not discuss. |

T3, T12, and T15 are the three the spec calls out by name or implication — if
review time is short, those are the ones to read closely.

---

## 5. Ordering

Three independently landable units. They touch disjoint code and can be
separate commits in one PR, or three PRs, in any order.

**Recommended sequence — cheapest and most self-contained first:**

1. **§4.1 `parse_class` + `DecisionSource`** *(~35 LOC, 5 tests)*
   Pure function, one file, no config, no new types crossing a module boundary.
   Lands the `DecisionSource` enum that §4.2 then consumes.
   *Landable alone.*

2. **§4.2 `classifier_input` + config key** *(~60 LOC, 6 tests)*
   Depends on step 1 only for `classify_task`'s already-widened return tuple. If
   landed first instead, that tuple change moves into this step — a two-line
   difference, so the order is a preference, not a constraint.
   *Landable alone.*

3. **§4.3 overflow** *(~75 LOC, 6 tests)*
   Fully independent of 1 and 2 — different files, no shared symbols.
   Internal order matters more than its position in the list:
   **3a** `BackendError::Overflow` + `is_context_overflow` + `api.rs` detection
   (compiles and is fully unit-testable on its own; nothing yet consumes the
   variant, and until 3b lands an `Overflow` behaves exactly like today's
   `Http(400)` — a safe intermediate state) →
   **3b** `record_failure` short-circuit + `record_overflow` →
   **3c** `chain_error_response` + the two call sites + the integration tests.
   *Landable alone; 3a/3b/3c should not be split across PRs — 3a without 3b
   changes nothing, and shipping it alone burns a review cycle for no behavior.*

**Do not** bundle any of this with the Phase 1 refactor. §11 of the spec is
explicit, and it is right: mixing bug fixes into a 700-line move is how a
refactor becomes unreviewable.

**Definition of done:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
and `cargo test` all green, with the test count at **146** (129 baseline + 17).

---

## 6. Size check

| Unit | Est. LOC (non-test) |
|---|---|
| §4.1 `parse_class` rewrite + `DecisionSource` + log line | ~35 |
| §4.2 `classifier_input` + `truncate_head` + `classify_task` signature + call site | ~55 |
| §4.2 config key + `SERVER_KEYS` + example comment | ~8 |
| §4.3 `Overflow` variant + `class`/`Display`/`backend_name` + `is_context_overflow` | ~30 |
| §4.3 `api.rs` detection hookup | ~6 |
| §4.3 `record_overflow` + `record_failure` short-circuit | ~18 |
| §4.3 `chain_error_response` + `all_overflow` + overflow body + 2 call sites | ~35 |
| **Total** | **~187** |

Against the spec's ~180 LOC and ~14 tests: **the estimate holds.** No hidden
scope surfaced during verification. The two places it could have blown up did
not — the "no retry on the same leg" bullet turned out to be a no-op (§1.3), and
the config key needed no `validate()` logic.

---

## 7. Risks and open questions

Ordered by how much a wrong answer costs. **O1 and O5 were open when this plan
was first written; both were decided on 2026-08-12 and are recorded below as
settled.** The rest are judgment calls this plan has already made and merely
records.

### Decisions settled

**O1 — What exactly is "every leg overflows"? → STRICT.** *(decided 2026-08-12)*
`run_chain` mixes real dispatch errors with synthetic skip markers
(`"skipped: cooling Ns"`, `"skipped: budget"`) and `Busy` in one `errors` vec.
Under the strict `all(...)` rule this plan implements, a chain where leg 1
overflowed and leg 2 was skipped as cooling returns **503**, not 400 — even
though nothing that actually ran could serve the prompt. The looser rule (at
least one `Overflow`, no non-overflow *dispatch* failure) returns 400 there.
**Strict wins:** a wrong 400 tells a client "your request is unservable, do not
retry" when a retry in 60s would in fact work. T16 pins the rule, so a later
drift to "any overflow → 400" has to break a test to happen. Revisit only if the
mixed case shows up in practice.

**O5 — Is `classifier_include_opening` per-router or server-wide? → SERVER-WIDE.**
*(decided 2026-08-12)* A tmuxlet server's chain is configured once and is meant
to apply to every session; the flag governing what the classifier is shown
should follow the same shape. One line in `[server]` pins v0.2.0 behavior
globally, rather than five edits across the router tables in
`examples/server-router-framework.toml`. §2.2 and §3 are written to this
decision.

Consequences accepted:

- It is the only classifier key living outside `[routers.<name>]`. Comment the
  asymmetry at the declaration — the neighbouring `classifier_max_chars` *is*
  per-router, and the next person to read the struct will assume this one is too.
- Per-router variation is no longer expressible. If it is ever wanted, the
  `[server]`-default-plus-router-override variant (~10 LOC) remains **strictly
  additive**: a router key that defaults to "inherit the server value" cannot
  break a config written against server-wide. So this is no longer the
  hard-to-reverse decision the original draft flagged it as.
### Judgment calls already made (recorded, not blocking)

**O2 — Fixed 60/40, or redistribute the unused head budget?**
The spec says "budget split 60/40" and stops. This plan redistributes: a short
opening hands its remainder to the latest turn. Strictly more signal, never
exceeds `max`, ~2 LOC. The argument against is only that it makes the split
non-obvious from the config value. *Recommendation: redistribute.*

**O3 — `Overflow(String)` or `Overflow(String, String)`?**
The spec writes one field; this plan uses two (name, detail) so `backend_name()`
works and the 400 body can carry the upstream's actual message. See §2.3.

**O4 — Should an overflow update `last_error` on the health entry?**
The spec is silent. This plan writes `last_error` and `last_latency_ms` (so
`/v1/backends` explains why a leg was skipped) but leaves `consecutive_failures`
and `cooling_until` alone. The alternative — touch nothing — makes an overflow
invisible in `/v1/backends`, which seems worse for debugging. Note the
deliberate asymmetry: overflow neither increments **nor resets**
`consecutive_failures`, so a chatty client cannot launder a genuinely failing
backend back into rotation.

**O6 — `DecisionSource` is log-only in Phase 0.**
No header, no `/v1/stats`. Phase 1 §5.1 adds `x-tmuxlet-decision`. Anyone
wanting the fall-open rate before then greps `source=fall_open` in the logs.
Called out because "we fixed the ambiguity" and "you can see the ambiguity" are
different deliverables, and Phase 0 only ships the first.

### Risks

**R1 — T3 is the only test that can catch a nondeterminism regression, and it is
probabilistic against the *old* code.** 100 iterations reduces a false pass to
~2⁻¹⁰⁰ for a two-way ambiguity, which is fine. But run it against unpatched
`parse_class` once during implementation to confirm it actually goes red —
a test for nondeterminism that never observed the nondeterminism is not evidence
of anything.

**R2 — §4.2 changes routing behavior for every existing router config on
upgrade.** That is the fix working as designed, and the opt-out key is the
mitigation the spec chose. Worth one line in `docs/STATUS.md` and the release
notes so an operator seeing different classes after upgrading knows why. It is
not a silent change, but it is only non-silent if someone writes it down.

**R3 — Overflow detection covers `api` backends only.** `tmuxlet` and `cli`
backends have no HTTP status; an overflowing local model still cools exactly as
today. The spec never states this limitation, and it inverts the motivating
example in §4.3 ("a 32k local model in a chain with a 200k cloud model") —
**if that local model is a `cli` or `tmuxlet` backend rather than an `api`
backend pointed at a local server, Phase 0 does not fix its case.** The most
common shape (Ollama/llama.cpp via `type = "api"` on `127.0.0.1`) *is* covered,
so the fix still lands where it matters most. Document the boundary on
`is_context_overflow`; do not widen it in Phase 0 — a text-pattern-only detector
with no status to gate on is exactly the false-positive risk the narrow 400 rule
exists to avoid.

**R4 — Four substrings will not match every upstream.** Anthropic-shaped errors
(`"prompt is too long"`) and OpenAI-shaped ones (`"maximum context length"`,
`"context_length_exceeded"`) are covered; a proxy that rewrites the message into
its own vocabulary is not, and falls back to today's behavior (cools the
backend). The failure mode is a silent non-improvement, not a regression, which
is the right way for this to fail. Adding patterns later is a one-line change
with an obvious test row.

**R5 — Streaming cannot return 400.** Headers are committed before the chain
runs (`src/http.rs:685-700`), so a streaming request whose legs all overflow gets
a 200 SSE stream with an overflow-coded body in-band. Unavoidable without
buffering the first leg, which is Phase 5 territory (§9.2's commit-point
design). T17 pins the current behavior; the comment in `stream_with_keepalive`
should say why rather than leaving it to be rediscovered.
