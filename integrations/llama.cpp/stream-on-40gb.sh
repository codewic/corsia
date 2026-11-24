#!/usr/bin/env bash
# Expert streaming test for the target machine: generation speed of an MoE model that does
# not fit in the memory it is given, three ways, under the same memory cap:
#   ssd      llama.cpp memory-maps the original file (stock behaviour)
#   mount    llama.cpp memory-maps the file from Corsia's mount (blind mode)
#   stream   the mount plus demand prefetch of each layer's chosen experts (patch 0002)
#   predict  the same plus guessing the next layer's experts from the hidden state (0003)
#   swap     predict plus residency-aware routing (CORSIA_SWAP=1): a chosen expert that is not
#            in memory is swapped for a close runner-up that is (not in the default ARMS)
# Corsia and llama.cpp run inside one systemd scope so the cap covers both: pages Corsia
# stores into the page cache count against the process that stored them.
#
# Usage:  integrations/llama.cpp/stream-on-40gb.sh MODEL.gguf CAP_GIB [COPY_DIR ...]
#   CAP_GIB: memory for model + engine + Corsia, e.g. 12 for an 18 GB model.
#
# Needs what try-on-40gb.sh needs, plus systemd-run with user scopes.
# LLAMA_ARGS and CORSIA_ARGS override the memory-lean defaults below. SKIP_BUILD=1 uses
# the binaries already built (CORSIA_BIN picks another Corsia binary). LLAMA_ENV adds environment for llama.cpp only, for example
# LLAMA_ENV="OMP_WAIT_POLICY=PASSIVE" (waiting threads sleep at once instead of spinning).
# CPU_QUOTA caps the whole scope, for example CPU_QUOTA=150% for one and a half cores.
# Every arm reports the CPU seconds it used and the package temperature, so speed can be
# weighed against heat. On a machine with less than 24 GiB of RAM it refuses unless FORCE=1
# (the cap still protects the machine).
# Progress goes to ~/.cache/corsia-build/progress for `corsia watch` (CORSIA_PROGRESS=off
# to skip, or a folder to put it elsewhere).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CORSIA="$(cd "$HERE/../.." && pwd)"
TAG="${LLAMA_TAG:-b12283}"
WORK="${WORK:-$HOME/.cache/corsia-build}"
JOBS="${JOBS:-$(( $(nproc) > 1 ? $(nproc) / 2 : 1 ))}"
TOKENS="${TOKENS:-64}"
PORT="${PORT:-7714}"
# Memory-lean defaults: small context, 8-bit KV cache (needs flash attention), small batches.
# No -t: llama.cpp already uses one thread per physical core, and hyperthreads only add heat
# to matrix work.
LLAMA_ARGS="${LLAMA_ARGS:--c 2048 -fa on -ctk q8_0 -ctv q8_0 -b 256 -ub 256}"
LLAMA_ENV="${LLAMA_ENV:-}"
CPU_QUOTA="${CPU_QUOTA:-}"
# The CPU package temperature sensor, if the machine has one we can read.
TEMP_SENSOR=""
for h in /sys/class/hwmon/hwmon*; do
  case "$(cat "$h/name" 2>/dev/null)" in coretemp|k10temp|zenpower) TEMP_SENSOR="$h/temp1_input"; break ;; esac
done
# Corsia's own block cache stays tiny: prefetch stores straight into the page cache.
CORSIA_ARGS="${CORSIA_ARGS:---cache-mib 16}"

# shellcheck source=../progress.sh
. "$HERE/../progress.sh"
die() { echo "stream-on-40gb: $*" >&2; prog log msg="$*"; exit 1; }
[ $# -ge 2 ] || die "usage: $0 MODEL.gguf CAP_GIB [COPY_DIR ...]"
MODEL="$(realpath "$1")"; CAP="$2"; shift 2
[ -f "$MODEL" ] || die "no model at $MODEL"
ram_gib=$(( $(awk '/MemTotal/ {print $2}' /proc/meminfo) / 1048576 ))
[ "$ram_gib" -ge 24 ] || [ "${FORCE:-0}" = 1 ] || die "only ${ram_gib} GiB of RAM (FORCE=1 to override)"
for t in git cmake cargo curl python3 fusermount3 systemd-run; do command -v "$t" >/dev/null || die "install $t first"; done
model_gib=$(( $(stat -c %s "$MODEL") / 1073741824 ))
[ "$CAP" -lt "$model_gib" ] || echo "Note: CAP_GIB=$CAP is not below the model size (${model_gib} GiB), so nothing has to stream."

mkdir -p "$WORK"
RUN="stream-$(date +%Y%m%d-%H%M%S)"
LOG="$WORK/$RUN.log"
echo "Results go to $LOG"
read -r -a ARM_LIST <<< "${ARMS:-ssd mount stream predict}"
prog_open "$RUN"
prog start title="Expert streaming: $(basename "$MODEL" .gguf), cap $CAP GiB" kind="stream test" unit=arms \
  total="${#ARM_LIST[@]}" pid=$$ headline.metric=tok/s headline.label=Generating headline.better=higher \
  meta.model="$(basename "$MODEL")" meta.cap="$CAP GiB" meta.tokens="$TOKENS" meta.host="$(hostname -s)" meta.log="$LOG"
PROG_ENDED=0
trap 'rc=$?; [ "$PROG_ENDED" = 1 ] || prog end status=failed summary.exit="$rc" summary.log="$LOG"' EXIT

if [ "${SKIP_BUILD:-0}" != 1 ]; then
prog log msg="Building Corsia and llama.cpp" current=build
(cd "$CORSIA" && cargo build --release -j "$JOBS" 2>&1 | tail -1)
fi
CORSIA_BIN="${CORSIA_BIN:-$CORSIA/target/release/corsia}"
[ -d "$WORK/llama.cpp" ] || git clone --depth 1 --branch "$TAG" https://github.com/ggml-org/llama.cpp.git "$WORK/llama.cpp"
cd "$WORK/llama.cpp"
if grep -q predict_next common/corsia-experts.h 2>/dev/null && grep -q corsia_prefetch_async src/corsia-client.h 2>/dev/null; then
  echo "this checkout already has the Corsia changes (fork branch or patched earlier)"
else
  for p in "$HERE"/000*.patch; do git apply "$p"; done
fi
if [ "${SKIP_BUILD:-0}" != 1 ]; then
  cmake -B build -DCMAKE_BUILD_TYPE=Release -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=OFF >/dev/null
  cmake --build build --target llama-completion -j "$JOBS" 2>&1 | tail -1
fi
LLAMA="$WORK/llama.cpp/build/bin/llama-completion"
[ -x "$LLAMA" ] || die "no llama-completion at $LLAMA; run without SKIP_BUILD=1"
MNT="$WORK/mnt"; mkdir -p "$MNT"
COPIES="${*:-}"

# One arm: a fresh scope with the cap, Corsia inside it when needed, cold caches.
arm() {
  local name=$1
  if mountpoint -q "$MNT"; then fusermount3 -u "$MNT"; fi
  python3 -c 'import os,sys; fd=os.open(sys.argv[1],os.O_RDONLY); os.posix_fadvise(fd,0,0,os.POSIX_FADV_DONTNEED)' "$MODEL"
  local temps="$WORK/temps-$name" sampler="" quota=()
  : > "$temps"
  if [ -n "$TEMP_SENSOR" ]; then
    ( while :; do cat "$TEMP_SENSOR" >> "$temps"; sleep 1; done ) &
    sampler=$!
  fi
  [ -z "$CPU_QUOTA" ] || quota=(-p CPUQuota="$CPU_QUOTA")
  # --expand-environment=no: systemd-run would otherwise expand ${...} in the script below
  # itself (it turned "${copy_args[@]}" into an empty argument).
  systemd-run --user --scope --quiet --expand-environment=no -p MemoryMax="${CAP}G" -p MemorySwapMax=0 "${quota[@]}" \
    env ARM="$name" LLAMA_ENV="$LLAMA_ENV" MODEL="$MODEL" MNT="$MNT" WORK="$WORK" PORT="$PORT" TOKENS="$TOKENS" \
        CORSIA_BIN="$CORSIA_BIN" LLAMA="$LLAMA" COPIES="$COPIES" LLAMA_ARGS="$LLAMA_ARGS" CORSIA_ARGS="$CORSIA_ARGS" \
    bash -c '
      set -e
      echo "== $ARM"
      cg="/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)"; t0=$(date +%s.%N)
      model="$MODEL"; sock=""; cpid=""
      if [ "$ARM" != ssd ]; then
        copy_args=(); for d in $COPIES; do copy_args+=(--copy-to "$d"); done
        rm -f "$WORK/serve-$ARM.log"  # a stale log from an earlier run would be read before Corsia rewrites it
        "$CORSIA_BIN" serve "$MODEL" --mount "$MNT" --port "$PORT" $CORSIA_ARGS "${copy_args[@]}" > "$WORK/serve-$ARM.log" 2>&1 &
        cpid=$!
        until grep -qE "Press Ctrl\+C|corsia:" "$WORK/serve-$ARM.log"; do sleep 0.5; done
        if ! grep -q "Press Ctrl+C" "$WORK/serve-$ARM.log"; then echo "corsia: did not start:"; cat "$WORK/serve-$ARM.log"; exit 1; fi
        until ! kill -0 "$cpid" 2>/dev/null || curl -s "http://127.0.0.1:$PORT/api/status" | python3 -c "import sys,json; sys.exit(0 if json.load(sys.stdin)[\"copier\"][\"state\"] in (\"done\",\"no copies\") else 1)"; do sleep 5; done
        kill -0 "$cpid" 2>/dev/null || { echo "corsia: stopped early:"; tail -5 "$WORK/serve-$ARM.log"; exit 1; }
        model="$MNT/$(basename "$MODEL")"
        if [ "$ARM" = stream ] || [ "$ARM" = predict ] || [ "$ARM" = swap ]; then
          sock="$(sed -n "s/^  Engines that speak Corsia: \(.*\)  (hinted.*/\1/p" "$WORK/serve-$ARM.log")"
          [ -n "$sock" ] || { echo "corsia: did not start: no engine socket in its log"; kill -INT "$cpid"; exit 1; }
        fi
      fi
      # CORSIA_SWAP is set only by the swap arm, so other arms get the default of the fork
      # (swapping on with the expert pool, off without it).
      predict=0; swapenv=""
      case "$ARM" in predict) predict=1 ;; swap) predict=1; swapenv="CORSIA_SWAP=1" ;; esac
      env $swapenv $LLAMA_ENV CORSIA_SOCKET="$sock" CORSIA_PREDICT="$predict" nice -n 5 "$LLAMA" -m "$model" $LLAMA_ARGS -p "Write a short story about a lighthouse keeper." -n "$TOKENS" -no-cnv --no-warmup || true
      if [ -n "$cpid" ]; then
        awk "{printf \"heat: Corsia %.1f CPU seconds (%.1f user, %.1f system)\\n\", (\$14+\$15)/100, \$14/100, \$15/100}" /proc/$cpid/stat
        kill -INT "$cpid"; wait "$cpid" || true
      fi
      awk "/^(pgfault|pgmajfault|workingset_refault_file) / {printf \"heat: %s %d\\n\", \$1, \$2}" "$cg/memory.stat"
      awk -v t0="$t0" -v t1="$(date +%s.%N)" "{ v[\$1] = \$2 } END { printf \"heat: %.1f CPU seconds in %.1f s (%.1f user, %.1f system)\\n\", v[\"usage_usec\"]/1e6, t1-t0, v[\"user_usec\"]/1e6, v[\"system_usec\"]/1e6 }" "$cg/cpu.stat"
    ' 2>&1 | tee -a "$LOG" | grep -E '^== |tokens per second|corsia:|load time|guessed|swapped|^heat:' || true
  if [ -n "$sampler" ]; then
    kill "$sampler" 2>/dev/null || true; wait "$sampler" 2>/dev/null || true
    awk 'NF { s += $1; n++; if ($1 > m) m = $1 } END { if (n) printf "heat: package peak %.0f C, mean %.0f C\n", m/1000, s/n/1000 }' "$temps" | tee -a "$LOG"
  fi
}

describe() {
  case "$1" in
    ssd) echo "stock llama.cpp, the file memory-mapped" ;;
    mount) echo "Corsia's mount, blind mode" ;;
    stream) echo "the mount plus demand prefetch of each layer's experts" ;;
    predict) echo "demand prefetch plus the next-layer guess" ;;
    swap) echo "the guess plus swapping missing experts for close runners-up in memory" ;;
    *) echo "unknown arm" ;;
  esac
}

echo "== Cap ${CAP} GiB, model ${model_gib} GiB, ${TOKENS} tokens${LLAMA_ENV:+, $LLAMA_ENV}${CPU_QUOTA:+, CPU quota $CPU_QUOTA}" | tee -a "$LOG"
echo "== llama.cpp: $LLAMA_ARGS" | tee -a "$LOG"
done_arms=0; best=""; best_tok=0; missing=""
for a in "${ARM_LIST[@]}"; do
  prog log msg="$a: $(describe "$a"), $TOKENS tokens" current="$a"
  from=$(wc -l < "$LOG"); t0=$(date +%s.%N)
  arm "$a"
  secs=$(python3 -c "print(round($(date +%s.%N) - $t0, 1))")
  metrics=(); while IFS= read -r m; do metrics+=("$m"); done < <(prog_llama_metrics "$LOG" "$from")
  tok=""; for m in "${metrics[@]}"; do case "$m" in metrics.tok/s=*) tok="${m#*=}" ;; esac; done
  if [ -z "$tok" ]; then
    why=$(tail -n +"$((from + 1))" "$LOG" | grep -E 'corsia: (did not start|stopped early)|Killed|error|failed' | tail -1 | cut -c1-160 || true)
    prog log msg="$a: no result${why:+ ($why)}; full output in $LOG"
    missing="${missing:+$missing }$a"
  elif python3 -c "import sys; sys.exit(0 if $tok > $best_tok else 1)"; then
    best="$a"; best_tok="$tok"
  fi
  done_arms=$((done_arms + 1))
  prog step done="$done_arms" item="$a" "${metrics[@]}" "metrics.time s=$secs"
done
PROG_ENDED=1
prog end status=done summary.best="${best:+$best, $best_tok tok/s}" summary.no\ result="$missing" summary.log="$LOG"
echo "Done. Compare the 'eval time ... tokens per second' lines above (full output in $LOG)."
