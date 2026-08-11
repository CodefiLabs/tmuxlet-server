# V2 Architecture Proposal: from string resolution to a decision layer

**Status:** proposal
**Date:** 2026-08-11
**Author:** architecture review
**Supersedes:** nothing. Extends [`2026-07-09-optimization-spec.md`](./2026-07-09-optimization-spec.md) (V1 + §5 auto-router), which is fully landed as of v0.2.0.
**Prior art:** [NVIDIA-NeMo/Switchyard](https://github.com/NVIDIA-NeMo/Switchyard) (Apache-2.0), read at `main` on 2026-08-11.

---

## 0. Thesis

tmuxlet-server routes by **resolving a string once, ahead of dispatch**.
`router::resolve(model, cfg) -> Vec<&str>` turns a model name into an ordered
list of backend names, and `run_chain` walks it. The §5 auto-router bolted a
classifier onto the front of that resolution, but the shape did not change: one
decision, made blind, before any work happens.

Switchyard routes by **running a decision pipeline over typed request state**.
Its algorithms read the conversation — tool results, error severity, production
intensity, session history — and produce a decision with a recorded provenance.

The gap is not features. It is that tmuxlet-server has no place to *put* a
routing decision. There is no `Decision` type, no notion of a session, no
acceptance criterion beyond "output is non-empty", and no way for the chain to
learn anything from the leg it just ran.

**This proposal adds that layer** — a synchronous decision pipeline that produces
chains rather than replacing them — and then fills it with three capabilities
ported from Switchyard — tool-signal scoring, session affinity, and output-gated
escalation — plus one that has no Switchyard analog: a **subscription-aware cost
model** (§7.3), because our premium tier is flat-rate and theirs is metered.

The chain stays. The chain is tmuxlet-server's genuine advantage: an ordered
fallback across *heterogeneous, process-backed* backends, which Switchyard
structurally cannot express (verified: no `process::Command`, no `tokio::process`,
no PTY anywhere in its tree; every `[targets.X]` requires an `llm_client` with a
`base_url`). We are not converging on Switchyard. We are taking its routing
intelligence and mounting it on an execution engine it does not have.

### The hard constraint

**Zero new dependencies.** Every proposal below is implementable with the current
9 direct deps and `std`. This is not a stretch goal, it is a design input:

| Capability | Implemented with |
|---|---|
| Tool-signal extraction | `serde_json` (already in tree) |
| Scorer | `f64::tanh` (`std`) |
| Session store | `std::collections::HashMap` + `Mutex` |
| Session key hashing | `std::hash::DefaultHasher` (process-local cache key; not a security boundary) |
| Prometheus exposition | hand-rolled text format 0.0.4 (~80 LOC) |
| Anthropic envelope | `serde` derive |
| Real `api` streaming | existing `http_client` + `tiny_http` chunked writer |

If a phase cannot hold this line, the phase is wrong.

---

## 1. What we are not doing

Explicit non-goals, so scope creep has something to bounce off:

1. **No async runtime.** No tokio, no axum. The thread-per-request model with
   `workers = max(16, cores)` is correct for a workload whose legs run for
   minutes. Async buys nothing when concurrency is bounded by subprocess count.
2. **No tool-call round-tripping.** Translating OpenAI `tool_calls` ↔ Anthropic
   `tool_use` is why Switchyard's translation crate is 11.5k lines. Our backends
   are text-in/text-out. Phase 5 ships a text-only Anthropic surface and says so
   in the README.
3. **No streaming for `tmuxlet`/`cli` backends.** They are structurally blocking
   — the child emits at exit, and a PTY's intermediate output is TUI redraw, not
   tokens. Real streaming is `api`-only, forever.
4. **Not becoming a general LLM gateway.** We do not need `/v1/responses`,
   protocol translation between three formats, or OpenTelemetry.
5. **No config breakage.** Every existing `~/.tmuxlet/server.toml` must keep
   working unchanged through all six phases. See §9.

---

## 2. Target module layout

`src/http.rs` is 1172 lines doing four unrelated jobs: HTTP transport, routing,
chain execution, and classification. Every phase below is blocked on splitting
it. The split is the enabling refactor, and it is mostly mechanical.

```
src/
  main.rs              # CLI, startup, signal handling         (unchanged)
  config.rs            # + schema_version, + new route types
  env.rs log.rs auth.rs pty.rs http_client.rs                  (unchanged)

  proto/               # ← was openai.rs
    mod.rs             #   PromptMode, shared shaping
    openai.rs          #   ChatRequest/ChatCompletion/chunks   (moved verbatim)
    anthropic.rs       #   Phase 5 only

  route/               # ← was router.rs (35 LOC) + the routing half of http.rs
    mod.rs             #   Decision, DecisionSource, RequestCtx
    resolve.rs         #   the current router::resolve         (moved verbatim)
    policy.rs          #   Policy trait + Static/Classifier/Signal impls
    signals.rs         #   ToolSignals extraction              (Phase 3)
    scorer.rs          #   dimensions + tanh scorer            (Phase 3)
    session.rs         #   SessionStore                        (Phase 2)

  exec/                # ← the chain half of http.rs
    mod.rs
    chain.rs           #   run_chain + cooldown + slots        (moved)
    gate.rs            #   Gate trait: NonEmpty, Judge         (Phase 4)

  http.rs              # transport only: parse, dispatch to route+exec, respond
  stats.rs             # counters, /v1/stats, /metrics          (Phase 1)
  backend/{mod,tmuxlet,api,cli}.rs                             (+ Overflow error)
```

Nothing in `backend/` changes shape. That boundary is already correct.

---

## 3. The decision layer

### 3.1 Types

```rust
// route/mod.rs

/// Everything a policy is allowed to look at. Borrowed — policies never own
/// request state, so this costs nothing to construct per request.
pub struct RequestCtx<'a> {
    pub model: &'a str,
    pub messages: &'a [proto::openai::ChatMessage],
    pub raw: &'a serde_json::Value,
    pub session: Option<SessionKey>,
    pub cfg: &'a Config,
}

/// The outcome of routing. Produced before dispatch, attached to the response.
pub struct Decision {
    /// Ordered backend names — the chain to walk. Never empty.
    pub legs: Vec<String>,
    /// Why these legs. Drives the rationale header and the stats label.
    pub source: DecisionSource,
    /// Human-readable, bounded to 200 chars. Goes in x-tmuxlet-rationale.
    pub rationale: String,
    /// class/target for routers; None for a direct chain or backend.
    pub label: Option<String>,
}

/// Provenance. Deliberately mirrors Switchyard's taxonomy so the two are
/// comparable when both are deployed — with `Static`/`Default` added, because
/// our model string can name a chain directly and theirs cannot.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    Static,      // model named a chain or backend
    Default,     // unknown model → default_chain
    Affinity,    // reused this session's earlier decision
    Quota,       // leg 0 exhausted (429 / Retry-After) — skipped, not cooled
    Override,    // critical error or compaction forced the flat-rate leg
    Settled,     // tests passed + recent production → shed a leg deeper
    Dimensions,  // scorer crossed confidence_threshold while rationing
    Classifier,  // LLM judge returned a usable class
    FallOpen,    // classifier failed/ambiguous/at-cap → fallback_class
}
```

`FallOpen` is not a synonym for `Default`. `Default` means the client asked for
something we do not know; `FallOpen` means we tried to be smart and could not.
Conflating them is exactly what makes the current server hard to debug — today
both paths land in the same silent `fallback_class`.

### 3.2 The policy trait

```rust
// route/policy.rs

pub trait Policy: Send + Sync {
    fn decide(&self, ctx: &RequestCtx, state: &State) -> Decision;
}
```

Synchronous, `&self`, no allocation in the hot path beyond the returned
`Decision`. Four implementations:

| Impl | Replaces / adds | Phase |
|---|---|---|
| `StaticPolicy` | today's `router::resolve` branch of `handle_chat` | 1 |
| `ClassifierPolicy` | today's `classify_task` + `parse_class` | 1 |
| `SignalPolicy` | new — tool-signal scoring | 3 |
| `LayeredPolicy` | new — affinity → override → signals → classifier → fall open | 3 |

Policy selection happens once at startup, in `State::build`, not per request.
A `[routers.x]` table with `classifier` and no `signals` builds a
`ClassifierPolicy`; with both, a `LayeredPolicy`.

### 3.3 What this buys immediately

`handle_chat` (`src/http.rs:430-570`) collapses from a 60-line inline routing
block with two early-return error paths into:

```rust
let decision = state.policy_for(&model).decide(&ctx, state);
let outcome  = exec::chain::run(state, &decision, &shaped, &reqid);
```

and every response — router-driven or not — carries provenance. Today
`x-tmuxlet-route` is only emitted when `route_label` is `Some`, i.e. only on the
router path (`src/http.rs:552`); a plain chain request gets no header at all.

---

## 4. Phase 0 — correctness (ships alone, no refactor)

Three defects the Switchyard comparison surfaced. All are in shipped v0.2.0 code
and none need the decision layer.

### 4.1 `parse_class` is nondeterministic under ambiguity

`src/http.rs:934`. Two passes over `routes.keys()` — a `HashMap`, so arbitrary
iteration order — an exact match then a substring `contains`. A classifier reply
of *"this is a code task, not a write task"* matches both `code` and `write` on
the second pass, and **which one wins varies between runs of the same binary**
(`RandomState` reseeds per process).

Fix:

```rust
fn parse_class(reply: &str, routes: &HashMap<String, String>, fallback: &str)
    -> (String, DecisionSource)
{
    let r = reply.trim().to_ascii_lowercase();

    // Pass 1: exact match. Unambiguous by construction (keys are unique).
    if let Some(k) = routes.keys().find(|k| k.to_ascii_lowercase() == r) {
        return (k.clone(), DecisionSource::Classifier);
    }

    // Pass 2: substring — only when EXACTLY ONE class matches. Iterate a sorted
    // key list so a future change here can't reintroduce order-dependence.
    let mut keys: Vec<&String> = routes.keys().collect();
    keys.sort_unstable();
    let hits: Vec<&&String> =
        keys.iter().filter(|k| r.contains(&k.to_ascii_lowercase())).collect();
    match hits.as_slice() {
        [only] => ((**only).clone(), DecisionSource::Classifier),
        _ => (fallback.to_string(), DecisionSource::FallOpen),  // 0 or 2+
    }
}
```

Ambiguity now falls open *and says so*, instead of silently picking a coin-flip
winner. Test: a reply containing two class names must return the fallback, and
must do so across 100 fresh `HashMap`s.

### 4.2 The classifier is shown the wrong text

`classify_task` (`src/http.rs:892`) receives `last_user`, then `truncate_tail`
keeps the **last** `classifier_max_chars`. On turn 12 of an agent run,
`last_user` is *"yes, continue"* — zero routing signal — and we silently route to
`fallback_class` while logging a confident `class=general`.

Switchyard's classifier is deliberately shown *the opening user task plus the
latest user message*, and documents this exact failure: "If a client sends only a
follow-up fragment without the opening task, enable affinity or include the task
history. Threshold tuning cannot recover missing task context."

Fix — build the classifier input from both ends of the conversation:

```rust
/// Opening task (head-truncated: intent is at the start) + latest user turn
/// (tail-truncated: the ask is at the end). Budget split 60/40. When there is
/// only one user message, it gets the whole budget.
fn classifier_input(messages: &[ChatMessage], max: usize) -> String
```

Add `classifier_include_opening = true` (default `true`) so anyone depending on
current behavior can pin it. This is a routing-quality change, so it is opt-out
rather than silent.

### 4.3 Context overflow cools a healthy backend

`record_failure`/`cooldown_for` (`src/http.rs:822`, `:848`) treat every failure
as evidence of backend ill-health. A context overflow is not: the backend is
fine, the *prompt* does not fit. Today a 32k local model in a chain with a 200k
cloud model gets benched for `cooldown_secs` by a long conversation, and stays
benched for unrelated short requests.

Add a variant and a class:

```rust
// backend/mod.rs
pub enum BackendError {
    // ...
    /// Prompt exceeds this backend's context window. Not a health signal.
    Overflow(String),
}

// class() → "context overflow"
```

Detection, matching Switchyard's deliberately narrow rule — HTTP **400** *and* a
body that identifies a context error:

```rust
fn is_context_overflow(status: u16, body: &str) -> bool {
    if status != 400 { return false; }
    let b = body.to_ascii_lowercase();
    b.contains("context_length_exceeded")
        || b.contains("maximum context length")
        || b.contains("prompt is too long")
        || b.contains("context window")
}
```

413 and 422 deliberately do **not** count — they are too ambiguous across
upstreams, and a false positive silently skips a healthy leg. Behavior:

- `Overflow` **never** increments `consecutive_failures` and never sets
  `cooling_until`.
- The chain advances immediately (no retry on the same leg).
- With a session (Phase 2), the backend is excluded for the rest of that session.
- When every leg overflows, return **400** with code `context_length_exceeded`,
  not the generic 503 — the client sent a request nothing can serve, and a 503
  invites a pointless retry.

**Estimate:** ~180 LOC + ~14 tests. No config changes except the one opt-out key.

---

## 5. Phase 1 — decision layer + observability

The §3 refactor, plus the provenance it makes possible. Behavior-preserving
except for new headers and one new endpoint.

### 5.1 Response headers

| Header | Example | Emitted |
|---|---|---|
| `x-tmuxlet-route` | `do/code/do-code/agy` | always (widened from router-only) |
| `x-tmuxlet-backend` | `agy` | always |
| `x-tmuxlet-decision` | `dimensions` | always |
| `x-tmuxlet-rationale` | `spinning + severity 0.7 → capable (confidence 0.76)` | always |

Rationale is bounded to 200 chars, control-stripped, and **suppressed when
`redact_errors` is set** (S-6, bound non-loopback) — it can otherwise leak
backend names and error text to a remote client. This is a real disclosure
surface, not a formality.

### 5.2 `/v1/stats`

`/v1/backends` already reports per-backend health. `/v1/stats` adds traffic:

```json
{
  "uptime_secs": 8412,
  "requests": { "total": 1842, "ok": 1795, "failed": 47 },
  "decisions": { "static": 900, "dimensions": 512, "classifier": 300,
                 "fall_open": 88, "affinity": 42, "quota": 31 },
  "backends": {
    "claude-max": {
      "requests": 612, "errors": 3, "overflow": 1,
      "latency_ms": { "p50": 8200, "p95": 41000, "max": 92000 },
      "cost": "subscription",
      "quota": { "rate_limited": 31, "calls_in_window": 148,
                 "limit_hint": 200, "pressure": 0.74,
                 "exhausted_secs_total": 1840 }
    }
  },
  "classifier": { "calls": 388, "fail_open": 88, "mean_ms": 240 }
}
```

Latency percentiles from a fixed 512-sample reservoir per backend — bounded
memory, no histogram crate. `POST /v1/stats/reset` for benchmarking runs.

The `quota` block ships in **Phase 1**, not Phase 3, even though nothing consumes
it until Phase 3. It is pure observation — counting 429s and time-spent-exhausted
costs nothing and changes no behavior — and per §13 it is the measurement that
decides whether Phase 3 gets built at all. `limit_hint` and `pressure` read
`null` until a limit is configured or learned.

### 5.3 `/metrics` (optional, `[server] metrics = true`)

Prometheus text format 0.0.4 is ~15 lines of grammar. Hand-emit it from the same
counters. No `prometheus` crate, no registry, no dependency. Bounded label sets
only: `backend`, `decision`, `outcome` ∈ {success, retryable_error, other_error}.
Switchyard's cardinality discipline (§"Cardinality" in their metrics reference) is
worth copying wholesale — no per-request or per-user value ever becomes a label.

**Estimate:** ~700 LOC moved, ~350 new, ~25 tests. Highest-risk phase because it
touches everything; mitigated by being behavior-preserving and landing before any
new routing logic.

---

## 6. Phase 2 — sessions

Your own optimization spec already lists this as deferred V1.5 ("per-conversation
classification cache keyed on a hash of the first user message"). Switchyard
ships the finished design; the part worth copying is that it recognizes
**coding-agent session headers**, which is what we would have gotten wrong.

### 6.1 Key derivation

```rust
pub enum SessionKey { Header(String), MessageHash(u64) }

fn session_key(headers: &Headers, messages: &[ChatMessage]) -> Option<SessionKey> {
    const HEADERS: &[&str] = &[
        "x-tmuxlet-session-id",
        "x-claude-code-session-id",   // Claude Code
        "x-session-id",               // generic
        "x-conversation-id",          // Continue
    ];
    // header wins; else hash the first user message (opt-in)
}
```

### 6.2 The store — and its bound

```rust
pub struct SessionState {
    pub decision: Option<(Vec<String>, DecisionSource)>,  // affinity
    pub excluded: HashSet<String>,   // overflow evictions (Phase 0)
    pub escalate_streak: u32,        // escalation confirmations (Phase 4)
    pub latched: Option<String>,
    pub last_seen: Instant,
}

pub struct SessionStore {
    inner: Mutex<HashMap<SessionKey, SessionState>>,
    max_entries: usize,   // default 4096
    ttl: Duration,        // default 2h
}
```

**A client-supplied header keying an unbounded map is a memory-exhaustion
vector.** Switchyard's affinity map is process-local and its docs do not
document a bound; ours must have one. Both caps are enforced: a TTL sweep on
insert (amortized, no background thread), plus hard eviction of the oldest
entry at `max_entries`. Config: `[server] session_max = 4096`,
`session_ttl_secs = 7200`.

### 6.3 Affinity semantics

- First request in a session runs the full policy; the decision is stored.
- Later requests reuse it with `DecisionSource::Affinity` and **skip the
  classifier call entirely** — this is the cost win.
- A `FallOpen` decision is stored too (matching Switchyard: "This includes
  `strong_target` when it was selected as the fallback for an unusable verdict"),
  otherwise a flaky classifier gets re-hammered every turn.
- Overflow exclusions accumulate for the session's life and are never cleared —
  documented, with the same guidance Switchyard gives: after a client truncates
  or resets context, it should send a **new** session id.

Config: `session_affinity = true` per router, `message_hash_fallback = false`
(opt-in, because the hash collides across independent conversations that open
with an identical prompt — a real hazard for templated agent harnesses).

**Estimate:** ~320 LOC + ~18 tests.

---

## 7. Phase 3 — the signal router and the cost model

The reason this proposal exists. Switchyard's
`crates/libsy/src/algorithms/util/{tool_signals,stage}.rs` is a pure,
deterministic, zero-I/O, non-async scorer — it drops into a synchronous server
without modification to its logic. And it matters more for us than for NVIDIA,
because **our backends are coding agents**: tool-result history is the signal
actually present in our traffic.

Critically: it **replaces an LLM call**. Today every request to a `[routers.x]`
pays a `gemini-flash` round-trip (`classifier_timeout_secs = 5`) before any real
work starts. The scorer is microseconds and free.

### 7.1 Signal extraction (`route/signals.rs`)

Walk the OpenAI `messages` array; collect assistant `tool_calls` and `role:"tool"`
results. Categorize each call by tool name, falling back to Bash-command pattern
matching for shell tools. The name tables cover claude-code, codex, and
hermes-style harnesses, and generic shell tools route through command patterns —
which is how `agy` gets classified.

```rust
pub struct ToolSignals {
    pub severity: f32,             // windowed max: 0.0 | 0.3 soft | 0.7 hard | 1.0 critical
    pub recent_write_count: u32,
    pub recent_edit_count: u32,
    pub recent_read_count: u32,
    pub recent_todowrite_count: u32,
    pub tests_passed: bool,
    pub turn_depth: u32,
    pub compacted: bool,
}
```

Severity comes from a curated substring table (OOM and connection-refused =
critical 1.0; traceback, import error, assertion, timeout, no-such-file = hard
0.7; bare non-zero exit = soft 0.3), evaluated over the last `recent_window = 3`
tool results so an error persists through recovery turns instead of clearing the
instant one clean result lands.

Two details worth preserving verbatim from the reference implementation, because
both are trace-mined rather than obvious:

- `"file does not exist"` is anchored as a full phrase, **not** a bare
  `"does not exist"`, which fires on `ls` output and ordinary prose.
- Test-failure keywords (`failed`, `errors`) only trip when a **non-zero integer
  precedes** them, so cargo's `0 failed` and go's `0 errors` on a clean run are
  not misread as failures.

### 7.2 The scorer (`route/scorer.rs`)

Exact constants from the reference implementation (`stage.rs:35-47`, `:244-321`):

```rust
const SIGNAL_UNIT: f64 = 0.10;         // weight of one maxed signal
const SCORE_GAIN:  f64 = 5.0;          // pre-tanh gain
const HARD_SEVERITY: f64 = 0.7;        // normalizes severity to one unit
const STALL_MIN_TURN_DEPTH: u32 = 8;   // below this, no-write turns are normal

let recent_ops = recent_write + recent_edit + recent_read + recent_todowrite;
let deep       = turn_depth >= STALL_MIN_TURN_DEPTH;
let no_prod    = recent_write == 0 && recent_edit == 0;
let investig   = recent_read >= 1 || recent_todowrite >= 1;

// spinning and exploring partition the not-producing case, so at most one
// fires — no double-counting on the production axis.
let spinning   = (deep && no_prod && !investig) as u8 as f64;
let exploring  = (deep && no_prod &&  investig) as u8 as f64;
let production = ratio(recent_write + recent_edit, recent_ops);

let raw   = SIGNAL_UNIT
          * (severity / HARD_SEVERITY + spinning + exploring - production);
let score = (SCORE_GAIN * raw).tanh();     // signed: + capable, − efficient
let confidence = score.abs();
```

Verify against the published calibration: one maxed signal → `tanh(0.5) = 0.462`;
two corroborating → `tanh(1.0) = 0.762`. That is what makes
`confidence_threshold = 0.5` mean "about one-and-a-half signals" — the property
that keeps a single noisy signal from flipping a route.

Hard overrides, checked before the score:

```rust
// → capable, unconditionally
fn should_escalate(s: &ToolSignals) -> bool {
    s.compacted || s.severity >= 1.0
}
// → efficient, unconditionally (a settled run)
fn should_deescalate(s: &ToolSignals) -> bool {
    s.tests_passed && (s.recent_write_count + s.recent_edit_count) >= 1
        && s.severity == 0.0
}
```

`compacted` is detected from a context-compaction marker in the prompt prefix and
is self-latching — once an agent's context has been summarized, the task belongs
on the capable tier and should not snap back when the summary wipes the
accumulated signals.

### 7.3 The cost model — why `capable_first`/`efficient_first` is the wrong axis

Switchyard's two pickers encode one assumption: **the capable tier costs more per
call.** Under metered API pricing that is true and the curve is linear — every
escalation is a marginal charge, so escalation needs evidence to justify itself.

tmuxlet-server's flagship backends invert this. `claude-max` and `codex` run
through tmuxlet against an *interactive subscription*: the marginal cost of a
call is **zero** until the quota window is exhausted, at which point it is
effectively **infinite** (429 until reset). The cost curve is a step function,
not a line.

Three consequences, none of which Switchyard can express:

1. **The premium backend should be the default**, not the exception. Nothing is
   saved by routing an easy turn to `gpt-oss` when `claude-max` is free at the
   margin. Both pickers are wrong here: `capable_first` does the right thing for
   the wrong reason, and `efficient_first` actively spends money to conserve
   something that is not scarce.
2. **The scarce resource is quota, not dollars.** The real question is not "is
   this task hard enough to deserve the good model" but **"how much subscription
   headroom is left, and is this turn worth spending it on?"**
3. **Difficulty only matters while rationing.** With plentiful headroom every
   turn goes to the subscription leg and the scorer changes nothing.

So the scorer's role changes from **decider** to **ranker**. Quota pressure sets
*how many* turns get the good backend; the score decides *which* ones.

### 7.4 The chain already encodes cost preference

We do not need a `capable`/`efficient` target pair. Every chain in
`examples/server-router-framework.toml` is already ordered flat-rate-first —
`["claude-max", "codex", "or-sonnet"]` reads exactly "free, then free, then
paid". What the router needs is an **entry point into the chain we already
have**, plus a reason to move it.

Declare cost on the backend, where it belongs — it is a property of the backend,
not of any route that happens to use it:

```toml
[backends.claude-max]
type = "tmuxlet"
cost = "subscription"      # subscription | local | free | metered
agentic = true             # runs tools; not substitutable by a text-only leg
```

| `cost` | Marginal call | Scarce resource | Router behavior |
|---|---|---|---|
| `subscription` | 0 until the wall | quota window | preferred; rationed under pressure |
| `local` | ~0 | your hardware | preferred; bounded by `max_concurrent` |
| `free` | 0 | provider rate limit | preferred; subscription with an unknown quota |
| `metered` | linear $ | money | the shed target — what rationing protects you from |

Defaults when unset: `subscription` for `tmuxlet`, `local` for `cli`, `metered`
for `api`. That matches how the three backend types are actually used, so most
existing configs need no edit at all.

### 7.5 Quota tracking

You usually cannot know a subscription's limit — Claude's windows are not
published as call counts. So the mechanism is **reactive first, configured
second, learned third**:

```rust
pub struct QuotaState {
    window_start: Instant,
    calls_in_window: u32,
    /// Configured, or learned from where the last 429 landed.
    limit_hint: Option<u32>,
    /// From a 429 (+ Retry-After when present). Hard exclusion until then.
    exhausted_until: Option<Instant>,
}
```

Lives beside `BackendHealth` in `State.health` — same shape of per-backend
runtime state, same lock.

1. **Reactive (always on, zero config).** A 429 sets `exhausted_until` from
   `Retry-After` when present, else `cooldown_secs`. This promotes the deferred
   V1 item **P-9 `Retry-After`** from a nicety to the load-bearing quota signal.
2. **Configured (opt-in).** `quota_calls` + `quota_window_secs` give proactive
   pressure before the wall is reached.
3. **Learned (opt-in, `quota_learn = true`).** Record `calls_in_window` at the
   first 429 and carry it as next window's `limit_hint`. Self-tuning, costs one
   failed leg per window to discover, and is the only option that works when the
   provider's limit is opaque or varies with load.

```rust
fn pressure(q: &QuotaState) -> f64 {
    if q.exhausted_until.is_some_and(|t| t > Instant::now()) { return 1.0; }
    match q.limit_hint {
        Some(limit) if limit > 0 =>
            (f64::from(q.calls_in_window) / f64::from(limit)).clamp(0.0, 1.0),
        _ => 0.0,   // unknown limit → no proactive pressure; reactive only
    }
}
```

An unknown limit yields zero pressure, so an unconfigured server behaves exactly
as it does today until it hits a 429. **Backward compatible by construction.**

> **Burn-down, deliberately omitted.** Unspent quota in an expiring window is
> wasted, so a "use it or lose it" term that *lowers* pressure late in a window is
> theoretically correct. It is also an excellent way to build something that
> spends your entire remaining quota at 4:55pm. Leave it out; revisit only with
> §12 replay data.

### 7.6 The policy: one knob, `reserve_threshold`

Both pickers collapse into a single threshold.

```toml
[routers.do]
type = "signals"
chain = "do-general"           # ordered chain; leg 0 is the preferred backend
confidence_threshold = 0.5     # how decisive the score must be to act
reserve_threshold = 0.7        # quota pressure at which rationing begins
recent_turn_window = 3

# Optional: consult the LLM classifier only on sub-threshold turns.
classifier = "gemini-flash"
fallback_class = "general"
```

- **Pressure below `reserve_threshold`** — headroom is plentiful. Every turn
  starts at leg 0. The score is computed and recorded but moves nothing.
- **Pressure at or above it** — rationing. A turn whose score does not clear
  `confidence_threshold` on the capable side starts **one leg deeper**, preserving
  the flat-rate leg. High-value turns (error recovery, spinning, exploration,
  compaction) still get leg 0.
- **Pressure `== 1.0`** — leg 0 is skipped outright, as today's cooldown path does.

The scorer's sign convention from §7.2 is unchanged. What changes is what
"escalate" *costs*: in Switchyard it spends **money**; here it spends **quota** —
free, but finite and replenishing.

**The agentic constraint.** Rationing must never shed a tool-running turn to a
text-only backend. `codex` writing files and running tests is not substitutable
by `or-sonnet` returning prose *about* writing files — the request would
"succeed" and return the wrong kind of artifact entirely.

> A turn with a non-zero tool signal (`recent_ops > 0`) may only be shed to a leg
> with `agentic = true`. If no such leg exists deeper in the chain, the turn stays
> at leg 0 regardless of quota pressure.

Spending quota you meant to save is recoverable next window. Silently returning
prose where the caller expected an edit is not. Switchyard has no analog for this
rule because all of its targets are the same kind of thing.

### 7.7 The cascade

| Rung | Condition | Source |
|---|---|---|
| 1 | session affinity hit | `Affinity` |
| 2 | leg 0 exhausted (`pressure == 1.0`) | `Quota` |
| 3 | `should_escalate` — critical error or compaction | `Override` |
| 4 | pressure < `reserve_threshold` | `Static` (leg 0 — the common path) |
| 5 | rationing + `should_deescalate` + an agentic-safe shed target | `Settled` |
| 6 | rationing + `confidence >= confidence_threshold` | `Dimensions` |
| 7 | classifier configured and returns a usable class | `Classifier` |
| 8 | anything else | `FallOpen` |

Add `Quota` to `DecisionSource` (§3.1).

Note rung 4: **with headroom, the fast path is one comparison and a return** — no
scoring, no classifier, no allocation. The expensive machinery engages only when
there is something to save. That is the property that makes this safe to enable
by default.

### 7.8 Honest limits

- Needs real tool traffic. A pure chat client (curl, a plain completion) has no
  tool history, scores 0.0, and lands on leg 0 every time. The docs must say
  this — Switchyard's "When *not* to use stage-router" section is the model.
- `turn_depth` is a message-count proxy and is wire-format dependent. Anthropic
  batches tool results into fewer messages than OpenAI-chat does, so
  `STALL_MIN_TURN_DEPTH = 8` means something different per client. Gate on it
  loosely; do not tune it before Phase 5 lands and we can measure both.
- The scorer constants were calibrated on SWE-Bench Pro Python-75 against a
  specific metered model pair. They are a starting point for our backend mix,
  not a law. §12 covers recalibration.
- **Rationing is only as good as the quota estimate.** With `quota_learn` off and
  no configured limit the router is purely reactive: it finds the wall by hitting
  it, and the turn that hits it pays one failed leg. Still strictly better than
  today, which hits the same wall *and* cools a healthy backend for
  `cooldown_secs` (§4.3) — but it is not proactive, and the docs should not imply
  otherwise.
- **Quota state is process-local and dies on restart.** A restart mid-window
  resets `calls_in_window` to zero and over-estimates headroom. Persisting to
  `~/.tmuxlet/quota.json` is ~30 LOC but adds a write to the dispatch path;
  deferred until measurement shows restarts matter.

**Estimate:** ~1050 LOC + ~75 tests (the pattern tables and the quota state
machine both need table-driven coverage). Largest phase; also the most
self-contained — `signals.rs` and `scorer.rs` are pure functions over `&Value`,
and `pressure()` is a pure function over `QuotaState`, so all three are testable
without a server or a clock.

---

## 8. Phase 4 — acceptance gates

Today `run_chain` advances on failure and on exactly one quality condition:
`res.content.trim().is_empty() && !backend.allow_empty()` (`src/http.rs:790`).
That is already an acceptance predicate — it is just hardcoded and singular.
Generalize it, and escalation falls out for free.

```rust
// exec/gate.rs
pub enum Verdict { Accept, Reject(String) }

pub trait Gate: Send + Sync {
    fn check(&self, result: &DispatchResult, ctx: &GateCtx) -> Verdict;
}

pub struct NonEmptyGate;   // today's behavior, now explicit
pub struct JudgeGate {     // NEW: escalation
    classifier: String,
    prompt: Option<String>,
    confirmations: u32,
    window_chars: usize,
}
```

`run_chain` gains one line: a leg that dispatches successfully must also pass the
chain's gates, or it is rejected and the chain advances — the same path an empty
result already takes.

### 8.1 Why this matters

The current chain only advances when a leg **fails or returns empty**. A
confidently wrong answer from `gpt-oss` wins the chain and is served. For the
`improve` router in `examples/server-router-framework.toml`
(`order = ["gemini-flash", "gpt-oss", "deepseek-flash"]`) that is the *expected*
case, not the edge case.

`JudgeGate` implements Switchyard's escalation semantics:

- Dispatch the cheap leg, buffer the reply.
- Ask the judge to rate **the completed turn** — work actually done, not a
  prediction about work that might be done.
- Increment a consecutive-escalate streak on escalate; reset to zero on decline.
- Serve the buffered reply until the streak reaches `confirmations` (default 2).
- On confirmation, discard the buffer and advance to the next leg; latch the
  session so later turns skip both the cheap leg and the judge.
- **Judge failure fails open**: serve the buffered reply, and *hold* the streak
  rather than clearing it. A judge outage must never create a latch, and must
  never reset accumulated evidence.

Cost model, stated plainly in the docs because it is not obvious: a declined turn
costs cheap-leg + judge; a confirming turn costs cheap-leg + judge + expensive-leg.
`confirmations > 1` requires a session key (Phase 2) — without one there is
nowhere to keep the streak, and the gate must degrade to `confirmations = 1`
with a startup warning rather than silently never escalating.

```toml
[chains.improve-general]
order = ["gemini-flash", "gpt-oss", "or-sonnet"]

[chains.improve-general.gate]
type = "judge"
classifier = "gemini-flash"
confirmations = 2
window_chars = 500
```

**Estimate:** ~380 LOC + ~22 tests.

---

## 9. Phase 5 — protocol surface

Two independent items, either can be dropped without affecting the others.

### 9.1 `/v1/messages` (Anthropic Messages, text-only)

Because our backends are text-in/text-out, inbound translation is shallow: parse
`messages` + the top-level `system` field, run the **same** `flatten_to_prompt`,
and emit an Anthropic-shaped envelope (`type:"message"`, `content:[{type:"text"}]`,
`stop_reason:"end_turn"`, `usage` zeroed for the same reason completions zero it
— see README, "fabricated `len/4` estimates would feed cost trackers
plausible-but-wrong numbers").

Roughly 250 LOC, versus Switchyard's 11.5k-line translation crate, precisely
because we refuse tool-call round-tripping.

**The ceiling, which must be in the README, not buried here:** a client that
sends `tools` and expects `tool_use` blocks back will not work. That is most of
Claude Code's real usage. This endpoint serves text-shaped Anthropic clients and
`api` backends; it does not make tmuxlet-server a drop-in Anthropic provider. If
that limitation makes the endpoint uninteresting, **cut this item** — it is the
one piece of the proposal whose value I would not defend hard.

### 9.2 Real streaming for `api` backends

`build_body` currently drops `stream` (`src/backend/api.rs:38`) and V1 always
requests non-streamed upstream; the SSE path emits 15s keepalives and then the
whole answer at once. For `api` backends we can pass `stream: true` upstream and
relay chunks as they arrive.

**The design constraint that governs this:** *streaming and chain fallback are
mutually exclusive after the first byte.* Once we have written a `data:` frame to
the client we have committed to that backend — there is no way to un-send it and
try the next leg. So:

- A streaming request runs the chain **non-streamed** for every leg *except the
  last eligible one*, or
- (preferred) streaming engages only after the chosen leg returns its first token
  successfully; any failure *before* the first token still falls through
  normally. First-token success is the commit point.

This also unblocks the deferred **P-7 client-disconnect detection**: a write to a
dead socket fails fast, which is the signal needed to kill the child process and
free the worker. The two were correctly identified as one piece of work in the
optimization spec; this is where they land.

`tmuxlet`/`cli` backends keep the keepalive behavior. Documented as permanent,
not as a gap.

**Estimate:** ~450 LOC + ~20 tests. Highest execution risk in the proposal.

---

## 10. Phase 6 — operations

- **`schema_version = 1`** at the top of the TOML. We have no version key today,
  so a future breaking change has no migration signal and no way to give a good
  error. Accept a missing key as `1` forever (back-compat); require it for any
  config using a Phase 3+ feature.
- **`--dry-run`** as an alias for `--validate`, purely for muscle-memory parity
  with the neighboring tool.
- **`--explain <model>`**: print the resolved policy, cascade, and leg order for a
  model id without starting the server. This is the debugging tool the config
  format has needed since the five-router framework landed.
- **U-22 SIGHUP reload**, already deferred in V1: re-load, validate, swap
  `Arc<State>`; in-flight requests finish on the old config. Now more valuable,
  because a `SessionStore` and stats counters must survive the swap — which means
  they must live *outside* the swapped `Arc<Config>`. Design for this in Phase 1
  by keeping `stats` and `sessions` as sibling `Arc`s in `State`, not fields of
  `Config`.
- **P-9 `Retry-After`**: honor the header on 429 instead of always using
  `cooldown_secs`. Small, already documented as a known gap in `cooldown_for`.

**Estimate:** ~300 LOC + ~15 tests.

---

## 11. Sequencing, effort, risk

| Phase | Scope | New LOC | Risk | Gate to proceed |
|---|---|---|---|---|
| 0 | Correctness: parse_class, classifier input, overflow | ~180 | Low | — |
| 1 | Decision layer, headers, `/v1/stats` | ~350 (+700 moved) | **High** (touches all) | Full suite green, zero behavior diff on the example configs |
| 2 | Sessions: affinity, exclusion, bounds | ~320 | Medium | Memory bound proven under a synthetic key flood |
| 3 | Signal router + cost model + quota | ~1050 | Medium | Beats leg-0-always on §12 replay *and* survives a forced-429 drill |
| 4 | Acceptance gates, escalation | ~380 | Medium | Judge-outage test shows fail-open, no latch |
| 5 | `/v1/messages`, `api` streaming | ~450 | **High** | Streaming commit-point test; drop 9.1 if unloved |
| 6 | Ops: schema_version, reload, explain | ~300 | Low | — |

Total ≈ 3000 new LOC against 5270 today. That is a large number, and it is the
main argument *against* this proposal — see §13.

**Phase 0 and Phase 1 should land as separate PRs and separate releases.** Phase 1
is a pure refactor and must be reviewable as one; mixing bug fixes into it is how
refactors become unreviewable.

Phases 3, 4, 5 are independent of each other once 1 and 2 are in. If effort runs
out, **stopping after Phase 3 leaves a coherent, complete product.** That is the
natural release boundary and the intended v0.3.0.

---

## 12. How we know any of this worked

The weakest part of the current five-router framework is that there is no
evidence it beats a plain chain. Switchyard's calibration methodology is the part
of their work most worth stealing and least likely to be, because it is the
unglamorous part.

**Build a replay harness** (`tests/replay/`, or a `--replay` flag):

1. Capture N real conversations as JSONL (the raw `messages` arrays; scrub paths).
2. Run each through the scorer offline. Emit per-turn `score`, `confidence`, and
   the tier each picker would choose. No model calls, no server — this is why
   keeping `signals.rs`/`scorer.rs` pure matters.
3. Sweep `confidence_threshold` ∈ {0.0, 0.3, 0.5, 0.7, 0.9} and read the routing
   split per threshold.

Then the four-quadrant analysis, over conversations run both ways. Read "leg 0"
as the flat-rate leg and "shed" as the metered leg one step deeper:

| Quadrant | Meaning | What it tells us |
|---|---|---|
| `RESCUE` | leg 0 passes, shed leg fails | quota well spent — protect these turns |
| `LOSS` | leg 0 fails, shed leg passes | over-reserving; leg 0 is not always better |
| `SAFE` | both pass | **safe to shed under pressure** — the rationing budget |
| `HARD` | both fail | routing is irrelevant |

Note how the quadrants re-read under §7.3. For Switchyard, `SAFE` is "cheap tier
is free money" — every SAFE turn routed cheap is dollars saved, so you want to
maximize them. For us, `SAFE` is *quota you may spend elsewhere*: shedding a SAFE
turn **costs** money and only makes sense when headroom is scarce. `SAFE` is not
a win to be harvested, it is the reserve you draw down under pressure. That is
the whole difference between a linear cost curve and a step function, and it is
why the calibration target differs too:

- **Switchyard's target:** the lowest threshold that covers RESCUE without
  inflating LOSS — minimize spend subject to quality.
- **Ours:** the `reserve_threshold` at which shedding the SAFE set is *just*
  enough to keep leg 0 available for the RESCUE set through the end of the
  window — minimize spend subject to *not running out*.

Their reported result (~20% escalation ≈ `confidence_threshold = 0.5` with
`capable_first`) is not our null hypothesis; it answers a different question. Our
null hypothesis is **`reserve_threshold = 1.0`** — never ration, always leg 0,
fall through only on an actual 429. That is today's behavior, it is free to
measure, and per §13 it may well be correct.

**Success criteria for v0.3.0:**
1. Classifier LLM calls per conversation drop by >70% (affinity + signals).
2. Median routing overhead drops from one classifier round-trip (~200-400ms) to
   <1ms on signal-routed turns, and to ~0 on the §7.7 rung-4 fast path.
3. Zero regressions on the shipped example configs (they are already asserted
   clean by `config.rs` tests — extend those to assert *decisions*, not just
   parsing).
4. `/v1/stats` shows a `fall_open` rate under 10%. Above that, the classifier
   prompt or the class list is wrong, and now we can see it.
5. **No metered call is made while flat-rate headroom remains.** This is the one
   that matters for a subscription-first deployment, and it is directly
   checkable: `decisions.dimensions + decisions.settled` should be ~0 whenever
   every `cost = "subscription"` backend reports `pressure < reserve_threshold`.
   A non-zero count there means we are shedding turns — and paying for them —
   while the free tier still had room.
6. **A forced-429 drill routes cleanly.** Point a subscription backend at a stub
   that returns 429 with `Retry-After: 120`, and assert: leg 0 is skipped with
   `DecisionSource::Quota`, `consecutive_failures` does **not** increment, no
   cooldown is applied, and the leg returns to service when `Retry-After`
   elapses — not `cooldown_secs` later.

---

## 13. Alternatives considered

**A. Do nothing; keep V1 + the §5 auto-router.** Defensible. V1 is complete, CI
is green, and it does its job. The case against: the five-router framework is
unmeasured, every router request pays an LLM round-trip, and the three Phase 0
defects are real and shipped. **At minimum, Phase 0 should land regardless of
what happens to the rest of this document.**

**B. Deprecate tmuxlet-server and adopt Switchyard.** Rejected on a verified
technical fact: Switchyard cannot execute a local process. Every target requires
an `llm_client` with a `base_url`; there is no subprocess, PTY, or `Command` code
anywhere in its tree, and no extension point where one would go. Adopting it
means losing the `tmuxlet` backend (and with it the flat-rate subscription
economics that are the entire premise), the `cli`/PTY backend, and agentic
backends that run tools in a `cwd`. It also inverts the topology — their model
puts the coding agent in front as the *client*; ours puts it behind as the
*backend*.

**C. Compose: run both, layered.** Not an alternative — it is available today and
costs nothing, in either direction:

- *Switchyard behind us:* `[backends.sy] type = "api", base_url =
  "http://127.0.0.1:4000/v1"` as a chain leg. Their routing over our metered API
  pool, our process backends untouched.
- *Switchyard in front:* point one of its targets at
  `http://127.0.0.1:3456/v1`. Its stage-router chooses between
  `claude-max-via-tmuxlet` and a cheap cloud model; we do the process execution
  only it cannot do.

The second layering looked like the more interesting one when this document was
drafted, and like a genuine argument for scoping the proposal down to Phase 0.
**§7.3 undermines it.** Switchyard's router cannot model our cost curve: to its
stage-router, a target pointed at `claude-max-via-tmuxlet` is just an endpoint,
and `capable_first`/`efficient_first` both assume the capable tier is the one
that costs money. It would spend real OpenRouter credits to *avoid* a backend
that is free at the margin — precisely backwards — and it has no way to learn
otherwise, because the notion of a replenishing quota does not exist anywhere in
its configuration surface.

So option C stays valuable for what it *is* — a way to reach their translation
and streaming without building either — but it does not substitute for Phase 3.

The argument against relying on it: Switchyard is self-described **pre-alpha,
"not for production use"**, with an API expected to change significantly before
v1.0. Putting it on the critical path of a tool you use daily is a real
operational bet. It also needs its target timeouts raised far past their defaults
— `request_timeout_secs = 1800` exists here because agent turns run for minutes,
and their retry/timeout defaults assume API-shaped latency.

**Recommendation (revised after §7.3):** Phase 0 now, unconditionally — three
shipped defects, no refactor needed. Then Phase 1, because nothing can be
measured without it.

The original plan was to run option C for a few weeks and let `/v1/stats` tell us
whether Phase 3 was worth building. **That experiment no longer answers the
question.** A cost-model-blind router in front would tell us how well
difficulty-based routing performs, when the thing we actually need to know is how
well *quota-aware* routing performs — and no configuration of Switchyard can
produce that number.

The cheap experiment that *does* answer it needs only Phase 1: instrument
decisions and per-backend 429s, run normally for two weeks, and read off how often
leg 0 is quota-exhausted. If it is near zero, headroom is never scarce, rationing
has nothing to optimize, and Phase 3 collapses to just the tool-signal scorer
(≈600 LOC) or is not worth building at all. If it is material, the cost model is
the highest-value work in this document and the rest of Phase 3 exists to serve
it.

**Phase 1 is therefore the real decision point, not Phase 3.** Build the
measurement, then let the 429 rate choose.

---

## 14. Licensing

Switchyard is **Apache-2.0**; tmuxlet-server is **MIT**. Apache-2.0 code can be
incorporated into an MIT-licensed project, but §4 requires retaining the
copyright notice, license text, and attribution for derivative works.

Two of the items above are genuine derivations, not just inspiration: the
`ERROR_PATTERNS` / tool-name / test-phrase tables (§7.1) and the scorer constants
and formula (§7.2). Practical compliance:

- Put the ported modules in `src/route/{signals,scorer}.rs` with a file header
  naming the origin, the commit, and the Apache-2.0 terms.
- Add a top-level `NOTICE` reproducing NVIDIA's copyright line for those files.
- Note the derivation in `README.md` and `CHANGELOG.md`.

Cheap, correct, and the right thing to do regardless — the trace-mining behind
those tables is real work by other people.

*Not legal advice; if the repo ever needs a formal license review, this is the
section to hand over.*

---

## 15. Open questions

1. **Does §9.1 (`/v1/messages`) earn its place?** Without tool-call support it
   serves a narrow client set. Decide before Phase 5, not during.
2. **Signal extraction across wire formats.** `turn_depth` and tool-result
   batching differ between OpenAI-chat and Anthropic shapes. If both surfaces
   ship, does the scorer need per-format normalization, or is one shared
   `recent_window` good enough? Measure before deciding.
3. ~~**Is `capable_first` even right here?**~~ **Resolved — see §7.3-7.7.** Both
   Switchyard pickers assume a linear cost curve where the capable tier costs
   more per call. A subscription is a step function: free at the margin until
   the quota wall, then infinite. The picker vocabulary is not merely misnamed
   for this deployment, it models the wrong economics. Replaced by a per-backend
   `cost` class, reactive quota tracking, and a single `reserve_threshold` that
   turns the scorer from a decider into a ranker — it only chooses *which* turns
   to shed once headroom is actually scarce.
4. **Should `Decision` carry a confidence float into `/v1/stats`?** Useful for
   §12 calibration, but it widens the public JSON contract. Probably yes, behind
   a `stats_verbose` flag.
