#!/usr/bin/env bash
# Manual end-to-end smoke test against a running tmuxlet-server (NOT part of CI).
# Run before releases to confirm the daily workflow still works.
#
#   ./scripts/smoke.sh [BASE_URL] [MODEL]
#
# Defaults: BASE_URL=http://127.0.0.1:3456  MODEL=default
#
# NOTE: the `agy` leg requires Antigravity.app installed (the agy binary is a
# symlink into it). If agy is a broken symlink the `default` chain falls through
# to the next backend.
set -euo pipefail

BASE="${1:-http://127.0.0.1:3456}"
MODEL="${2:-default}"

say() { printf '\n=== %s ===\n' "$1"; }

say "health"
curl -fsS "$BASE/health"; echo

say "models"
curl -fsS "$BASE/v1/models"; echo

say "chat (non-streaming) via model=$MODEL"
curl -fsS -X POST "$BASE/v1/chat/completions" \
  -H 'Content-Type: application/json' \
  -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"say READY\"}]}" \
  | tee /tmp/tmuxlet-smoke-resp.json; echo
grep -q '"content"' /tmp/tmuxlet-smoke-resp.json && echo "OK: got content" || { echo "FAIL: no content"; exit 1; }

say "chat (streaming) via model=$MODEL"
curl -fsS -N -X POST "$BASE/v1/chat/completions" \
  -H 'Content-Type: application/json' \
  -d "{\"model\":\"$MODEL\",\"stream\":true,\"messages\":[{\"role\":\"user\",\"content\":\"say READY\"}]}" \
  | tee /tmp/tmuxlet-smoke-stream.txt; echo
grep -q 'chat.completion.chunk' /tmp/tmuxlet-smoke-stream.txt && echo "OK: got chunks" || { echo "FAIL: no chunks"; exit 1; }
grep -q 'data: \[DONE\]' /tmp/tmuxlet-smoke-stream.txt && echo "OK: stream terminated with [DONE]" || { echo "FAIL: no [DONE]"; exit 1; }

echo
echo "SMOKE PASSED"
