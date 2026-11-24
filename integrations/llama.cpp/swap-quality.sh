#!/usr/bin/env bash
# Quality gate for residency-aware routing (CORSIA_SWAP): how far swapping missing experts
# for close runners-up moves the model's output, measured against the unchanged model.
#
# Runs llama-perplexity in micro-batches of one token (swapping only happens on one-token
# steps), streamed through Corsia under a memory cap, once without swapping to save the
# model's full output for every token, then once per swap setting to compare against it:
# KL divergence (0 = identical), how often the top token is still the same, and perplexity.
#
# Usage:  integrations/llama.cpp/swap-quality.sh MODEL.gguf CAP_GIB TEXT [COPY_DIR ...]
#   SETTINGS="0.5:2 0.3:4"   settings to test, RATIO:MAX[:POOL_MIB[:SKIP]] each (default "0.5:2 0.3:4");
#                            SKIP turns on confidence-based skipping (CORSIA_SKIP) at that threshold;
#                            RATIO 0 means no swapping, POOL_MIB turns on the expert pool
#                            (CORSIA_POOL_MIB), so "0:0:3584" checks the pool alone: its KL
#                            divergence must be exactly 0, the bytes are the same
#   CTX=256 CHUNKS=3         tokens per chunk and chunks scored (default 256 and 3)
# Needs the fork built with llama-perplexity, like stream-on-40gb.sh.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CORSIA="$(cd "$HERE/../.." && pwd)"
WORK="${WORK:-$HOME/.cache/corsia-build}"
PORT="${PORT:-7714}"
CTX="${CTX:-256}"
CHUNKS="${CHUNKS:-3}"
SETTINGS="${SETTINGS:-0.5:2 0.3:4}"
CORSIA_BIN="${CORSIA_BIN:-$CORSIA/target/release/corsia}"
PPL="$WORK/llama.cpp/build/bin/llama-perplexity"

die() { echo "swap-quality: $*" >&2; exit 1; }
[ $# -ge 3 ] || die "usage: $0 MODEL.gguf CAP_GIB TEXT [COPY_DIR ...]"
MODEL="$(realpath "$1")"; CAP="$2"; TEXT="$(realpath "$3")"; shift 3
[ -f "$MODEL" ] || die "no model at $MODEL"
[ -f "$TEXT" ] || die "no text at $TEXT"
[ -x "$PPL" ] || die "no llama-perplexity at $PPL (build it with scripts/corsia-build.sh llama-perplexity)"
MNT="$WORK/mnt"; mkdir -p "$MNT"
RUN="swapq-$(date +%Y%m%d-%H%M%S)"
LOG="$WORK/$RUN.log"
BASE="$WORK/$RUN.kld"
COPIES="${*:-}"
echo "Results go to $LOG"

# One pass: a fresh capped scope with Corsia serving the mount, cold caches.
pass() {
  local label=$1 swap=$2 ratio=$3 max=$4 mode=$5 pool=${6:-0} skip=${7:-0}
  if mountpoint -q "$MNT"; then fusermount3 -u "$MNT"; fi
  python3 -c 'import os,sys; fd=os.open(sys.argv[1],os.O_RDONLY); os.posix_fadvise(fd,0,0,os.POSIX_FADV_DONTNEED)' "$MODEL"
  echo "== $label" | tee -a "$LOG"
  systemd-run --user --scope --quiet --expand-environment=no -p MemoryMax="${CAP}G" -p MemorySwapMax=0 \
    env MODEL="$MODEL" MNT="$MNT" WORK="$WORK" PORT="$PORT" CORSIA_BIN="$CORSIA_BIN" PPL="$PPL" COPIES="$COPIES" \
        TEXT="$TEXT" BASE="$BASE" CTX="$CTX" CHUNKS="$CHUNKS" SWAP="$swap" RATIO="$ratio" MAX="$max" MODE="$mode" POOL="$pool" SKIP="$skip" \
    bash -c '
      set -e
      copy_args=(); for d in $COPIES; do copy_args+=(--copy-to "$d"); done
      rm -f "$WORK/serve-swapq.log"  # a stale log from an earlier run would be read before Corsia rewrites it
      "$CORSIA_BIN" serve "$MODEL" --mount "$MNT" --port "$PORT" --cache-mib 16 "${copy_args[@]}" > "$WORK/serve-swapq.log" 2>&1 &
      cpid=$!
      until grep -qE "Press Ctrl\+C|corsia:" "$WORK/serve-swapq.log"; do sleep 0.5; done
      grep -q "Press Ctrl+C" "$WORK/serve-swapq.log" || { echo "corsia: did not start:"; cat "$WORK/serve-swapq.log"; exit 1; }
      until ! kill -0 "$cpid" 2>/dev/null || curl -s "http://127.0.0.1:$PORT/api/status" | python3 -c "import sys,json; sys.exit(0 if json.load(sys.stdin)[\"copier\"][\"state\"] in (\"done\",\"no copies\") else 1)"; do sleep 5; done
      sock="$(sed -n "s/^  Engines that speak Corsia: \(.*\)  (hinted.*/\1/p" "$WORK/serve-swapq.log")"
      [ -n "$sock" ] || { echo "corsia: did not start: no engine socket in its log"; kill -INT "$cpid"; exit 1; }
      if [ "$MODE" = base ]; then
        what=(-f "$TEXT" --kl-divergence-base "$BASE")
      else
        what=(--kl-divergence-base "$BASE" --kl-divergence)
      fi
      CORSIA_SOCKET="$sock" CORSIA_PREDICT=1 CORSIA_SWAP="$SWAP" CORSIA_SWAP_RATIO="$RATIO" CORSIA_SWAP_MAX="$MAX" CORSIA_POOL_MIB="$POOL" CORSIA_SKIP="$SKIP" \
        nice -n 5 "$PPL" -m "$MNT/$(basename "$MODEL")" "${what[@]}" -c "$CTX" --chunks "$CHUNKS" -b "$CTX" -ub 1 \
        -fa on -ctk q8_0 -ctv q8_0 || true
      kill -INT "$cpid"; wait "$cpid" || true
    ' 2>&1 | tee -a "$LOG" | grep -E '^== |swapped|skipped|expert pool|Final estimate|Mean +KLD|Same top|eval time|did not start' || true
}

pass "unchanged model (saves the reference output)" 0 0 0 base
for s in $SETTINGS; do
  IFS=: read -r ratio max pool skip <<< "$s"
  pool=${pool:-0}; skip=${skip:-0}
  if [ "$ratio" = 0 ]; then swap=0; label="no swapping"; else swap=1; label="swap ratio $ratio, at most $max per layer"; fi
  [ "$pool" = 0 ] || label="$label, expert pool $pool MiB"
  [ "$skip" = 0 ] || label="$label, skipping under $skip"
  pass "$label" "$swap" "$ratio" "$max" compare "$pool" "$skip"
done
echo "Done. KL divergence near 0 and the same top token almost always mean swapping is safe (full output in $LOG)."
