# tmuxlet-server

A single static Rust binary that exposes an **OpenAI-compatible
`/v1/chat/completions`** endpoint backed by a configurable fallback chain of
heterogeneous AI backends:

- **`tmuxlet`** — interactive coding CLIs (claude, codex, gemini, …) run through
  [`tmuxlet`](https://github.com/CodefiLabs/tmuxlet).
- **`cli`** — arbitrary CLI tools, optionally in a PTY (for tools like `agy`
  that detect a non-TTY and redirect output).
- **`api`** — any OpenAI-compatible HTTP upstream (Ollama, OpenRouter, …).

Downstream clients (Continue, Cursor, Hermes, anything OpenAI-shaped) see one
endpoint; the routing happens server-side from a TOML config. A request's
`model` field selects a named chain (or a single backend); the server walks the
chain in order until one succeeds.

## Install

```
cargo install --git https://github.com/CodefiLabs/tmuxlet-server --force
cp examples/server.toml ~/.tmuxlet/server.toml   # then edit
tmuxlet-server --validate
tmuxlet-server                                    # serve on 127.0.0.1:3456
```

For step-by-step install + service setup + verification, see
[`AGENTS.md`](./AGENTS.md). Example config: [`examples/server.toml`](./examples/server.toml).

## HTTP surface (V1)

| Method | Path | Behavior |
|---|---|---|
| `GET` | `/health` | `{"status":"ok","backends":N,"chains":M}` |
| `GET` | `/v1/models` | OpenAI-compatible list of chain + backend names |
| `POST` | `/v1/chat/completions` | OpenAI chat completions (JSON or SSE) |

Reserved namespaces (`/`, `/ui/*`, `/api/*`) return `501` for a future web UI;
everything else returns `404`. Errors use the OpenAI error envelope.

## Streaming limitation (V1)

V1 backends are blocking: `cli`/`tmuxlet` emit output only after the child
exits, and `api` is a single non-streamed call. So with `stream: true` the SSE
frames (role-prime → content → final → `[DONE]`) are emitted all at once after
the full response is ready. True incremental streaming is V2.

## Dependency budget

Synchronous (no tokio). Direct deps: `tiny_http`, `ctrlc`, `serde`,
`serde_json`, `toml`, `rustls` (ring backend — no C toolchain), `webpki-roots`,
`portable-pty`. ~25 crates including transitive.

## Status

V1 scaffold: pure logic, config, routing, the `rustls` HTTP client, and the
`portable-pty` runner are implemented and tested (`cargo test` → green). The
three backend `dispatch` entry points are stubbed pending the implementation
plan in [`docs/superpowers/plans/2026-05-28-tmuxlet-server.md`](./docs/superpowers/plans/2026-05-28-tmuxlet-server.md).

## License

MIT © Codefi Foundation on Rural Innovation
