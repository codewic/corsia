#!/usr/bin/env bash
# Corsia for coding assistants on this machine: `corsia serve` streams the model's experts,
# the Corsia fork of llama-server runs it with the expert pool sized from memory, and
# `corsia agent` puts an OpenAI-compatible endpoint in front that keeps long system prompts
# reusable (stable prefix, slots, KV snapshots on disk). All three run as one systemd user
# service under a memory cap, so the rest of the machine keeps its memory.
#
# Usage:  integrations/llama.cpp/local-agent.sh setup | install | models | use N | start | stop | restart | status | autostart on|off | logs
#   install           puts `corsia` on the PATH and turns on start at login
#   autostart on|off  start Corsia at login with the chosen model and saved settings
#   restart           stop, then start (picks up changed settings)
# Settings: ~/.config/corsia/settings.env (the management page edits it), the environment wins over it
#   models            the models in ~/models, numbered, with their size and whether they fit in memory
#   use N|NAME        switch to one of them (stops the one running); start uses it from then on
#   setup             run once per machine and model: checks the processor, memory and drive,
#                     builds what is missing, writes the copy of the model tuned to this CPU
#                     (TUNE=0 skips it) and saves what it found in ~/.config/corsia/machine.env
#   CAP=auto          memory for everything, in GiB (default: what the model needs, or what is
#                     free less a reserve for the rest of the machine, whichever is smaller)
#   MODEL=path.gguf   the model (default ~/models/gguf/Qwen3-30B-A3B-UD-Q2_K_XL.gguf, or its
#                     -cpu.gguf copy from cpu-tune.sh when there is one)
#   CTX=32768         context shared by the two slots (default 32768)
#   PORT=7720         the endpoint: point the assistant at http://127.0.0.1:PORT/v1
#   DRAFTS=auto       free n-gram drafts, chosen per model: on when the model streams experts from
#                     the drive (+28% on copy-heavy answers at 10 GiB, nothing lost on fresh writing),
#                     off when it fits in memory and the processor is the limit (a rejected draft is
#                     wasted arithmetic: an agent task took 215 s with them, 159 s without); 1 or 0 forces it
#   PROJECT_DRAFTS=0  1 also drafts from the project's files (corsia agent --corpus), experimental
#   THREADS=N         compute threads (default: llama.cpp's own choice, one per physical core;
#                     on a virtual machine try the number of real cores behind it)
#   THINK=0           the model answers without thinking (a coding task: 118 s against 25+ minutes,
#                     tests passing both ways); THINK=1 lets it think
# Watch it with: corsia-watch --url 127.0.0.1:7715 --agent 127.0.0.1:PORT
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CORSIA="$(cd "$HERE/../.." && pwd)"
# Settings saved by the management page (or by hand), one KEY=VALUE a line: CAP, CTX, DRAFTS,
# THINK, THREADS, TUNE. A value given in the environment wins over the file.
SETTINGS="$HOME/.config/corsia/settings.env"
if [ -f "$SETTINGS" ]; then
  while IFS='=' read -r k v; do
    case "$k" in
      CAP|CTX|DRAFTS|THINK|THREADS|TUNE) if [ -z "${!k:-}" ] && [ -n "$v" ]; then printf -v "$k" '%s' "$v"; fi ;;
    esac
  done < <(grep -E '^[A-Z]+=[A-Za-z0-9]+$' "$SETTINGS")
fi
CAP="${CAP:-auto}"
# The models to choose from (`models`, `use`): ~/models, or ~/models/gguf when that is where they are
MODELS_DIR="${MODELS_DIR:-$HOME/models}"
[ -d "$MODELS_DIR" ] || MODELS_DIR="$HOME/models/gguf"
CHOSEN="$HOME/.config/corsia/model"
# the model: MODEL=, else the last one picked with `use`, else the first 30B found
if [ -z "${MODEL:-}" ] && [ -s "$CHOSEN" ] && [ -f "$(cat "$CHOSEN")" ]; then MODEL="$(cat "$CHOSEN")"; fi
MODEL="${MODEL:-$HOME/models/gguf/Qwen3-30B-A3B-UD-Q2_K_XL.gguf}"
BASE_MODEL="$MODEL"
# a copy tuned to this CPU by cpu-tune.sh, when there is one, is used instead
if [ -f "${MODEL%.gguf}-cpu.gguf" ]; then MODEL="${MODEL%.gguf}-cpu.gguf"; fi
FORK="${FORK:-$HOME/.cache/corsia-build/llama.cpp}"
PROFILE="$HOME/.config/corsia/machine.env"
CTX="${CTX:-32768}"
PORT="${PORT:-7720}"
DAEMON_PORT="${DAEMON_PORT:-7715}"
LLAMA_PORT="${LLAMA_PORT:-8094}"
CORSIA_BIN="${CORSIA_BIN:-$CORSIA/target/release/corsia}"
# the build without OpenMP is preferred when present: ggml's own thread pool decodes faster
LLAMA_SERVER="${LLAMA_SERVER:-$HOME/.cache/corsia-build/llama.cpp/build-noomp/bin/llama-server}"
[ -x "$LLAMA_SERVER" ] || LLAMA_SERVER="$HOME/.cache/corsia-build/llama.cpp/build/bin/llama-server"
STATE="$HOME/.cache/corsia/local"
KV="$HOME/.cache/corsia/kv"
MNT="$STATE/mnt"
UNIT=corsia-local
DRAFTS="${DRAFTS:-auto}"
PROJECT_DRAFTS="${PROJECT_DRAFTS:-0}"
THINK="${THINK:-0}"
THREADS="${THREADS:-}"
[ "$THREADS" != auto ] || THREADS=""
AUTOSTART_UNIT="$HOME/.config/systemd/user/corsia.service"
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
  echo "Memory: $(( $(systemctl --user show -p MemoryCurrent --value "$UNIT") / 1048576 )) MiB of $(( $(systemctl --user show -p MemoryMax --value "$UNIT") >> 30 )) GiB"
  echo "Watch: corsia-watch --url 127.0.0.1:$DAEMON_PORT --agent 127.0.0.1:$PORT"
}

gib() { awk -v k="$1" '$1 == k":" { print int($2 / 1048576) }' /proc/meminfo; }

# What the model needs to sit in memory whole: its file, the KV cache and working room.
need_gib() { echo $(( (($(stat -c %s "$MODEL") + 1073741823) >> 30) + 3 )); }

# The cap when none is given: what the model needs, or what is free less a reserve for the
# rest of the machine (a tenth of the memory, at least 2 GiB), whichever is smaller.
auto_cap() {
  local reserve room
  reserve=$(( $(gib MemTotal) / 10 )); [ "$reserve" -ge 2 ] || reserve=2
  room=$(( $(gib MemAvailable) - reserve )); [ "$room" -ge 4 ] || room=4
  if [ "$(need_gib)" -le "$room" ]; then need_gib; else echo "$room"; fi
}

setup() {
  local cpu flags feats="" f cores total avail drive rota tuned="${BASE_MODEL%.gguf}-cpu.gguf" map="" probe gib_probe
  cpu="$(lscpu | sed -n 's/^Model name: *//p')"
  flags=" $(grep -m1 '^flags' /proc/cpuinfo | cut -d: -f2) "
  for f in avx avx2 fma f16c avx512f avx_vnni; do case "$flags" in *" $f "*) feats="$feats $f" ;; esac; done
  cores="$(lscpu -p=core,socket | grep -v '^#' | sort -u | wc -l)"
  total="$(gib MemTotal)"; avail="$(gib MemAvailable)"
  echo "Corsia setup"
  echo "  Processor: $cpu, $cores cores,${feats:- no AVX}"
  echo "  Memory: $total GiB, $avail GiB free now"
  [ -f "$BASE_MODEL" ] || die "no model at $BASE_MODEL (set MODEL=path.gguf)"
  drive="$(df --output=source "$BASE_MODEL" | tail -1)"
  rota="$(lsblk -no ROTA "$drive" 2>/dev/null | head -1 | tr -d ' ')"
  echo "  Model: $BASE_MODEL ($(( $(stat -c %s "$BASE_MODEL") >> 20 )) MiB, on a $([ "$rota" = 1 ] && echo "spinning disk: a cold start reads it at disk speed" || echo "solid-state drive"))"

  echo "1. Programs"
  if [ ! -x "$CORSIA_BIN" ]; then
    command -v cargo >/dev/null || die "no corsia at $CORSIA_BIN and no cargo to build it"
    echo "   building corsia"; (cd "$CORSIA" && cargo build --release --quiet)
  fi
  if [ ! -x "$LLAMA_SERVER" ]; then
    [ -x "$FORK/scripts/corsia-build.sh" ] || die "no Corsia fork of llama.cpp at $FORK (see README.md here for how to get it)"
    echo "   building the Corsia fork of llama.cpp for this processor (a few minutes)"
    TESTS=ON "$FORK/scripts/corsia-build.sh" llama-server llama-quantize test-backend-ops
    LLAMA_SERVER="$FORK/build/bin/llama-server"
  fi
  echo "   corsia and llama-server are in place"

  echo "2. Weights in the formats this processor computes fastest"
  if [ -f "$tuned" ]; then
    echo "   already there: $tuned"
  elif [ "${TUNE:-1}" = 0 ]; then
    echo "   skipped (TUNE=0); later: $HERE/cpu-tune.sh $BASE_MODEL"
  elif ! { [ -x "$FORK/build/bin/test-backend-ops" ] || [ -x "$FORK/build-noomp/bin/test-backend-ops" ]; }; then
    echo "   skipped: the tuner needs two more programs. Build them, then run setup again:"
    echo "   TESTS=ON $FORK/scripts/corsia-build.sh llama-quantize test-backend-ops"
  elif [ "$(df --output=avail -B1G "$(dirname "$tuned")" | tail -1)" -lt $(( ($(stat -c %s "$BASE_MODEL") >> 30) * 14 / 10 + 2 )) ]; then
    echo "   not enough room next to the model for a tuned copy; skipped"
  else
    WORK="$(dirname "$FORK")" "$HERE/cpu-tune.sh" "$BASE_MODEL" "$tuned" | sed 's/^/   /' || echo "   tuning failed; the original is used"
  fi
  [ -f "$tuned" ] && MODEL="$tuned"

  echo "3. Memory speed"
  probe="$STATE/membw"; mkdir -p "$STATE"
  gib_probe=$(( avail - 4 )); [ "$gib_probe" -le 24 ] || gib_probe=24
  if systemctl --user is-active --quiet "$UNIT"; then
    echo "   skipped: Corsia is running (stop it and run setup again to measure)"
  elif [ "$gib_probe" -lt 2 ] || ! command -v cc >/dev/null || [ -z "$feats" ]; then
    echo "   skipped (needs a C compiler, AVX and 6 GiB free)"
  elif cc -O2 -mavx -pthread "$HERE/membw.c" -o "$probe" 2>/dev/null && map="$("$probe" map "$gib_probe" | sed -n 's/^map: //p')" && [ -n "$map" ]; then
    set -- $map
    if awk -v r="$9" 'BEGIN { exit !(r >= 1.25) }'; then
      echo "   uneven: of $1 GiB measured, $3 GiB reads at $4 GB/s and $6 GiB at $7 GB/s"
      echo "   (memory modules of different sizes: only the matched part runs dual-channel)"
    else
      echo "   even: about $4 GB/s across the $1 GiB measured"
    fi
  else
    echo "   could not measure"
  fi

  mkdir -p "$(dirname "$PROFILE")"
  {
    echo "# Corsia machine profile, written by local-agent.sh setup on $(date +%F)"
    echo "CORSIA_CPU=\"$cpu\""
    echo "CORSIA_CPU_FEATURES=\"${feats# }\""
    echo "CORSIA_CORES=$cores"
    echo "CORSIA_MEM_GIB=$total"
    echo "CORSIA_MEM_MAP=\"$map\""
    echo "CORSIA_MODEL=\"$MODEL\""
  } > "$PROFILE"
  echo "4. Memory for the model"
  echo "   it needs about $(need_gib) GiB to sit in memory whole; a start now would get $(auto_cap) GiB"
  [ "$(auto_cap)" -ge "$(need_gib)" ] || echo "   less than it needs: the experts that do not fit stream from the drive (slower). Closing other programs before a start helps."
  echo "Saved in $PROFILE. Start with: $0 start"
}

# Every model is tuned to this processor once, without anyone running setup: after a start,
# cpu-tune.sh times each format at the model's own shape and writes a -cpu.gguf copy when a
# faster format holds at least the same precision; the next start uses that copy. It runs
# at low priority beside the model and posts what it did to the notes. TUNE=0 turns it off.
auto_tune() {
  local base="$BASE_MODEL" name mark free_gib need
  name="$(basename "$base")"
  mark="$HOME/.config/corsia/tuned/$name"
  [ "${TUNE:-1}" != 0 ] || return 0
  [ ! -f "$mark" ] && [ ! -f "${base%.gguf}-cpu.gguf" ] || return 0
  { [ -x "$FORK/build/bin/test-backend-ops" ] || [ -x "$FORK/build-noomp/bin/test-backend-ops" ]; } || return 0
  free_gib="$(df --output=avail -B1G "$(dirname "$base")" | tail -1 | tr -d ' ')"
  need=$(( ($(stat -c %s "$base") >> 30) * 14 / 10 + 2 ))
  [ "$free_gib" -ge "$need" ] || return 0
  mkdir -p "$(dirname "$mark")"
  systemd-run --user --unit "corsia-tune-$(date +%s)" --collect --quiet -p Nice=15 -p IOSchedulingClass=idle \
    --expand-environment=no env BASE="$base" NAME="$name" MARK="$mark" TUNER="$HERE/cpu-tune.sh" WORK="$(dirname "$FORK")" LOG="$STATE/tune.log" bash -c '
      note() { echo "$(date +%H:%M)  $*" >> ~/.cache/corsia/notes.log; }
      echo "tuning $NAME for this processor" > ~/.cache/corsia/progress
      note "Tuning $NAME to this processor in the background (each format timed at its own shape)."
      if "$TUNER" "$BASE" > "$LOG" 2>&1; then
        if [ -f "${BASE%.gguf}-cpu.gguf" ]; then
          note "Tuned copy of $NAME written: $(grep -E "^  [a-z_]+: [0-9]+ tensors" "$LOG" | head -3 | tr -s " " | tr "\n" ";" | cut -c1-220) It is used from the next start."
        else
          note "$NAME is already in the fastest formats for this processor: $(grep -E "^  [a-z0-9_]+ [0-9]+ us" "$LOG" | head -1 | tr -s " " | cut -c1-160)."
        fi
      else
        note "Tuning $NAME failed: $(tail -1 "$LOG" | cut -c1-160)"
      fi
      date +%F > "$MARK"
      : > ~/.cache/corsia/progress
    '
}

# The models in MODELS_DIR, numbered for `use`: one file each (split parts and the -cpu.gguf
# copies the tuner writes are left out; a tuned copy is used on its own when it is there).
model_files() {
  find "$MODELS_DIR" -maxdepth 1 -name '*.gguf' ! -name '*-cpu.gguf' ! -name '*-0000?-of-0000?.gguf' -printf '%f\n' 2>/dev/null | sort
}

models() {
  local i=0 f size need fits mark state running="" total
  total="$(gib MemTotal)"
  systemctl --user is-active --quiet "$UNIT" && running="$(cat "$CHOSEN" 2>/dev/null || true)"
  echo "Models in $MODELS_DIR (switch with: $0 use NUMBER):"
  while IFS= read -r f; do
    i=$((i + 1))
    size=$(( ($(stat -c %s "$MODELS_DIR/$f") + 1073741823) >> 30 ))
    need=$(( size + 3 ))
    if [ "$need" -le $(( total * 9 / 10 )) ]; then fits="fits in memory"; else fits="streams experts from the drive"; fi
    mark="  "; state=""
    if [ "$MODELS_DIR/$f" = "$running" ]; then mark="▸ "; state=", running"; fi
    printf '%s%2d  %-62s %4d GiB  %s%s\n' "$mark" "$i" "$f" "$size" "$fits" "$state"
  done < <(model_files)
  [ "$i" -gt 0 ] || echo "  none (put .gguf files there, or set MODELS_DIR)"
}

# `use 3` or `use coder`: remember the model, stop the one running and start it
use_model() {
  local want="${1:-}" f="" n
  [ -n "$want" ] || die "use NUMBER or part of a name (see: $0 models)"
  if [[ "$want" =~ ^[0-9]+$ ]]; then
    f="$(model_files | sed -n "${want}p")"
  else
    n="$(model_files | grep -ic -- "$want" || true)"
    [ "$n" -le 1 ] || die "\"$want\" matches $n models; be more exact (see: $0 models)"
    f="$(model_files | grep -i -m1 -- "$want" || true)"
  fi
  [ -n "$f" ] || die "no model \"$want\" (see: $0 models)"
  mkdir -p "$(dirname "$CHOSEN")"
  echo "$MODELS_DIR/$f" > "$CHOSEN"
  echo "Switching to $f"
  "$0" stop >/dev/null
  MODEL="$MODELS_DIR/$f" exec "$0" start
}

# Start at login: a user unit that runs `start` when the desktop session begins (and `stop`
# when it ends), with the chosen model and the saved settings. `autostart on|off|status`.
autostart() {
  case "${1:-status}" in
    on)
      mkdir -p "$(dirname "$AUTOSTART_UNIT")"
      cat > "$AUTOSTART_UNIT" <<UNITEOF
[Unit]
Description=Corsia: the local model for coding assistants (chosen model, saved settings)

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=$HERE/local-agent.sh start
ExecStop=$HERE/local-agent.sh stop
TimeoutStartSec=600

[Install]
WantedBy=default.target
UNITEOF
      systemctl --user daemon-reload
      systemctl --user enable --quiet corsia.service
      echo "Corsia starts at login with the chosen model."
      ;;
    off)
      systemctl --user disable --quiet corsia.service 2>/dev/null || true
      echo "Corsia no longer starts at login (start it with: corsia start)."
      ;;
    status)
      if systemctl --user is-enabled --quiet corsia.service 2>/dev/null; then echo "on"; else echo "off"; fi
      ;;
    *) die "autostart on | off | status" ;;
  esac
}

# `corsia` on the PATH (~/.local/bin, which Ubuntu adds at login once it exists)
install_command() {
  mkdir -p "$HOME/.local/bin"
  ln -sf "$CORSIA_BIN" "$HOME/.local/bin/corsia"
  echo "The corsia command is in ~/.local/bin (a new terminal finds it)."
}

case "${1:-status}" in
  models) models ;;
  autostart) autostart "${2:-status}" ;;
  install) install_command; autostart on ;;
  restart) "$0" stop >/dev/null; exec "$0" start ;;
  use) use_model "${2:-}" ;;
  start)
    if systemctl --user is-active --quiet "$UNIT"; then status; exit 0; fi
    [ -f "$MODEL" ] || die "no model at $MODEL"
    [ -x "$CORSIA_BIN" ] || die "no corsia at $CORSIA_BIN (cargo build --release in $CORSIA)"
    [ -x "$LLAMA_SERVER" ] || die "no llama-server at $LLAMA_SERVER (build the Corsia fork, see README.md here)"
    for p in "$PORT" "$DAEMON_PORT" "$LLAMA_PORT"; do
      if ss -ltn "( sport = :$p )" | grep -q LISTEN; then die "port $p is taken; set PORT, DAEMON_PORT or LLAMA_PORT"; fi
    done
    if [ "$CAP" = auto ]; then
      CAP="$(auto_cap)"
      if [ "$CAP" -lt "$(need_gib)" ]; then
        echo "Only $(gib MemAvailable) GiB of memory is free: the cap is $CAP GiB and the model needs about $(need_gib) to sit in memory whole."
        echo "The experts that do not fit stream from the drive, which is slower. Closing other programs first helps."
      fi
    fi
    if [ "$DRAFTS" = auto ]; then
      if [ "$CAP" -ge "$(need_gib)" ]; then DRAFTS=0; else DRAFTS=1; fi
    fi
    # the fork's own switches (CORSIA_...), when set by hand for a test, reach llama-server
    EXTRA=(CORSIA_LOCAL=1)
    for v in CORSIA_COOL CORSIA_OP_PROFILE CORSIA_FIRST CORSIA_GQA_TILED CORSIA_Q8_SCORES CORSIA_HEAD_PREVIEW CORSIA_KV_FIRST CORSIA_REPACK; do
      if [ -n "${!v:-}" ]; then EXTRA+=("$v=${!v}"); fi
    done
    # a mount left behind by a killed run answers "Transport endpoint is not connected":
    # mountpoint cannot see it, so it is always unmounted first
    fusermount3 -uz "$MNT" 2>/dev/null || true
    mkdir -p "$STATE" "$KV" "$MNT"
    systemctl --user reset-failed "$UNIT" 2>/dev/null || true
    # Uneven memory (setup measured it): a guard holds the slow part while the model loads,
    # so the weights read for every token and the most used experts get the fast part. It
    # lets go as memory fills and ends when the endpoint is up. GUARD=0 turns it off. It leaves
    # 4 GiB untouched so other programs are not squeezed while it measures (about 25 s).
    GUARD_MIB=0; TIERS=1
    ratio="$(sed -n 's/^CORSIA_MEM_MAP=".* ratio \([0-9.]*\)"$/\1/p' "$PROFILE" 2>/dev/null || true)"
    if [ "${GUARD:-1}" != 0 ] && [ -x "$STATE/membw" ] && [ -n "$ratio" ] && awk -v r="$ratio" 'BEGIN { exit !(r >= 1.25) }'; then
      rm -f "$STATE/guard.log"
      systemctl --user reset-failed "$UNIT-guard" 2>/dev/null || true
      systemd-run --user --unit "$UNIT-guard" --quiet --collect --expand-environment=no \
        env PROBE="$STATE/membw" G="$(( $(gib MemAvailable) - 4 ))" LOG="$STATE/guard.log" PORT="$PORT" bash -c '
          "$PROBE" guard "$G" > "$LOG" 2>&1 &
          gp=$!
          for _ in $(seq 1800); do
            kill -0 $gp 2>/dev/null || exit 0
            if curl -sf "http://127.0.0.1:$PORT/api/agent" >/dev/null; then break; fi
            sleep 1
          done
          kill $gp 2>/dev/null
        '
      for _ in $(seq 300); do grep -q "^guard:" "$STATE/guard.log" 2>/dev/null && break; sleep 1; done
      if ! grep -q "^guard:" "$STATE/guard.log" 2>/dev/null; then
        # still measuring after 5 minutes: never load beside a guard that holds the memory
        systemctl --user stop "$UNIT-guard" 2>/dev/null || true
        echo "The memory guard took too long to measure and was stopped: loading without it."
      fi
      if grep -q "^guard: holding" "$STATE/guard.log" 2>/dev/null; then
        GUARD_MIB="$(sed -n 's/^guard: holding \([0-9.]*\) GiB.*/\1/p' "$STATE/guard.log" | awk '{ print int($1 * 1024) }')"
        TIERS=4
        echo "Memory is uneven here: $(sed -n 's/^guard: //p' "$STATE/guard.log" | head -1)."
      fi
    fi
    systemd-run --user --unit "$UNIT" --quiet --collect --expand-environment=no \
      -p MemoryMax="${CAP}G" -p MemorySwapMax=0 \
      env MODEL="$MODEL" MNT="$MNT" STATE="$STATE" KV="$KV" CTX="$CTX" PORT="$PORT" DAEMON_PORT="$DAEMON_PORT" \
          LLAMA_PORT="$LLAMA_PORT" CORSIA_BIN="$CORSIA_BIN" LLAMA_SERVER="$LLAMA_SERVER" \
          DRAFTS="$DRAFTS" PROJECT_DRAFTS="$PROJECT_DRAFTS" CORPUS="$CORPUS" THINK="$THINK" \
          CORSIA_GUARD_MIB="$GUARD_MIB" CORSIA_WARM_TIERS="$TIERS" THREADS="$THREADS" "${EXTRA[@]}" \
      bash -c '
        rm -f "$STATE/serve.log"
        "$CORSIA_BIN" serve "$MODEL" --mount "$MNT" --port "$DAEMON_PORT" --cache-mib 16 > "$STATE/serve.log" 2>&1 &
        until grep -qE "Press Ctrl\+C|corsia:" "$STATE/serve.log" 2>/dev/null; do sleep 0.5; done
        sock="$(sed -n "s/^  Engines that speak Corsia: \(.*\)  (hinted.*/\1/p" "$STATE/serve.log")"
        [ -n "$sock" ] || { echo "corsia serve did not start:"; cat "$STATE/serve.log"; exit 1; }
        spec=(); [ "$DRAFTS" = 1 ] && spec=(--spec-type ngram-simple)
        corpus_env=""; corpus_arg=(--corpus off)
        if [ "$PROJECT_DRAFTS" = 1 ]; then corpus_env="$CORPUS"; corpus_arg=(--corpus "$CORPUS"); fi
        think_arg=(--think off); [ "$THINK" = 1 ] && think_arg=(--think on)
        threads_arg=(); [ -n "$THREADS" ] && threads_arg=(-t "$THREADS")
        # the model'"'"'s own name, so the chat page and /v1/models show it; with one model loaded
        # llama-server answers whatever name a client sends, so assistants set to "local" still work
        name="$(basename "$MODEL" .gguf)"; name="${name%-cpu}"
        CORSIA_SOCKET="$sock" CORSIA_DRAFT_CORPUS="$corpus_env" "$LLAMA_SERVER" -m "$MNT/$(basename "$MODEL")" --alias "$name" "${spec[@]}" "${threads_arg[@]}" \
          -c "$CTX" -np 2 --kv-unified -fa on -ctk q8_0 -ctv q8_0 -b 2048 -ub 512 -cram 0 \
          --slot-save-path "$KV" --jinja --no-warmup --port "$LLAMA_PORT" > "$STATE/llama-server.log" 2>&1 &
        until curl -sf "http://127.0.0.1:$LLAMA_PORT/health" >/dev/null; do sleep 2; done
        exec "$CORSIA_BIN" agent --upstream "127.0.0.1:$LLAMA_PORT" --port "$PORT" --snapshots "$KV" "${corpus_arg[@]}" "${think_arg[@]}" > "$STATE/agent.log" 2>&1
      '
    echo "Starting Corsia for coding assistants (${CAP} GiB cap, logs in $STATE)."
    echo "It is ready when '$0 status' says so: about a minute to load and fill the expert pool."
    auto_tune
    ;;
  stop)
    systemctl --user stop "$UNIT-guard" 2>/dev/null || true
    systemctl --user stop "$UNIT" 2>/dev/null || true
    fusermount3 -uz "$MNT" 2>/dev/null || true
    echo "Stopped."
    ;;
  status) status ;;
  setup) setup ;;
  logs) tail -n 20 "$STATE/serve.log" "$STATE/llama-server.log" "$STATE/agent.log" 2>/dev/null ;;
  *) die "usage: $0 setup | install | models | use N | start | stop | restart | status | autostart on|off | logs" ;;
esac
