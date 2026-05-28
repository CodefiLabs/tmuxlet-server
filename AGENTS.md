# AGENTS.md

## Audience

You are an AI coding agent installing **tmuxlet-server** on the user's machine.
Follow these steps in order. Each step has a precondition, a command, an
expected outcome, and a verification you must run before moving on. Do not ask
the user questions you can answer by running a command.

> **Status:** V1 complete. All three backend `dispatch` functions
> (`src/backend/{tmuxlet,api,cli}.rs`) are implemented and tested; `/health`,
> `/v1/models`, and `/v1/chat/completions` (streaming and non-streaming) all
> work. See the implementation plan at
> `docs/superpowers/plans/2026-05-28-tmuxlet-server.md` for design details.

## Step 1: Verify prerequisites

```
command -v cargo   # Rust toolchain (edition 2024 / Rust >= 1.92)
command -v tmux    # required only if you use a `tmuxlet` backend
```

For each configured backend type you also need its tool:
- `tmuxlet` backend → `command -v tmuxlet` and the target CLI (`command -v claude`).
- `cli` backend → the `bin` path must exist and be executable. **agy specifically
  is a symlink into `/Applications/Antigravity.app`** — verify with
  `test -e "$(readlink -f /Users/$USER/.antigravity/antigravity/bin/agy)" && echo OK || echo "BROKEN: install Antigravity.app"`.
- `api` backend → the upstream must be reachable (`curl -fsS http://127.0.0.1:11434/api/tags` for Ollama).

If a prerequisite is missing, STOP and tell the user which to install.

## Step 2: Install

```
cargo install --git https://github.com/CodefiLabs/tmuxlet-server --force
tmuxlet-server --version
```
Expected: prints `tmuxlet-server <version>`. If not, the `cargo install` failed —
read its output.

## Step 3: Write config

```
mkdir -p ~/.tmuxlet
cp examples/server.toml ~/.tmuxlet/server.toml   # then edit for the detected backends
```
Use only backends whose prerequisites passed Step 1. `api_key_env` holds the
NAME of an env var (e.g. `OPENROUTER_API_KEY`), never the key itself.

## Step 4: Validate config

```
tmuxlet-server --validate
```
Expected: `OK (N backends, M chains)` and exit 0. On error the message names the
offending key (e.g. `chain 'default' references undefined backend 'ghost'`).

## Step 5: Install as a service

### macOS (LaunchAgent)
```
cp examples/launchagent.plist ~/Library/LaunchAgents/com.codefilabs.tmuxlet-server.plist
# edit paths/username inside the plist, then:
launchctl load ~/Library/LaunchAgents/com.codefilabs.tmuxlet-server.plist
launchctl list | grep tmuxlet-server      # verification
```

### Linux (systemd user)
```
mkdir -p ~/.config/systemd/user
cp examples/tmuxlet-server.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now tmuxlet-server
systemctl --user status tmuxlet-server    # verification
```

> Startup note: with `env_source = "shell"` the server runs `$SHELL -ilc env`
> once at boot (~1s with a heavy interactive zsh) so PATH entries from
> `~/.zshrc` (e.g. agy) are visible to backends. Set `env_source = "process"` to
> skip that if your service environment already has the right PATH.

## Step 6: Smoke test

```
curl -fsS http://127.0.0.1:3456/health
```
Expected: `{"status":"ok","backends":N,"chains":M}`.

```
curl -fsS -X POST http://127.0.0.1:3456/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"default","messages":[{"role":"user","content":"say READY"}]}'
```
Expected: a chat completion JSON with non-empty `choices[0].message.content`.
(Or run `./scripts/smoke.sh`.)

## Step 7: Wire downstream clients

Point any OpenAI-compatible client at `http://127.0.0.1:3456/v1` with any API
key (auth is not checked in V1). The `model` field selects a chain name (e.g.
`default`, `thinking`) or a single backend name.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `failed to bind 127.0.0.1:3456` | another process owns the port; change `[server] listen` |
| chat returns `503 all_backends_failed` | read the `detail` array; each entry names the backend and its error |
| `agy` leg always fails / `spawn failed` | `agy` symlink is broken — install Antigravity.app |
| api backend `HTTP 401` | the `api_key_env` var is unset in the captured env; check Step 5 startup note |
| startup hangs ~seconds | interactive-shell env capture; set `env_source = "process"` |
