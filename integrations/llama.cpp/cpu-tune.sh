#!/usr/bin/env bash
# Formats chosen for this CPU: a copy of a GGUF model whose tensors are rewritten into the
# weight formats this processor computes fastest, never into a less precise one.
#
# A model is downloaded in one mix of formats and run everywhere, but how fast each format
# computes depends on the CPU's instruction sets. On an AVX-only laptop (i5-2450M) Q3_K took
# 1,350 us per expert matmul and Q4_K 866 us, so the Q4_K copy of a Q2_K_XL model's Q3_K down
# experts made it 11% faster at the same quality (perplexity 8.3454 against 8.3459, KL 0.0085).
#
# 1. Times every format's expert matmul on this CPU (llama.cpp's test-backend-ops, at the
#    shape of a mixture-of-experts layer: 8 of 128 experts, 768 x 2048, one token).
# 2. For each tensor, picks the fastest format with at least the original's bits per weight,
#    at most MAX_GROWTH times its size, and at least MIN_GAIN faster; every other tensor
#    keeps its format and is copied byte for byte.
# 3. Writes the copy with llama-quantize and gives it the original's expert usage profile.
#
# Usage:  integrations/llama.cpp/cpu-tune.sh MODEL.gguf [OUT.gguf]
#   DRY=1           only show what would change
#   MIN_GAIN=1.10   MAX_GROWTH=1.35
# Needs the fork built with test-backend-ops (-DLLAMA_BUILD_TESTS=ON) and llama-quantize.
set -euo pipefail

WORK="${WORK:-$HOME/.cache/corsia-build}"
B="$WORK/llama.cpp/build/bin"
MIN_GAIN="${MIN_GAIN:-1.10}"
MAX_GROWTH="${MAX_GROWTH:-1.35}"
die() { echo "cpu-tune: $*" >&2; exit 1; }
[ $# -ge 1 ] || die "usage: $0 MODEL.gguf [OUT.gguf]"
MODEL="$(realpath "$1")"
OUT="${2:-${MODEL%.gguf}-cpu.gguf}"
[ -f "$MODEL" ] || die "no model at $MODEL"
[ -x "$B/test-backend-ops" ] || die "no test-backend-ops in $B (cmake -B build -DLLAMA_BUILD_TESTS=ON, then build test-backend-ops)"
[ -x "$B/llama-quantize" ] || die "no llama-quantize in $B"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

echo "Timing each format's expert matmul on this CPU ($(lscpu | sed -n 's/^Model name: *//p'))..."
systemd-run --user --scope --quiet -p MemoryMax=4G -p MemorySwapMax=0 \
  "$B/test-backend-ops" perf -b CPU -o MUL_MAT_ID -p 'type_b=f32,n_mats=128,n_used=8,b=0,m=768,n=1,k=2048' 2>&1 \
  | sed -nE 's/.*MUL_MAT_ID\(type_a=([a-zA-Z0-9_]+),.* - +([0-9.]+) us\/run.*/\1 \2/p' > "$TMP/speed.txt"
[ -s "$TMP/speed.txt" ] || die "test-backend-ops gave no timings"

rc=0
python3 -I - "$MODEL" "$TMP/speed.txt" "$TMP/types.txt" "$MIN_GAIN" "$MAX_GROWTH" <<'EOF' || rc=$?
import re, struct, sys
model, speed_file, out_file, min_gain, max_growth = sys.argv[1], sys.argv[2], sys.argv[3], float(sys.argv[4]), float(sys.argv[5])
NAMES = {0:"f32",1:"f16",2:"q4_0",3:"q4_1",6:"q5_0",7:"q5_1",8:"q8_0",10:"q2_K",11:"q3_K",12:"q4_K",13:"q5_K",14:"q6_K",16:"iq2_xxs",17:"iq2_xs",18:"iq3_xxs",19:"iq1_s",20:"iq4_nl",21:"iq3_s",22:"iq2_s",23:"iq4_xs",29:"iq1_m",30:"bf16"}
BPW = {"q2_K":2.625,"q3_K":3.4375,"q4_K":4.5,"q5_K":5.5,"q6_K":6.5625,"q8_0":8.5,"q4_0":4.5,"q4_1":5.0,"q5_0":5.5,"q5_1":6.0,
       "iq4_xs":4.25,"iq4_nl":4.5,"iq2_xs":2.3125,"iq2_xxs":2.0625,"iq2_s":2.5,"iq3_xxs":3.0625,"iq3_s":3.4375,"iq1_s":1.5625,"iq1_m":1.75}
# formats a tuned copy may use: plain block formats that need no importance matrix
TARGETS = ["q4_K", "q5_K", "q6_K", "q8_0", "q4_0", "q3_K", "q2_K"]
us = {}
for line in open(speed_file):
    t, v = line.split()
    us[t] = float(v)
print("  " + ", ".join(f"{t} {us[t]:.0f} us" for t in sorted(us, key=us.get)))
f = open(model, "rb")
rd = lambda fmt: struct.unpack("<" + fmt, f.read(struct.calcsize("<" + fmt)))[0]
rs = lambda: f.read(rd("Q")).decode("utf-8", "replace")
assert f.read(4) == b"GGUF", "not a GGUF file"
rd("I"); nt = rd("Q"); nkv = rd("Q")
SZ = {0:"B",1:"b",2:"H",3:"h",4:"I",5:"i",6:"f",7:"?",10:"Q",11:"q",12:"d"}
def skip(t):
    if t == 8: rs()
    elif t == 9:
        at = rd("I"); n = rd("Q")
        for _ in range(n): skip(at)
    else: rd(SZ[t])
for _ in range(nkv): rs(); skip(rd("I"))
lines, plan = [], {}
for _ in range(nt):
    name = rs(); nd = rd("I"); dims = [rd("Q") for _ in range(nd)]; ty = NAMES.get(rd("I"), "?"); rd("Q")
    if ty in ("f32", "f16", "bf16", "?") or nd < 2:
        continue
    new = ty
    if ty in us and ty in BPW and dims[0] % 256 == 0:
        best = None
        for t in TARGETS:
            if t in us and BPW[t] >= BPW[ty] and BPW[t] <= BPW[ty] * max_growth and us[ty] / us[t] >= min_gain:
                if best is None or us[t] < us[best]:
                    best = t
        new = best or ty
    if new != ty:
        n = 1
        for d in dims: n *= d
        k = (re.sub(r"blk\.\d+\.", "", name), ty, new)
        plan[k] = plan.get(k, [0, 0]); plan[k][0] += 1; plan[k][1] += n
    lines.append(f"^{re.escape(name)}$={new}")
open(out_file, "w").write("\n".join(lines) + "\n")
if not plan:
    print("Nothing to change: every tensor is already in the fastest format at its precision on this CPU.")
    sys.exit(3)
for (kind, a, b), (c, n) in sorted(plan.items(), key=lambda x: -x[1][1]):
    print(f"  {kind}: {c} tensors, {n/1e6:.0f} M weights, {a} -> {b} ({us[a]:.0f} -> {us[b]:.0f} us, {us[a]/us[b]:.2f}x faster, {BPW[b]/BPW[a]:.2f}x the bytes)")
EOF
[ $rc -eq 3 ] && exit 0
[ $rc -eq 0 ] || die "planning failed"
if [ "${DRY:-0}" = 1 ]; then echo "DRY=1: not writing $OUT"; exit 0; fi
echo "Writing $OUT ..."
systemd-run --user --scope --quiet -p MemoryMax=6G -p MemorySwapMax=0 \
  "$B/llama-quantize" --allow-requantize --tensor-type-file "$TMP/types.txt" "$MODEL" "$OUT" Q2_K 2 > "$TMP/quantize.log" 2>&1 \
  || { tail -5 "$TMP/quantize.log"; die "llama-quantize failed"; }
prof="$HOME/.cache/corsia/$(basename "$MODEL").pool-profile"
if [ -f "$prof" ]; then cp "$prof" "$HOME/.cache/corsia/$(basename "$OUT").pool-profile"; fi
echo "Done: $OUT ($(( $(stat -c %s "$OUT") >> 20 )) MiB, was $(( $(stat -c %s "$MODEL") >> 20 )) MiB). Check it with model-kl.sh before relying on it."
