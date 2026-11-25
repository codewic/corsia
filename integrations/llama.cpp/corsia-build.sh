#!/usr/bin/env bash
# Builds the Corsia branch of llama.cpp the way Corsia runs it: an optimised release build
# for this machine's own CPU (GGML_NATIVE), ggml's own thread pool rather than OpenMP
# (15 to 23% faster decode on the i7-6700, no worse on the i5-2450M), no network code, no tests.
# Builds only the programs Corsia uses, two jobs at a time by default so the machine stays
# usable (JOBS=4 to go faster).
#
#   scripts/corsia-build.sh            llama-completion, llama-server, llama-bench
#   scripts/corsia-build.sh llama-cli  any targets you name instead
# llama-server is built without its browser interface: Corsia uses the API only, and the
# prebuilt interface the build would download no longer matches this llama.cpp version (the
# build then fails on a fresh machine).
#   TESTS=ON also configures the test programs (test-backend-ops, which cpu-tune.sh times formats with)
set -euo pipefail
cd "$(dirname "$0")/.."
JOBS="${JOBS:-2}"
targets=("$@")
[ ${#targets[@]} -gt 0 ] || targets=(llama-completion llama-server llama-bench)
cmake -B build -DCMAKE_BUILD_TYPE=Release -DGGML_NATIVE=ON -DGGML_OPENMP="${OPENMP:-OFF}" \
  -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS="${TESTS:-OFF}" \
  -DLLAMA_BUILD_UI=OFF -DLLAMA_USE_PREBUILT_UI=OFF >/dev/null
for t in "${targets[@]}"; do
  cmake --build build --target "$t" -j "$JOBS" 2>&1 | tail -1
done
echo "Built: ${targets[*]} in build/bin"
