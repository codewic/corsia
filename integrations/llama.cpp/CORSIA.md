# llama.cpp, Corsia branch

This is llama.cpp with the changes Corsia needs to run mixture-of-experts models that are
bigger than the memory they get. It follows upstream releases: each change is one small
commit on top of a release tag, so the branch moves to a newer tag by replaying those
commits.

Without `CORSIA_SOCKET` in the environment, every program here behaves exactly like
upstream llama.cpp.

## What the branch adds

| Change | Where | What it does |
| --- | --- | --- |
| Load through Corsia | `src/corsia-client.h`, `src/llama-corsia.h`, `src/llama-model-loader.cpp` | Reads tensors through Corsia's socket when the model is loaded without memory mapping |
| Stream experts | `common/corsia-experts.h`, `common/common.cpp` | As soon as a layer's router picks its experts, asks Corsia for exactly those slices, in one request |
| Guess ahead | `common/corsia-experts.h` | Runs the hidden state through the next layer's norm and router to guess its experts, and fetches them in the background while this layer computes |
| Routing traces | `common/corsia-experts.h` | `CORSIA_TRACE=path` records which experts every token used, for `corsia replay` |
| Cool waiting | `common/corsia-threads.h`, `common/arg.cpp` | While streaming, compute threads waiting for experts give up the core quickly instead of spinning at full power |
| Complete trace lines | `common/corsia-experts.h` | A trace line is written only when every layer recorded its experts |
| Expert pointer table | `ggml/src/ggml-cpu/ggml-cpu.c`, `tiled/tiled.cpp`, `ggml-cpu.cpp` | The expert matmul can take each expert's weights from a pointer table (`ggml_backend_cpu_set_expert_ptrs`), so experts can live outside their tensor |
| Expert pool | `common/corsia-pool.h` | Experts live in memory the engine owns, read by Corsia straight from its drives (no page cache, no faults, no reclaim), least recently used evicted, warm-started from a per-model usage profile |
| Project-wide drafts | `common/corsia-corpus.h`, `common/speculative.cpp` | When `ngram-simple` finds nothing in the context, the same n-gram is looked up in the project's tokens (written by `corsia agent`), with a confidence gate that learns from accepted drafts |
| Drafts earn their length | `common/speculative.cpp` | n-gram drafts start at 8 tokens, double after a fully accepted one and shrink after a rejection, so thinking text no longer pays for long wrong drafts (`CORSIA_DRAFT_ADAPT=0` keeps the fixed size) |
| Grouped decode attention | `ggml/src/ggml-cpu/ops.cpp` | For one decoded token, each KV head is read once for all the query heads that share it (8 in Qwen3-30B-A3B) instead of once per query head; identical output, 1.5 to 2 times faster attention at 16k to 32k context (`CORSIA_GQA_DECODE=0` turns it off) Also takes 2 to 63 query tokens (draft checks, short tool results), which went to the per-row loop: on Qwen3-30B-A3B after 8,192 tokens, a 16-token batch 3.1 to 8.0 tok/s and a 48-token batch 3.3 to 8.2, output identical; single-token decode at that depth 2.2 to 4.3. |
| Tiled prompt attention over quantized caches | `ggml/src/ggml-cpu/ops.cpp` | With a q8_0 KV cache, prompt attention took the per-row path, where each query row walks the whole cache and converts every V row again; the tiled path now converts each 64-row K and V tile to f32 once for 64 query rows. Qwen3-30B-A3B on the i7-6700: 512 tokens after 4,096 of context 6.9 to 21.5 tok/s, a 2,048-token prompt 19.9 to 36.0; KL divergence against an f16 cache 0.0028 (old path 0.0029) (`CORSIA_FA_TILED_QUANT=0` turns it off) |
| Op timer | `ggml/src/ggml-cpu/ggml-cpu.c` | `CORSIA_OP_PROFILE=1` times every graph node on the main thread and prints the time per op and weight at exit |
| Built without OpenMP | `scripts/corsia-build.sh` | ggml's own thread pool: 15 to 23% faster decode on the i7-6700 (`OPENMP=ON` for the old build) |
| Tiled grouped attention | `ggml/src/ggml-cpu/ops.cpp` | Batches of 8 to 63 tokens (64 or more token and head rows per KV head) take a tiled product over K and V tiles converted once: 12 to 17 tok/s against 7 to 8 after 8,192 tokens on Qwen3-30B-A3B (`CORSIA_GQA_TILED=0` keeps the row loop) |
| Experts not prefetched | `src/llama-model-loader.cpp` | With `CORSIA_SOCKET` set the expert tensors are mapped as lazy ranges: the pool reads them, so the loader no longer pulls the whole file through the mount a second time (a cold start on a spinning disk: 484 s against about 26 minutes; `CORSIA_PREFETCH_ALL=1` for the old behaviour) |
| Hot weights first | `common/corsia-experts.h`, `common/corsia-pool.h` | The weights every token reads whole (all but the experts and the token embeddings) are read before the pool fills (`CORSIA_FIRST=0` turns it off), and `CORSIA_WARM_TIERS=N` warms the pool in N tiers of use. On a machine with uneven memory, where `membw guard` holds the slow part during the load, what loads first gets the fast part; `CORSIA_GUARD_MIB` tells the pool sizer how much the guard holds |
| One K row for all its query heads | `ggml/src/ggml-cpu/ops.cpp` | In grouped decode over a q8_0 cache each K row's blocks are loaded, made absolute and their scales converted once for all the query heads that share it, with ggml_vec_dot_q8_0_q8_0's own arithmetic in the same order: the same text, between nothing and +7% decode at 7,000 tokens of context on a busy machine (AVX2 with FMA; `CORSIA_Q8_SCORES=0` turns it off) |
| Two-stage output layer | `ggml/src/ggml-cpu/ggml-cpu.c` | A first pass over the high 2 bits of a Q6_K output matrix, kept in an array of their own (about 100 MB), then the best rows scored exactly; the others keep their preview value. On an i7-6700 the output layer went from 16.1 to 17.1 ms a token to 7.4 to 7.9, decode from 8.2 to 8.8 tok/s to 9.6 to 10.2. Quality against the exact layer: KL 0.00001 to 0.00003 at 4,096 rows, 0.000002 at 16,384, the same top token every time. On by default with AVX2 (8,192 rows); `CORSIA_HEAD_PREVIEW=N` sets the rows, `0` turns it off |
| Output layer first pass, shuffled scales | `ggml/src/ggml-cpu/ggml-cpu.c` | The first pass takes each block's 16 scales from one load and byte shuffles (as ggml's own Q6_K kernel does) instead of 16 single broadcasts, which had made it arithmetic-bound (12 to 13.5 GB/s against 20 to 24 for the other matmuls). The same integer sums in the same order, so the same output to the bit (64 tokens of greedy text identical). On a Xeon E3-1505M v5 with the desktop in use: 11.8 to 10.1 ms a token for the output layer over 3 rounds. `CORSIA_HEAD_FAST=0` uses the old loop |
| KV cache hold (off) | `common/corsia-experts.h` | `CORSIA_KV_FIRST=1`: takes the KV cache's size right after the hot weights and gives it back when the pool is warm, so with a load guard the cache lands in fast memory. Off until measured |
| Pool sized from memory | `common/corsia-experts.h` | On by default: the pool takes what the memory cap (or the free memory) leaves after the other weights, the KV cache and the program |
| Residency-aware routing | `common/corsia-experts.h` | A chosen expert that is not in memory is swapped for a close runner-up that is, before the weights are gathered |
| Confidence-based skipping | `common/corsia-experts.h` | Experts under a share of the layer's weight are skipped (off: it moved the output too much at useful thresholds) |
| Build script | `scripts/corsia-build.sh` | The release build Corsia uses |

## Settings

| Variable | Default | Meaning |
| --- | --- | --- |
| `CORSIA_SOCKET` | unset | Corsia's hinted socket (`corsia serve` prints it). Turns everything on |
| `CORSIA_PREDICT` | `1` | `0` turns guessing ahead off |
| `CORSIA_PREDICT_EXTRA` | `0` | Guesses beyond the model's own top-k |
| `CORSIA_TRACE` | unset | File to record routing traces in. Works without `CORSIA_SOCKET` |
| `CORSIA_COOL` | on | `0` keeps the OpenMP runtime's default spinning |
| `CORSIA_POOL_MIB` | `auto` | Size of the expert pool in MiB, `auto` to size it from memory, `0` for no pool. `auto` uses the cgroup's memory cap, or the memory free at start less a tenth of the machine (at least 1 GiB) for everything else, then leaves room for the weights that are not experts, the KV cache and 1.1 GiB for the program. The pool turns guessing ahead off and swapping on |
| `CORSIA_POOL_PROFILE` | `~/.cache/corsia/<model>.pool-profile` | Where the pool keeps its usage profile |
| `CORSIA_SWAP` | on with the pool, else off | Residency-aware routing; `0` turns it off. `CORSIA_SWAP_RATIO` (0.5), `CORSIA_SWAP_MAX` (2), `CORSIA_SWAP_DEPTH` (8) tune it |
| `CORSIA_DRAFT_CORPUS` | unset | The project token file `corsia agent` writes (`--corpus`); used by `--spec-type ngram-simple` |
| `CORSIA_SKIP` | off | Skip experts under this share of the layer's weight; `CORSIA_SKIP_MIN` (4) are always kept |

Your own `OMP_WAIT_POLICY` or `GOMP_SPINCOUNT` always wins over cool waiting.

## Running cool

Streaming is limited by the drives, not the processor, so the processor mostly waits.
Three things keep it from turning that waiting into heat:

- **One thread per physical core.** llama.cpp's default `-t` already does this. Don't pass
  `-t $(nproc)`: hyperthreads share a core's arithmetic units, so they add heat to matrix
  work without adding speed.
- **Cool waiting** (above), on by default while streaming.
- **A ceiling, if you want one.** Run inside a systemd scope with `-p CPUQuota=150%`, or
  turn turbo off (`echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo`).

## Measured (dev box, 6 GiB cap, Qwen3-30B-A3B UD-Q2_K_XL)

- Expert pool: output identical to the unchanged model (KL divergence 0.000001, same top token 100%), prompt about twice as fast.
- Pool + swapping: 22% faster than guessing on the page cache in cool conditions (2.29 against 1.87 tok/s), 38% in hot ones (1.93 against 1.40); swapping costs KL divergence 0.012, same top token 97%.
- More memory: 10 GiB against 6 GiB gave 3.8 against 2.7 tok/s (41%) and prompts 2.7 times faster; the automatic size picks what was tuned by hand.
- Skipping under 0.1 of the weight dropped 40% of experts but moved the output to KL divergence 0.148 (84% same top token): not used.

## Building

```bash
scripts/corsia-build.sh
```

## Moving to a newer llama.cpp release

```bash
git fetch origin tag b12345 --depth 1
git rebase --onto b12345 b12283 corsia
scripts/corsia-build.sh
```

Then export the commits as patches into Corsia's `integrations/llama.cpp/` folder
(`git format-patch b12345..corsia`) so people without the fork can apply them.
