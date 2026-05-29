# tmuxlet-server

A single static Rust binary that exposes an **OpenAI-compatible
`/v1/chat/completions`** endpoint backed by a configurable fallback chain of
heterogeneous AI backends:

- **`tmuxlet`** — interactive coding CLIs (claude, codex, gemini, …) run through
  [`tmuxlet`](https://github.com/CodefiLabs/tmuxlet).
- **`cli`** — arbitrary CLI tools, optionally in a PTY (for tools like `agy`
  that detect a non-TTY and redirect output). Use the `{prompt}` placeholder in
  `args` to inject the prompt as a flag value (e.g. `args = ["-p", "{prompt}"]`);
  without it the prompt is appended as the final positional argument.
- **`api`** — any OpenAI-compatible HTTP upstream (Ollama, OpenRouter, …).

> Companion project: [**CodefiLabs/tmuxlet**](https://github.com/CodefiLabs/tmuxlet)
> drives the interactive CLIs behind the `tmuxlet` backend.

Downstream clients (Continue, Cursor, Hermes, anything OpenAI-shaped) see one
endpoint; the routing happens server-side from a TOML config. A request's
`model` field selects a named chain (or a single backend); the server walks the
chain in order until one succeeds.

## Install

```
cargo install --git https://github.com/CodefiLabs/tmuxlet-server --locked --force
cp examples/server.toml ~/.tmuxlet/server.toml   # then edit
tmuxlet-server --validate
tmuxlet-server                                    # serve on 127.0.0.1:3456
```

`--locked` is required: it installs the dependency versions pinned in this
repo's `Cargo.lock`. Without it, Cargo resolves to the latest semver-compatible
releases, and `dispatch2 0.3.1` (pulled in transitively by `ctrlc` on macOS)
fails to compile with a `recursion limit reached` macro error.

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
`portable-pty`, `nix` (already in-tree via `portable-pty`; used to disable PTY
echo). ~25 crates including transitive.

## Status

**V1 complete.** All three backend `dispatch` functions
(`src/backend/{tmuxlet,api,cli}.rs`) are implemented and tested; `/health`,
`/v1/models`, and `/v1/chat/completions` (streaming and non-streaming) all work,
as does the fallback chain. CI (`fmt + clippy + test`) is green on macOS and
Linux. See [`docs/STATUS.md`](./docs/STATUS.md) for the verification summary and
[`docs/superpowers/plans/2026-05-28-tmuxlet-server.md`](./docs/superpowers/plans/2026-05-28-tmuxlet-server.md)
for the design.

## License

MIT © Codefi Foundation on Rural Innovation
