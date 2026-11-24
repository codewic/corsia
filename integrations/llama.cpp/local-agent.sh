#!/usr/bin/env bash
# Corsia for coding assistants on this machine: `corsia serve` streams the model's experts,
# the Corsia fork of llama-server runs it with the expert pool sized from memory, and
# `corsia agent` puts an OpenAI-compatible endpoint in front that keeps long system prompts
# reusable (stable prefix, slots, KV snapshots on disk). All three run as one systemd user
# service under a memory cap, so the rest of the machine keeps its memory.
#
# Usage:  integrations/llama.cpp/local-agent.sh start | stop | status | logs
#   CAP=10            memory for everything, in GiB (default 10)
#   MODEL=path.gguf   the model (default ~/models/gguf/Qwen3-30B-A3B-UD-Q2_K_XL.gguf, or its
#                     -cpu.gguf copy from cpu-tune.sh when there is one)
#   CTX=32768         context shared by the two slots (default 32768)
#   PORT=7720         the endpoint: point the assistant at http://127.0.0.1:PORT/v1
#   DRAFTS=1          free n-gram drafts (on: +28% on copy-heavy answers at 10 GiB, nothing lost
#                     on fresh writing); 0 turns them off
#   PROJECT_DRAFTS=0  1 also drafts from the project's files (corsia agent --corpus), experimental
# Watch it with: corsia-watch --url 127.0.0.1:7715 --agent 127.0.0.1:PORT
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CORSIA="$(cd "$HERE/../.." && pwd)"
CAP="${CAP:-10}"
MODEL="${MODEL:-$HOME/models/gguf/Qwen3-30B-A3B-UD-Q2_K_XL.gguf}"
# a copy tuned to this CPU by cpu-tune.sh, when there is one, is used instead
if [ -f "${MODEL%.gguf}-cpu.gguf" ]; then MODEL="${MODEL%.gguf}-cpu.gguf"; fi
CTX="${CTX:-32768}"
PORT="${PORT:-7720}"
DAEMON_PORT="${DAEMON_PORT:-7715}"
LLAMA_PORT="${LLAMA_PORT:-8094}"
CORSIA_BIN="${CORSIA_BIN:-$CORSIA/target/release/corsia}"
LLAMA_SERVER="${LLAMA_SERVER:-$HOME/.cache/corsia-build/llama.cpp/build/bin/llama-server}"
STATE="$HOME/.cache/corsia/local"
KV="$HOME/.cache/corsia/kv"
MNT="$STATE/mnt"
UNIT=corsia-local
DRAFTS="${DRAFTS:-1}"
PROJECT_DRAFTS="${PROJECT_DRAFTS:-0}"
CORPUS="$HOME/.cache/corsia/corpus/current.tok"

die() { echo "local-agent: $*" >&2; exit 1; }

status() {
  if ! systemctl --user is-active --quiet "$UNIT"; then
    echo "Corsia for coding assistants is not running (start it with: $0 start)"
    return 1
  fi
  if curl -sf "http://127.0.0.1:$PORT/api/agent" >/dev/null; then
    echo "Ready: point your assistant at http://127.0.0.1:$PORT/v1 (model name: local)"
  else
    echo "Starting: loading the model and filling the expert pool (about a minute)"
  fi
  grep -hE "sized from|expert pool of|repacking" "$STATE/llama-server.log" 2>/dev/null | sed 's/^[0-9.]* I //' | awk '!seen[$0]++' || true
  echo "Memory: $(( $(systemctl --user show -p MemoryCurrent --value "$UNIT") / 1048576 )) MiB of ${CAP} GiB"
  echo "Watch: corsia-watch --url 127.0.0.1:$DAEMON_PORT --agent 127.0.0.1:$PORT"
}

case "${1:-status}" in
  start)
    if systemctl --user is-active --quiet "$UNIT"; then status; exit 0; fi
    [ -f "$MODEL" ] || die "no model at $MODEL"
    [ -x "$CORSIA_BIN" ] || die "no corsia at $CORSIA_BIN (cargo build --release in $CORSIA)"
    [ -x "$LLAMA_SERVER" ] || die "no llama-server at $LLAMA_SERVER (build the Corsia fork, see README.md here)"
    for p in "$PORT" "$DAEMON_PORT" "$LLAMA_PORT"; do
      if ss -ltn "( sport = :$p )" | grep -q LISTEN; then die "port $p is taken; set PORT, DAEMON_PORT or LLAMA_PORT"; fi
    done
    mkdir -p "$STATE" "$KV" "$MNT"
    if mountpoint -q "$MNT"; then fusermount3 -u "$MNT"; fi
    systemctl --user reset-failed "$UNIT" 2>/dev/null || true
    systemd-run --user --unit "$UNIT" --quiet --collect --expand-environment=no \
      -p MemoryMax="${CAP}G" -p MemorySwapMax=0 \
      env MODEL="$MODEL" MNT="$MNT" STATE="$STATE" KV="$KV" CTX="$CTX" PORT="$PORT" DAEMON_PORT="$DAEMON_PORT" \
          LLAMA_PORT="$LLAMA_PORT" CORSIA_BIN="$CORSIA_BIN" LLAMA_SERVER="$LLAMA_SERVER" \
          DRAFTS="$DRAFTS" PROJECT_DRAFTS="$PROJECT_DRAFTS" CORPUS="$CORPUS" \
      bash -c '
        rm -f "$STATE/serve.log"
        "$CORSIA_BIN" serve "$MODEL" --mount "$MNT" --port "$DAEMON_PORT" --cache-mib 16 > "$STATE/serve.log" 2>&1 &
        until grep -qE "Press Ctrl\+C|corsia:" "$STATE/serve.log" 2>/dev/null; do sleep 0.5; done
        sock="$(sed -n "s/^  Engines that speak Corsia: \(.*\)  (hinted.*/\1/p" "$STATE/serve.log")"
        [ -n "$sock" ] || { echo "corsia serve did not start:"; cat "$STATE/serve.log"; exit 1; }
        spec=(); [ "$DRAFTS" = 1 ] && spec=(--spec-type ngram-simple)
        corpus_env=""; corpus_arg=(--corpus off)
        if [ "$PROJECT_DRAFTS" = 1 ]; then corpus_env="$CORPUS"; corpus_arg=(--corpus "$CORPUS"); fi
        CORSIA_SOCKET="$sock" CORSIA_DRAFT_CORPUS="$corpus_env" "$LLAMA_SERVER" -m "$MNT/$(basename "$MODEL")" --alias local "${spec[@]}" \
          -c "$CTX" -np 2 --kv-unified -fa on -ctk q8_0 -ctv q8_0 -b 2048 -ub 512 -cram 0 \
          --slot-save-path "$KV" --jinja --no-warmup --port "$LLAMA_PORT" > "$STATE/llama-server.log" 2>&1 &
        until curl -sf "http://127.0.0.1:$LLAMA_PORT/health" >/dev/null; do sleep 2; done
        exec "$CORSIA_BIN" agent --upstream "127.0.0.1:$LLAMA_PORT" --port "$PORT" --snapshots "$KV" "${corpus_arg[@]}" > "$STATE/agent.log" 2>&1
      '
    echo "Starting Corsia for coding assistants (${CAP} GiB cap, logs in $STATE)."
    echo "It is ready when '$0 status' says so: about a minute to load and fill the expert pool."
    ;;
  stop)
    systemctl --user stop "$UNIT" 2>/dev/null || true
    if mountpoint -q "$MNT"; then fusermount3 -u "$MNT"; fi
    echo "Stopped."
    ;;
  status) status ;;
  logs) tail -n 20 "$STATE/serve.log" "$STATE/llama-server.log" "$STATE/agent.log" 2>/dev/null ;;
  *) die "usage: $0 start | stop | status | logs" ;;
esac
