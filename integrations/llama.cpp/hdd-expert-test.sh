#!/usr/bin/env bash
# Expert-major copies on a spinning disk, live. The hard disk is the only drive llama.cpp
# waits on: the GGUF itself is copied onto it and served from there, so Corsia's
# soonest-drive choice cannot route around the disk to the SSD.
#
#   original  the GGUF on the hard disk, nothing else: today's layout, three seeks per expert
#   experts   the same, plus an expert-major copy beside it on the same disk
#
# Both run stream-on-40gb.sh's arms (default: stream, demand prefetch) under the same cap,
# from cold caches. While an arm runs, Corsia's status is sampled so the summary shows which
# source served the bytes.
#
# Usage:
#   hdd-expert-test.sh check     disk mounted and writable, room, direct reads (no writes but a 16 MiB test file)
#   hdd-expert-test.sh prepare   copy the GGUF onto the disk and fill the expert copy (no model run)
#   hdd-expert-test.sh run       the layouts one after the other (ROUNDS=2 runs them twice, the second time in reverse)
#
# LAYOUTS picks the arms (default "original experts"); "original-pool" and "experts-pool"
# add the fork's expert pool (CORSIA_POOL_MIB, default 3584, swapping on by default with it).
#
# Environment: WD (mount point, default /run/media/$USER/Skywondrive), MODEL (the GGUF to
# copy, default ~/models/gguf/Qwen3-30B-A3B-UD-Q2_K_XL.gguf), CAP (GiB, default 6), TOKENS
# (default 32), ARMS (default stream), ROUNDS (default 1), TRACE (co-firing order, default
# traces/qwen3-30b-q2-prose.trace), CORSIA_BIN, PORT, WORK.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CORSIA="$(cd "$HERE/../.." && pwd)"
WD="${WD:-/run/media/$USER/Skywondrive}"
SRC="$(realpath "${MODEL:-$HOME/models/gguf/Qwen3-30B-A3B-UD-Q2_K_XL.gguf}")"
DIR="$WD/corsia-hdd-test"
WD_MODEL="$DIR/$(basename "$SRC")"
TRACE="$(realpath "${TRACE:-$CORSIA/traces/qwen3-30b-q2-prose.trace}")"
BIN="${CORSIA_BIN:-$CORSIA/target/release/corsia}"
WORK="${WORK:-$HOME/.cache/corsia-build}"
CAP="${CAP:-6}"
TOKENS="${TOKENS:-32}"
ROUNDS="${ROUNDS:-1}"
PORT="${PORT:-7714}"
EXPERT_ARGS="--copy-layout experts --copy-order $TRACE"

die() { echo "hdd-expert-test: $*" >&2; exit 1; }
gib() { python3 -c "print(f'{$1/2**30:.2f}')"; }

# Drops files (or every file under a folder) from the page cache, so each run starts cold.
evict() {
  python3 - "$@" <<'EOF'
import os, sys
for root in sys.argv[1:]:
    paths = [root] if os.path.isfile(root) else [os.path.join(d, f) for d, _, fs in os.walk(root) for f in fs]
    for p in paths:
        try:
            fd = os.open(p, os.O_RDONLY)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            os.close(fd)
        except OSError:
            pass
EOF
}

check() {
  [ -f "$SRC" ] || die "no model at $SRC"
  [ -x "$BIN" ] || die "no Corsia binary at $BIN (cargo build --release -j 2)"
  if ! mountpoint -q "$WD"; then
    die "$WD is not mounted. If a power cut left the NTFS volume dirty, the owner runs: sudo ntfsfix -d /dev/sdb1, then mounts it again."
  fi
  local fstype opts dev parent rota
  fstype="$(findmnt -no FSTYPE --target "$WD")"
  opts="$(findmnt -no OPTIONS --target "$WD")"
  dev="$(findmnt -no SOURCE --target "$WD")"
  parent="$(lsblk -no PKNAME "$dev" 2>/dev/null | head -1)"
  rota="$(lsblk -ndo ROTA "/dev/${parent:-${dev#/dev/}}" 2>/dev/null | tr -d ' ')"
  echo "Disk: $dev at $WD, $fstype ($opts)"
  case ",$opts," in *,ro,*) die "$WD is mounted read-only" ;; esac
  [ "$rota" = 1 ] || echo "Note: $dev does not report itself as a spinning disk (ROTA=$rota)."
  "$BIN" lanes 2>/dev/null | grep -E "^\s+${parent:-sdb}\b" || true

  mkdir -p "$DIR"
  local size need avail
  size=$(stat -c %s "$SRC")
  need=$(( 2 << 30 ))
  [ -f "$WD_MODEL" ] && [ "$(stat -c %s "$WD_MODEL")" = "$size" ] || need=$(( need + size ))
  [ -f "$DIR/.corsia/$(basename "$SRC")-$size-experts/layout" ] || need=$(( need + size + size / 50 ))
  avail=$(df -B1 --output=avail "$WD" | tail -1 | tr -d ' ')
  echo "Room: $(gib "$avail") GiB free, $(gib "$need") GiB needed"
  [ "$avail" -ge "$need" ] || die "not enough room on $WD"

  # Corsia opens its files with O_DIRECT and falls back to buffered reads (dropping each
  # read's pages afterwards) when the filesystem refuses. A filesystem that accepts the
  # flag but then fails aligned reads would break it, so test exactly that.
  python3 - "$DIR/.direct-test" <<'EOF'
import errno, mmap, os, sys
p = sys.argv[1]
with open(p, "wb") as f:
    f.write(os.urandom(16 << 20))
    f.flush()
    os.fsync(f.fileno())
try:
    try:
        fd = os.open(p, os.O_RDONLY | os.O_DIRECT)
    except OSError as e:
        print(f"Direct reads: refused at open ({errno.errorcode.get(e.errno, e.errno)}); Corsia reads buffered and drops each read's pages")
        sys.exit(0)
    buf = mmap.mmap(-1, 1 << 20)
    try:
        for off in (0, 4096, 1 << 20, (16 << 20) - (1 << 20)):
            n = os.preadv(fd, [buf], off)
            assert n == 1 << 20, n
        print("Direct reads: work (aligned 1 MiB reads at page offsets)")
    except OSError as e:
        print(f"Direct reads: open accepted but reads fail ({errno.errorcode.get(e.errno, e.errno)}). Corsia needs a fix before this test.")
        sys.exit(1)
    finally:
        os.close(fd)
finally:
    os.unlink(p)
EOF
}

prepare() {
  check
  local size
  size=$(stat -c %s "$SRC")
  if [ -f "$WD_MODEL" ] && [ "$(stat -c %s "$WD_MODEL")" = "$size" ]; then
    echo "The model is already on the disk: $WD_MODEL"
  else
    echo "Copying $(basename "$SRC") ($(gib "$size") GiB) onto the disk"
    dd if="$SRC" of="$WD_MODEL.part" bs=16M conv=fsync status=progress
    mv "$WD_MODEL.part" "$WD_MODEL"
  fi
  # Spot check: 64 slices of 1 MiB must match the original.
  python3 - "$SRC" "$WD_MODEL" <<'EOF'
import os, random, sys
a, b = sys.argv[1:]
size = os.path.getsize(a)
assert os.path.getsize(b) == size, "sizes differ"
r = random.Random(7)
with open(a, "rb") as fa, open(b, "rb") as fb:
    for off in [0, size - (1 << 20)] + [r.randrange(0, size - (1 << 20)) for _ in range(62)]:
        fa.seek(off); fb.seek(off)
        assert fa.read(1 << 20) == fb.read(1 << 20), f"differs at {off}"
print("Spot check: 64 slices match the original")
EOF
  evict "$WD_MODEL"

  # Fill the expert copy outside any memory cap (Corsia itself needs little), then stop.
  local mnt="$WORK/mnt-prepare" log="$WORK/prepare-serve.log" cpid state
  mkdir -p "$mnt"
  if mountpoint -q "$mnt"; then fusermount3 -u "$mnt"; fi
  "$BIN" serve "$WD_MODEL" --mount "$mnt" --port "$PORT" --cache-mib 16 --copy-to "$DIR" $EXPERT_ARGS > "$log" 2>&1 &
  cpid=$!
  until grep -qE "Press Ctrl\+C|corsia:" "$log"; do sleep 0.5; done
  grep -q "Press Ctrl+C" "$log" || { cat "$log"; die "Corsia did not start"; }
  sed -n '1,/Press Ctrl+C/p' "$log"
  echo "Filling the expert copy (status every 30 s)"
  while kill -0 "$cpid" 2>/dev/null; do
    state="$(curl -s "http://127.0.0.1:$PORT/api/status" | python3 -c '
import json, sys
s = json.load(sys.stdin)
copies = [":".join(str(x[k]) for k in ("drive", "layout", "chunks")) for x in s["sources"] if x["role"] == "copy"]
print(s["copier"]["state"], " ".join(copies), "%.2f" % (s["copier"]["copied_bytes"] / 2**30))
' 2>/dev/null || echo "starting")"
    echo "  $(date +%H:%M:%S) $state GiB copied"
    case "$state" in done*) break ;; esac
    sleep 30
  done
  kill -INT "$cpid" 2>/dev/null || true
  wait "$cpid" 2>/dev/null || true
  evict "$WD_MODEL" "$DIR/.corsia"
  echo "Prepared. Run: $0 run"
}

# Waits until the CPU package is below COOL_C (default 65) so every arm starts unthrottled;
# gives up after 15 minutes and says so.
cool() {
  local sensor="" t waited=0
  for h in /sys/class/hwmon/hwmon*; do
    case "$(cat "$h/name" 2>/dev/null)" in coretemp|k10temp|zenpower) sensor="$h/temp1_input"; break ;; esac
  done
  [ -n "$sensor" ] || return 0
  while t=$(( $(cat "$sensor") / 1000 )); [ "$t" -ge "${COOL_C:-65}" ]; do
    if [ "$waited" -ge 900 ]; then echo "Still ${t} C after 15 minutes; starting anyway."; return 0; fi
    sleep 10; waited=$(( waited + 10 ))
  done
  echo "CPU at ${t} C after ${waited} s of cooling"
}

# One layout: stream-on-40gb.sh with the given copy folders, Corsia's status sampled meanwhile.
layout_run() {
  local label=$1; shift
  local args=$1; shift
  local llama_env=$1; shift
  cool
  evict "$WD_MODEL" "$DIR/.corsia"
  local status="$WORK/hdd-status-$label.json" sampler out
  : > "$status"
  ( while :; do curl -s "http://127.0.0.1:$PORT/api/status" > "$status.tmp" 2>/dev/null && [ -s "$status.tmp" ] && mv "$status.tmp" "$status"; sleep 5; done ) &
  sampler=$!
  # The sampler must never outlive this script: it looks busy to every test queue.
  trap 'kill "$sampler" 2>/dev/null || true' EXIT
  echo "=== $label ==="
  out="$WORK/hdd-run-$label.out"
  local rc=0
  FORCE=1 SKIP_BUILD=1 TOKENS="$TOKENS" ARMS="${ARMS:-stream}" CORSIA_BIN="$BIN" PORT="$PORT" CORSIA_ARGS="--cache-mib 16 $args" LLAMA_ENV="$llama_env" \
    "$HERE/stream-on-40gb.sh" "$WD_MODEL" "$CAP" "$@" 2>&1 | tee "$out" || rc=$?
  kill "$sampler" 2>/dev/null || true
  wait "$sampler" 2>/dev/null || true
  trap - EXIT
  [ "$rc" = 0 ] || die "$label: stream-on-40gb.sh failed (exit $rc), see $out"
  python3 - "$status" "$label" <<'EOF' || true
import json, sys
try:
    s = json.load(open(sys.argv[1]))
except Exception:
    print(f"{sys.argv[2]}: no status sampled"); sys.exit(0)
for x in s["sources"]:
    print(f"{sys.argv[2]}: {x['role']} ({x['layout']}) on {x['drive']}: {x['bytes_served']/2**30:.2f} GiB in {x['reads']} reads, {x['errors']} errors")
EOF
}

run() {
  mountpoint -q "$WD" || die "$WD is not mounted"
  [ -f "$WD_MODEL" ] || die "the model is not on the disk yet: run $0 prepare"
  [ -f "$DIR/.corsia/$(basename "$SRC")-$(stat -c %s "$SRC")-experts/layout" ] || die "no expert copy yet: run $0 prepare"
  rm -f "$WORK"/hdd-run-*.out "$WORK"/hdd-status-*.json
  local pool="CORSIA_POOL_MIB=${POOL_MIB:-3584}" layouts
  read -r -a layouts <<< "${LAYOUTS:-original experts}"
  for r in $(seq 1 "$ROUNDS"); do
    local order=("${layouts[@]}")
    # Even rounds run the layouts in reverse, so heat and order weigh on each the same.
    if [ $(( r % 2 )) = 0 ]; then
      order=(); for (( k=${#layouts[@]}-1; k>=0; k-- )); do order+=("${layouts[k]}"); done
    fi
    for name in "${order[@]}"; do
      case "$name" in
        original) layout_run "original-$r" "" "" ;;
        experts) layout_run "experts-$r" "$EXPERT_ARGS" "" "$DIR" ;;
        original-pool) layout_run "original-pool-$r" "" "$pool" ;;
        experts-pool) layout_run "experts-pool-$r" "$EXPERT_ARGS" "$pool" "$DIR" ;;
        *) die "unknown layout $name (original, experts, original-pool, experts-pool)" ;;
      esac
    done
  done
  echo "=== Summary (prompt, generation, then the pool's warm-up and hits) ==="
  for f in "$WORK"/hdd-run-*.out; do
    echo "$(basename "$f" .out | sed 's/^hdd-run-//'):"
    grep -E "common_perf_print: +(prompt )?eval time|expert pool warmed|expert pool: |swapped [0-9]+ experts" "$f" | sed 's/.*common_perf_print: */  /; s/.*corsia: /  /' || echo "  no result"
  done
}

case "${1:-}" in
  check) check ;;
  prepare) prepare ;;
  run) run ;;
  *) die "usage: $0 check|prepare|run" ;;
esac
