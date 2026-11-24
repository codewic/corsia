# Progress events for `corsia watch`. Sourced by the test scripts in integrations/.
#
# Each run writes one file, $CORSIA_PROGRESS/<run>.jsonl (default
# ~/.cache/corsia-build/progress), one JSON object per line: start, step, log, end.
# Nothing has to be watching: the file is only appended to. CORSIA_PROGRESS=off writes
# nothing. A failed write never stops the script; the event is just lost.
#
#   prog_open RUN_ID                  choose the file (truncates it)
#   prog EVENT key=value ...          append one event
#
# Keys with a dot nest one level (metrics.tok/s=1.47, meta.cap=6 GiB). Whole numbers
# become integers, other numbers floats, the rest strings. Empty values are left out, so a
# metric that was not measured is simply missing. Needs python3 (the scripts need it
# anyway).

PROGRESS_FILE=""

prog_open() {
  [ "${CORSIA_PROGRESS:-}" = off ] && return 0
  local dir="${CORSIA_PROGRESS:-${WORK:-$HOME/.cache/corsia-build}/progress}"
  if mkdir -p "$dir" 2>/dev/null && : > "$dir/$1.jsonl" 2>/dev/null; then
    PROGRESS_FILE="$dir/$1.jsonl"
  fi
}

prog() {
  [ -n "$PROGRESS_FILE" ] || return 0
  python3 - "$@" >> "$PROGRESS_FILE" 2>/dev/null <<'PY' || true
import json, re, sys, time
ev = {"event": sys.argv[1], "t": round(time.time(), 3)}
for arg in sys.argv[2:]:
    key, sep, val = arg.partition("=")
    if not sep or val == "":
        continue
    if re.fullmatch(r"-?\d+", val):
        v = int(val)
    elif re.fullmatch(r"-?\d+\.\d*|-?\.\d+", val):
        v = float(val)
    else:
        v = val
    parent, dot, child = key.partition(".")
    if dot:
        ev.setdefault(parent, {})[child] = v
    else:
        ev[key] = v
print(json.dumps(ev, ensure_ascii=False))
PY
}

# Metrics from one llama-completion run, read from the part of LOG after line START.
# Prints key=value arguments for `prog step`, one per line.
prog_llama_metrics() {
  tail -n +"$(( $2 + 1 ))" "$1" | python3 -c '
import re, sys
text = sys.stdin.read()
def last(pattern):
    m = re.findall(pattern, text)
    return m[-1] if m else None
ev = last(r"(?<!prompt )eval time =\s*[\d.]+ ms /\s*\d+ runs\s*\(\s*([\d.]+) ms per token,\s*([\d.]+) tokens per second")
pp = last(r"prompt eval time =\s*[\d.]+ ms /\s*\d+ tokens\s*\(\s*[\d.]+ ms per token,\s*([\d.]+) tokens per second")
guess = last(r"guessed (\d+) of (\d+) experts")
pre = last(r"(\d+) expert prefetches, (\d+) MiB put in memory")
cpu = last(r"heat: ([\d.]+) CPU seconds in ([\d.]+) s")
peak = last(r"heat: package peak (\d+) C, mean (\d+) C")
if ev:
    print("metrics.tok/s=%s" % ev[1])
    print("metrics.s/token=%.3f" % (float(ev[0]) / 1000))
if pp:
    print("metrics.prompt tok/s=%s" % pp)
if guess and int(guess[1]) > 0:
    print("metrics.hit %%=%.1f" % (100 * int(guess[0]) / int(guess[1])))
if pre:
    print("metrics.prefetch MiB=%s" % pre[1])
sysc = last(r"heat: [\d.]+ CPU seconds in [\d.]+ s \([\d.]+ user, ([\d.]+) system\)")
if cpu:
    print("metrics.CPU s=%s" % cpu[0])
    if sysc:
        print("metrics.system CPU s=%s" % sysc)
    if float(cpu[1]) > 0:
        print("metrics.cores busy=%.2f" % (float(cpu[0]) / float(cpu[1])))
if peak:
    print("metrics.peak C=%s" % peak[0])
    print("metrics.mean C=%s" % peak[1])
' 2>/dev/null || true
}
