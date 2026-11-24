#!/usr/bin/env bash
# Agent mode test: how long a coding assistant waits on a local model, without and with
# `corsia agent` in front of llama-server, replaying a real assistant's requests.
#
# Usage:  integrations/llama.cpp/agent-bench.sh MODEL.gguf CAPTURE_DIR [CAP_GIB [COPY_DIR ...]]
#   CAPTURE_DIR: requests saved by capture-agent-request.py. The biggest is the main
#     request (system prompt, tools, first user message); the smallest is a side request
#     (a title). From the main one the script makes a follow-up in the same session and a
#     new session in another folder on another day (only the changing lines differ).
#   CAP_GIB: run everything inside a systemd scope with this memory cap and serve the model
#     through Corsia's mount with expert prefetch (the streaming setup). Without it the
#     model is memory-mapped straight from MODEL.gguf.
#
# Arms (ARMS, default "plain agent"):
#   plain    stock llama-server, the client talks to it directly (Corsia only measures,
#            `corsia agent --plain`)
#   agent    `corsia agent` with stable prefix, two slots and KV snapshots
#     Both replay: side, main, follow-up, new session, then restart llama-server and replay
#     the new session again (the snapshot is dropped from the page cache first, so its
#     restore reads the disk).
#   ppN      prompt processing of the first PP_TOKENS tokens of the main request with
#            -b/-ub N (for example pp256 pp2048 pp4096), and the memory it took
#
# Env: LLAMA_ARGS (common llama-server flags), PLAIN_ARGS / AGENT_ARGS (extra per arm),
# N_PREDICT (tokens generated per request, default 16), PP_TOKENS (default 2048),
# STEPS (which requests to replay, default "side main followup newsession restart"),
# LLAMA_ENV (extra KEY=VALUE settings for the engine, e.g. "CORSIA_POOL_MIB=3584"),
# SKIP_BUILD=1, WORK (default ~/.cache/corsia-build), FORCE=1 on machines with less than
# 24 GiB of RAM when CAP_GIB is set. Progress goes to `corsia watch`.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CORSIA="$(cd "$HERE/../.." && pwd)"
WORK="${WORK:-$HOME/.cache/corsia-build}"
JOBS="${JOBS:-$(( $(nproc) > 1 ? $(nproc) / 2 : 1 ))}"
N_PREDICT="${N_PREDICT:-16}"
PP_TOKENS="${PP_TOKENS:-2048}"
PL="${PL:-8093}"   # llama-server
PA="${PA:-7717}"   # corsia agent
PC="${PC:-7718}"   # corsia serve's API
LLAMA_ARGS="${LLAMA_ARGS:--c 32768 -fa on -ctk q8_0 -ctv q8_0 -t $(nproc) --no-warmup --jinja}"

# shellcheck source=../progress.sh
. "$HERE/../progress.sh"
die() { echo "agent-bench: $*" >&2; prog log msg="$*"; exit 1; }

# ---- inside one arm (in the scope when capped) ----
if [ "${1:-}" = --arm ]; then
  ARM="$2"
  cleanup() { for p in ${LP:-} ${AP:-} ${CP:-}; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; [ -z "${CAP:-}" ] || fusermount3 -uz "$MNT" 2>/dev/null || true; }
  trap cleanup EXIT
  model="$MODEL"
  if [ -n "${CAP:-}" ]; then
    copy_args=(); for d in $COPIES; do copy_args+=(--copy-to "$d"); done
    # A run that was killed leaves a dead mount behind ("Transport endpoint is not connected").
    fusermount3 -uz "$MNT" 2>/dev/null || true
    "$CORSIA_BIN" serve "$MODEL" --mount "$MNT" --port "$PC" --cache-mib 16 "${copy_args[@]}" > "$RUN_DIR/serve-$ARM.log" 2>&1 &
    CP=$!
    until grep -qE "Press Ctrl\+C|corsia:" "$RUN_DIR/serve-$ARM.log"; do kill -0 "$CP" 2>/dev/null || break; sleep 0.5; done
    grep -q "Press Ctrl+C" "$RUN_DIR/serve-$ARM.log" || { echo "corsia did not start:"; cat "$RUN_DIR/serve-$ARM.log"; exit 1; }
    until curl -s "http://127.0.0.1:$PC/api/status" | python3 -c "import sys,json; sys.exit(0 if json.load(sys.stdin)['copier']['state'] in ('done','no copies') else 1)"; do sleep 5; done
    model="$MNT/$(basename "$MODEL")"
    export CORSIA_SOCKET="$(sed -n "s/^  Engines that speak Corsia: \(.*\)  (hinted.*/\1/p" "$RUN_DIR/serve-$ARM.log")"
  fi
  LN=0
  start_llama() {
    LN=$((LN + 1))
    # shellcheck disable=SC2086
    nice -n 5 "$SERVER" -m "$model" $LLAMA_ARGS "$@" --port "$PL" > "$RUN_DIR/llama-$ARM-$LN.log" 2>&1 &
    LP=$!
    until curl -sf "http://127.0.0.1:$PL/health" >/dev/null; do
      kill -0 "$LP" 2>/dev/null || { echo "llama-server stopped:"; tail -5 "$RUN_DIR/llama-$ARM-$LN.log"; exit 1; }
      sleep 1
    done
    # "corsia: streaming experts", not just "streaming experts": the failure line reads
    # "corsia: not streaming experts (...)". A capped run without streaming measures stock llama.cpp.
    if [ -n "${CAP:-}" ] && { [ -z "${CORSIA_SOCKET:-}" ] || ! grep -q "corsia: streaming experts" "$RUN_DIR/llama-$ARM-$LN.log"; }; then
      echo "llama-server is not streaming experts through Corsia (socket '${CORSIA_SOCKET:-}'):"
      grep "corsia:" "$RUN_DIR/llama-$ARM-$LN.log" | head -3
      exit 1
    fi
  }
  stop_llama() { kill "$LP"; wait "$LP" 2>/dev/null || true; LP=""; }
  peak() { local cg; cg="/sys/fs/cgroup$(cut -d: -f3- /proc/self/cgroup | tail -1)"; cat "$cg/memory.peak" 2>/dev/null || echo 0; }
  send() { # LABEL REQUEST
    local before=0 i
    [ -f "$RUN_DIR/$ARM.jsonl" ] && before=$(wc -l < "$RUN_DIR/$ARM.jsonl")
    curl -sN -o /dev/null --max-time 86400 "http://127.0.0.1:$PA/v1/chat/completions" -H 'content-type: application/json' --data-binary @"$RUN_DIR/req/$2.json"
    # Corsia writes its record after the answer ends (and after saving a snapshot): wait for it.
    for i in $(seq 600); do
      [ -f "$RUN_DIR/$ARM.jsonl" ] && [ "$(wc -l < "$RUN_DIR/$ARM.jsonl")" -gt "$before" ] && break
      sleep 1
    done
    python3 - "$RUN_DIR/$ARM.jsonl" "$1" "$RUN_DIR/results.jsonl" "$ARM" <<'PY'
import json, sys
lines = open(sys.argv[1]).read().splitlines()
r = json.loads(lines[-1]); r["label"] = sys.argv[2]; r["arm"] = sys.argv[4]
open(sys.argv[3], "a").write(json.dumps(r) + "\n")
print(f"   {sys.argv[2]:<22} prompt {r['prompt_tokens']:>6}  reused {r['cache_n']:>6}  computed {r['prompt_n']:>6} in {r['prompt_ms']/1000:8.1f} s  wall {r['wall_ms']/1000:8.1f} s" + (f"  restore {r['restore']['ms']/1000:.1f} s" if r.get('restore') else "") + (f"  saved {r['save']['bytes']/1e6:.0f} MB in {r['save']['ms']/1000:.1f} s" if r.get('save') else ""))
PY
  }
  case "$ARM" in
    plain | agent)
      snap="$RUN_DIR/kv-$ARM"; mkdir -p "$snap"
      if [ "$ARM" = agent ]; then
        # shellcheck disable=SC2206
        extra=(-np 2 --kv-unified --slot-save-path "$snap" -cram 0 ${AGENT_ARGS:-})
        flags=(--snapshots "$snap")
      else
        # shellcheck disable=SC2206
        extra=(${PLAIN_ARGS:-})
        flags=(--plain)
      fi
      "$CORSIA_BIN" agent --upstream "127.0.0.1:$PL" --port "$PA" --log "$RUN_DIR/$ARM.jsonl" "${flags[@]}" > "$RUN_DIR/agent-$ARM.log" 2>&1 &
      AP=$!
      start_llama "${extra[@]}"
      step() { case " $STEPS " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
      step side && send "side request" side
      step main && send "main request" main
      step followup && send "follow-up" followup
      step newsession && send "new session" newsession
      step restart || { echo "   memory peak $(( $(peak) / 1048576 )) MiB"; exit 0; }
      stop_llama
      python3 -c 'import os,sys
for d in sys.argv[1:]:
    for f in os.listdir(d):
        fd = os.open(os.path.join(d, f), os.O_RDONLY); os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED); os.close(fd)' "$snap"
      start_llama "${extra[@]}"
      send "new session, restarted" newsession
      echo "   memory peak $(( $(peak) / 1048576 )) MiB"
      ;;
    pp*)
      n="${ARM#pp}"
      start_llama -b "$n" -ub "$n"
      python3 - "$PL" "$RUN_DIR" "$PP_TOKENS" "$n" "$(peak)" <<'PY'
import json, sys, time, urllib.request
port, run, want, n = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
def post(path, body):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", json.dumps(body).encode(), {"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=86400))
main = json.load(open(f"{run}/req/main.json"))
text = post("/apply-template", {"messages": main["messages"], "tools": main.get("tools", [])})["prompt"]
toks = post("/tokenize", {"content": text})["tokens"][:want]
t0 = time.time()
r = post("/completion", {"prompt": toks, "n_predict": 1, "cache_prompt": False})
t = r["timings"]
rec = {"arm": f"pp{n}", "batch": n, "tokens": t["prompt_n"], "prompt_ms": t["prompt_ms"], "wall_ms": (time.time() - t0) * 1000}
open(f"{run}/pp.jsonl", "a").write(json.dumps(rec) + "\n")
print(f"   batch {n:>5}: {t['prompt_n']} tokens in {t['prompt_ms']/1000:.1f} s ({t['prompt_n']*1000/t['prompt_ms']:.2f} tok/s)")
PY
      pk=$(( $(peak) / 1048576 ))
      python3 -c 'import json,sys
p=sys.argv[1]; L=open(p).read().splitlines(); r=json.loads(L[-1]); r["peak_mib"]=int(sys.argv[2]); L[-1]=json.dumps(r); open(p,"w").write("\n".join(L)+"\n")' "$RUN_DIR/pp.jsonl" "$pk"
      echo "   memory peak $pk MiB"
      ;;
    *) echo "unknown arm $ARM"; exit 1 ;;
  esac
  exit 0
fi

# ---- driver ----
[ $# -ge 2 ] || die "usage: $0 MODEL.gguf CAPTURE_DIR [CAP_GIB [COPY_DIR ...]]"
MODEL="$(realpath "$1")"; CAPTURE="$(realpath "$2")"; shift 2
CAP="${1:-}"; [ $# -gt 0 ] && shift
COPIES="${*:-}"
[ -f "$MODEL" ] || die "no model at $MODEL"
ls "$CAPTURE"/req-*.json >/dev/null 2>&1 || die "no req-*.json in $CAPTURE (capture with capture-agent-request.py)"
if [ -n "$CAP" ]; then
  ram_gib=$(( $(awk '/MemTotal/ {print $2}' /proc/meminfo) / 1048576 ))
  [ "$ram_gib" -ge 24 ] || [ "${FORCE:-0}" = 1 ] || die "only ${ram_gib} GiB of RAM (FORCE=1 to override)"
  command -v systemd-run >/dev/null || die "install systemd first"
fi
for t in cargo curl python3; do command -v "$t" >/dev/null || die "install $t first"; done

if [ "${SKIP_BUILD:-0}" != 1 ]; then
  (cd "$CORSIA" && cargo build --release -j "$JOBS" 2>&1 | tail -1)
  (cd "$WORK/llama.cpp" && cmake -B build -DLLAMA_BUILD_SERVER=ON -DLLAMA_USE_PREBUILT_UI=OFF -DLLAMA_BUILD_UI=OFF >/dev/null && cmake --build build --target llama-server -j "$JOBS" 2>&1 | tail -1)
fi
CORSIA_BIN="$CORSIA/target/release/corsia"
SERVER="$WORK/llama.cpp/build/bin/llama-server"
[ -x "$SERVER" ] || die "no llama-server at $SERVER; run without SKIP_BUILD=1 (needs the Corsia llama.cpp fork in $WORK/llama.cpp)"

RUN="agent-$(date +%Y%m%d-%H%M%S)"
RUN_DIR="$WORK/$RUN"; mkdir -p "$RUN_DIR/req"
MNT="$WORK/mnt"; mkdir -p "$MNT"
echo "Results go to $RUN_DIR"

# The replayed requests, derived from the capture.
python3 - "$CAPTURE" "$RUN_DIR/req" "$N_PREDICT" <<'PY'
import copy, glob, json, os, sys
cap, out, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
reqs = []
for f in glob.glob(os.path.join(cap, "req-*.json")):
    try:
        r = json.load(open(f))
    except ValueError:
        continue
    if r.get("messages"):
        reqs.append((os.path.getsize(f), f, r))
reqs.sort()
main, side = reqs[-1][2], reqs[0][2]
def fix(r):
    r = copy.deepcopy(r); r["max_tokens"] = n; r["stream"] = True; r.pop("max_completion_tokens", None)
    return r
def other(line):
    # Another folder, day and branch, only where the line says one.
    label, sep, value = line.partition(":")
    l = label.lower()
    if not sep or len(label) > 48:
        return line
    if value.strip().startswith("/") and any(w in l for w in ("directory", "folder", "workspace", "cwd")):
        return f"{label}: /home/user/projects/other-app"
    if "date" in l or "today" in l:
        return f"{label}: Mon Nov 23 2026"
    if "branch" in l:
        return f"{label}: fix/login"
    return line
newsession = fix(main)
for m in newsession["messages"]:
    if m["role"] in ("system", "developer") and isinstance(m["content"], str):
        m["content"] = "\n".join(other(x) for x in m["content"].split("\n"))
users = [m for m in newsession["messages"] if m["role"] == "user"]
if users:
    users[-1]["content"] = "Where is the entry point of this app, and what does it start?"
follow = fix(main)
follow["messages"] += [{"role": "assistant", "content": "I listed the files and read the module you asked about."},
                       {"role": "user", "content": "Good. Now explain how the second module uses it."}]
for name, r in (("main", fix(main)), ("side", fix(side)), ("followup", follow), ("newsession", newsession)):
    json.dump(r, open(os.path.join(out, name + ".json"), "w"))
changed = sum(a != b for a, b in zip(main["messages"][0]["content"].split("\n"), newsession["messages"][0]["content"].split("\n")))
print(f"Replaying: main request {reqs[-1][0]:,} bytes ({len(main.get('tools', []))} tools), side request {reqs[0][0]:,} bytes; the new session changes {changed} system prompt line(s)")
PY

read -r -a ARM_LIST <<< "${ARMS:-plain agent}"
prog_open "$RUN"
prog start title="Agent mode: $(basename "$MODEL" .gguf)${CAP:+, cap $CAP GiB}" kind="agent test" unit=arms total="${#ARM_LIST[@]}" pid=$$ \
  headline.metric="s" headline.label="New session wait" headline.better=lower meta.model="$(basename "$MODEL")" meta.cap="${CAP:+$CAP GiB}" meta.host="$(hostname -s)" meta.dir="$RUN_DIR"
PROG_ENDED=0
trap 'rc=$?; [ "$PROG_ENDED" = 1 ] || prog end status=failed summary.exit="$rc" summary.dir="$RUN_DIR"' EXIT

STEPS="${STEPS:-side main followup newsession restart}"
LLAMA_ENV="${LLAMA_ENV:-}"
export MODEL RUN_DIR MNT CORSIA_BIN SERVER LLAMA_ARGS PLAIN_ARGS="${PLAIN_ARGS:-}" AGENT_ARGS="${AGENT_ARGS:-}" PL PA PC COPIES CAP PP_TOKENS STEPS
done_arms=0
for a in "${ARM_LIST[@]}"; do
  echo "== $a"
  prog log msg="$a" current="$a"
  python3 -c 'import os,sys; fd=os.open(sys.argv[1],os.O_RDONLY); os.posix_fadvise(fd,0,0,os.POSIX_FADV_DONTNEED)' "$MODEL"
  if [ -n "$CAP" ]; then
    systemd-run --user --scope --quiet --expand-environment=no -p MemoryMax="${CAP}G" -p MemorySwapMax=0 \
      env $LLAMA_ENV MODEL="$MODEL" RUN_DIR="$RUN_DIR" MNT="$MNT" CORSIA_BIN="$CORSIA_BIN" SERVER="$SERVER" LLAMA_ARGS="$LLAMA_ARGS" \
          PLAIN_ARGS="$PLAIN_ARGS" AGENT_ARGS="$AGENT_ARGS" PL="$PL" PA="$PA" PC="$PC" COPIES="$COPIES" CAP="$CAP" PP_TOKENS="$PP_TOKENS" STEPS="$STEPS" \
      bash "$0" --arm "$a" 2>&1 | tee -a "$RUN_DIR/run.log" || true
  else
    env $LLAMA_ENV bash "$0" --arm "$a" 2>&1 | tee -a "$RUN_DIR/run.log" || true
  fi
  done_arms=$((done_arms + 1))
  wait_s=$(python3 -c 'import json,sys
rs=[json.loads(l) for l in open(sys.argv[1])] if __import__("os").path.exists(sys.argv[1]) else []
rs=[r for r in rs if r["arm"]==sys.argv[2] and r["label"]=="new session"]
print(round(rs[-1]["wall_ms"]/1000,1) if rs else "")' "$RUN_DIR/results.jsonl" "$a")
  prog step done="$done_arms" item="$a" "metrics.s=$wait_s"
done

# Summary table.
python3 - "$RUN_DIR" <<'PY'
import json, os, sys
d = sys.argv[1]
p = os.path.join(d, "results.jsonl")
if os.path.exists(p):
    print("\n| Arm | Request | Prompt tokens | Reused | Computed | Prompt time | Restore | Client waited |")
    print("|---|---|---|---|---|---|---|---|")
    for l in open(p):
        r = json.loads(l)
        rs = f"{r['restore']['ms']/1000:.1f} s" if r.get("restore") else ""
        print(f"| {r['arm']} | {r['label']} | {r['prompt_tokens']:,} | {r['cache_n']:,} | {r['prompt_n']:,} | {r['prompt_ms']/1000:.1f} s | {rs} | {r['wall_ms']/1000:.1f} s |")
p = os.path.join(d, "pp.jsonl")
if os.path.exists(p):
    print("\n| Batch (-b/-ub) | Tokens | Prompt time | Tokens per second | Memory peak |")
    print("|---|---|---|---|---|")
    for l in open(p):
        r = json.loads(l)
        print(f"| {r['batch']} | {r['tokens']:,} | {r['prompt_ms']/1000:.1f} s | {r['tokens']*1000/r['prompt_ms']:.2f} | {r.get('peak_mib', 0):,} MiB |")
PY
PROG_ENDED=1
prog end status=done summary.dir="$RUN_DIR"
echo "Done. Everything is in $RUN_DIR"
