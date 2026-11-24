#!/usr/bin/env bash
# Tries the Corsia llama.cpp patch on the target machine: how long a model takes to load
# reading the file directly, and reading it through Corsia (original plus copies on the
# extra drives you name). Runs each way 3 times, cold, alternating.
#
# Usage:  integrations/llama.cpp/try-on-40gb.sh MODEL.gguf [COPY_DIR ...]
#   COPY_DIR: a folder on another drive for a speed copy (filled before timing starts).
#
# Needs: git, cmake, a C++ compiler, cargo, curl, python3, fusermount3.
# Refuses to run below 24 GiB of RAM (FORCE=1 overrides): it loads the whole model.
# Progress goes to ~/.cache/corsia-build/progress for `corsia watch` (CORSIA_PROGRESS=off
# to skip, or a folder to put it elsewhere).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CORSIA="$(cd "$HERE/../.." && pwd)"
TAG="${LLAMA_TAG:-b12283}"
WORK="${WORK:-$HOME/.cache/corsia-build}"
JOBS="${JOBS:-$(( $(nproc) > 1 ? $(nproc) / 2 : 1 ))}"
RUNS="${RUNS:-3}"
PORT="${PORT:-7714}"

# shellcheck source=../progress.sh
. "$HERE/../progress.sh"
die() { echo "try-on-40gb: $*" >&2; prog log msg="$*"; exit 1; }
[ $# -ge 1 ] || die "usage: $0 MODEL.gguf [COPY_DIR ...]"
MODEL="$(realpath "$1")"; shift
[ -f "$MODEL" ] || die "no model at $MODEL"
ram_gib=$(( $(awk '/MemTotal/ {print $2}' /proc/meminfo) / 1048576 ))
[ "$ram_gib" -ge 24 ] || [ "${FORCE:-0}" = 1 ] || die "only ${ram_gib} GiB of RAM; this loads the whole model (FORCE=1 to override)"
for t in git cmake cargo curl python3 fusermount3; do command -v "$t" >/dev/null || die "install $t first"; done

mkdir -p "$WORK"
RUN="try-$(date +%Y%m%d-%H%M%S)"
LOG="$WORK/$RUN.log"
echo "Results go to $LOG"
prog_open "$RUN"
prog start title="Model load: $(basename "$MODEL" .gguf), direct and through Corsia" kind="load test" unit=loads \
  total=$(( RUNS * 2 )) pid=$$ headline.metric="load s" headline.label="Load time" headline.better=lower \
  meta.model="$(basename "$MODEL")" meta.copies="$#" meta.host="$(hostname -s)" meta.log="$LOG"
PROG_ENDED=0
CORSIA_PID=""
finish() {
  rc=$?
  [ "$PROG_ENDED" = 1 ] || prog end status=failed summary.exit="$rc" summary.log="$LOG"
  if [ -n "$CORSIA_PID" ]; then kill -INT "$CORSIA_PID" 2>/dev/null; wait "$CORSIA_PID" 2>/dev/null || true; fi
}
trap finish EXIT

echo "== Building Corsia"
prog log msg="Building Corsia and llama.cpp" current=build
(cd "$CORSIA" && cargo build --release -j "$JOBS" 2>&1 | tail -1)
CORSIA_BIN="$CORSIA/target/release/corsia"

echo "== Getting llama.cpp $TAG and applying the patches"
if [ ! -d "$WORK/llama.cpp" ]; then
  git clone --depth 1 --branch "$TAG" https://github.com/ggml-org/llama.cpp.git "$WORK/llama.cpp"
fi
cd "$WORK/llama.cpp"
if grep -q predict_next common/corsia-experts.h 2>/dev/null && grep -q corsia_prefetch_async src/corsia-client.h 2>/dev/null; then
  echo "this checkout already has the Corsia changes (fork branch or patched earlier)"
else
  for p in "$HERE"/000*.patch; do git apply "$p"; done
fi
cmake -B build -DCMAKE_BUILD_TYPE=Release -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=OFF >/dev/null
cmake --build build --target llama-completion -j "$JOBS" 2>&1 | tail -1
LLAMA="$WORK/llama.cpp/build/bin/llama-completion"

echo "== Starting Corsia"
prog log msg="Starting Corsia" current="start"
MNT="$WORK/mnt"; mkdir -p "$MNT"
mountpoint -q "$MNT" && fusermount3 -u "$MNT"
copy_args=(); for d in "$@"; do copy_args+=(--copy-to "$d"); done
"$CORSIA_BIN" serve "$MODEL" --mount "$MNT" --port "$PORT" "${copy_args[@]}" > "$WORK/serve.log" 2>&1 &
CORSIA_PID=$!
until grep -qE 'Press Ctrl\+C|corsia:' "$WORK/serve.log"; do sleep 0.5; done
grep -q 'Press Ctrl+C' "$WORK/serve.log" || { cat "$WORK/serve.log"; die "Corsia did not start"; }
SOCKET="$(sed -n 's/^  Engines that speak Corsia: \(.*\)  (hinted.*/\1/p' "$WORK/serve.log")"
cat "$WORK/serve.log"
status() { curl -s "http://127.0.0.1:$PORT/api/status"; }
if [ $# -gt 0 ]; then
  echo "Waiting for the copies to fill (they copy while nothing reads)…"
  prog log msg="Filling the copies on $# drives before timing" current="copies"
  until status | python3 -c 'import sys,json; sys.exit(0 if json.load(sys.stdin)["copier"]["state"] in ("done","no copies") else 1)'; do sleep 5; done
fi

cold() { python3 -c 'import os,sys; fd=os.open(sys.argv[1],os.O_RDONLY); os.posix_fadvise(fd,0,0,os.POSIX_FADV_DONTNEED); os.close(fd)' "$MODEL"; }
load_ms() { grep -m1 'load time' | sed -E 's/.*load time = *([0-9.]+) ms.*/\1/'; }
done_runs=0
run() { # label, then env assignments
  local label=$1; shift
  local item="$label $(( done_runs / 2 + 1 ))"
  prog log msg="$item: loading the model cold" current="$item"
  cold
  local t0 t1 ms wall
  t0=$(date +%s.%N)
  ms=$(env "$@" "$LLAMA" -m "$MODEL" -lm none -p "Hello" -n 1 -no-cnv --no-warmup 2>&1 | tee -a "$LOG" | load_ms) || true
  t1=$(date +%s.%N)
  wall=$(python3 -c "print(round($t1 - $t0, 1))")
  printf '%-8s load %8.0f ms   wall %6.1f s\n' "$label" "${ms:-0}" "$wall" | tee -a "$LOG"
  done_runs=$(( done_runs + 1 ))
  [ -n "$ms" ] || prog log msg="$item: llama.cpp printed no load time; full output in $LOG"
  prog step done="$done_runs" item="$item" "metrics.load s=$(python3 -c "print(round(${ms:-0} / 1000, 2) if '${ms}' else '')")" "metrics.wall s=$wall"
}

echo "== Timing ($RUNS runs each, cold, alternating)" | tee -a "$LOG"
for _ in $(seq "$RUNS"); do
  run direct CORSIA_SOCKET=
  run corsia CORSIA_SOCKET="$SOCKET"
done
if ! grep -q 'reading tensors through Corsia' "$LOG"; then
  echo "WARNING: llama.cpp never said it read through Corsia; check $LOG" | tee -a "$LOG"
  prog log msg="llama.cpp never said it read through Corsia; the corsia rows may be direct reads"
fi
echo "== Corsia's view" | tee -a "$LOG"
status | python3 -c 'import sys,json; d=json.load(sys.stdin); [print("  %-6s %-8s served %d MiB in %d reads, errors %d" % (s["drive"], s["role"], s["bytes_served"] >> 20, s["reads"], s["errors"])) for s in d["sources"]]' | tee -a "$LOG"
PROG_ENDED=1
prog end status=done summary.log="$LOG"
echo "Done. Corsia stops now; copies stay on their drives for next time."
