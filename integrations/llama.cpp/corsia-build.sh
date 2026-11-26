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
#   TESTS=ON   also configures the test programs (test-backend-ops, which cpu-tune.sh times formats with)
#   GPU=cuda   a build that can put the dense layers on an NVIDIA GPU, in build-cuda (needs nvcc);
#              the GPU's own architecture is read from the driver, and when nvcc refuses the
#              system's C++ compiler an older one it accepts (g++-12, g++-11...) is used instead
#   GPU=vulkan the same for any GPU with a Vulkan driver, in build-vk (needs glslc)
set -euo pipefail
cd "$(dirname "$0")/.."
JOBS="${JOBS:-2}"
targets=("$@")
[ ${#targets[@]} -gt 0 ] || targets=(llama-completion llama-server llama-bench)
dir=build; gpu=()
case "${GPU:-}" in
  "") ;;
  cuda)
    command -v nvcc >/dev/null || { echo "GPU=cuda needs nvcc (the CUDA toolkit)" >&2; exit 1; }
    dir=build-cuda
    arch="$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader 2>/dev/null | head -1 | tr -d ' .')"
    gpu=(-DGGML_CUDA=ON -DCMAKE_CUDA_ARCHITECTURES="${arch:-native}")
    probe="$(mktemp -d)"; echo '__global__ void k() {}' > "$probe/t.cu"
    if ! nvcc -c "$probe/t.cu" -o "$probe/t.o" 2>/dev/null; then
      for cxx in g++-14 g++-13 g++-12 g++-11 g++-10; do
        if command -v "$cxx" >/dev/null && nvcc -ccbin "$cxx" -c "$probe/t.cu" -o "$probe/t.o" 2>/dev/null; then
          gpu+=(-DCMAKE_CUDA_HOST_COMPILER="$cxx"); break
        fi
      done
    fi
    rm -rf "$probe"
    ;;
  vulkan)
    command -v glslc >/dev/null || { echo "GPU=vulkan needs glslc (the Vulkan shader compiler)" >&2; exit 1; }
    dir=build-vk; gpu=(-DGGML_VULKAN=ON)
    ;;
  *) echo "GPU is cuda or vulkan" >&2; exit 1 ;;
esac
cmake -B "$dir" -DCMAKE_BUILD_TYPE=Release -DGGML_NATIVE=ON -DGGML_OPENMP="${OPENMP:-OFF}" \
  -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS="${TESTS:-OFF}" \
  -DLLAMA_BUILD_UI=OFF -DLLAMA_USE_PREBUILT_UI=OFF "${gpu[@]}" >/dev/null
for t in "${targets[@]}"; do
  cmake --build "$dir" --target "$t" -j "$JOBS" 2>&1 | tail -1
done
echo "Built: ${targets[*]} in $dir/bin"
