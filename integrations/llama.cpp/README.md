# Corsia for llama.cpp

Patches for llama.cpp build b12283, applied in order: `0001` loads models through Corsia, `0002` streams experts for MoE models bigger than RAM, `0003` and `0004` guess the next layer's experts and fetch them ahead, `0005` records routing traces (`0007` keeps out the partial line the prompt pass used to leave), `0006` keeps the processor cool while it waits for experts, `0008` lets ggml's expert matmul take expert pointers from a table, and `0009` adds the expert pool (`CORSIA_POOL_MIB`, experts in memory Corsia fills directly, about twice as fast on prompts and 22 to 38% faster generating together with swapping), residency-aware routing (`CORSIA_SWAP`, on with the pool) and expert skipping (`CORSIA_SKIP`, off). `swap-quality.sh` measures how far a setting moves the output.

The same changes live as commits on the `corsia` branch of our llama.cpp fork. Two files from that branch are here as they are: `CORSIA.md` (every change, every setting, how to move to a newer release) and `corsia-build.sh` (the release build Corsia uses; it goes in the fork's `scripts/`). The patches are exported from the branch without those two files, and the whole set is checked to apply in order with `git apply` on a clean b12283. There is no `0016` or `0024`: those two changes were to the build script alone.

## Running cool

Streaming is limited by the drives, so the processor mostly waits, and by default it waits loudly: OpenMP keeps each waiting thread spinning at full power before it sleeps. With `CORSIA_SOCKET` set, patch `0006` makes waiting threads give up the core after a short spin (`CORSIA_COOL=0` turns it off; your own `OMP_WAIT_POLICY` or `GOMP_SPINCOUNT` always wins). Also leave `-t` alone: llama.cpp already uses one thread per physical core, and hyperthreads add heat to matrix work without adding speed. `stream-on-40gb.sh` reports the CPU seconds and package temperature of every arm, and takes `CPU_QUOTA=150%` for a hard ceiling.

## Speed on the processor (0014 to 0021, 0023, 0027)

The fork is built without OpenMP by default (`corsia-build.sh`): ggml's own thread pool decodes 15 to 23% faster on the i7-6700, and its waiting threads sleep at once under `CORSIA_COOL`. Prompt attention over the q8_0 cache Corsia uses now runs tiled (`0018`): each 64-row tile of the cache is converted once for 64 prompt tokens, so reading new tokens on top of a long conversation, the normal case for a coding agent, is about 3 times faster (`CORSIA_FA_TILED_QUANT=0` turns it off). `0015` does the same for single-token decode across the query heads that share a KV head, and `0014` sizes n-gram drafts by how well they have been doing. `CORSIA_OP_PROFILE=1` (`0017`) prints where graph time went, per op and weight, when the program exits. `0027` scores each cached K row once for all the query heads that share it (the same output, a little faster at long context on AVX2 processors). `0026` fixes `CORSIA_COOL=0`, which also turned weight repacking back on.

Two more are in the fork switched off, because their speed is not measured yet: `0029` is a two-stage output layer (`CORSIA_HEAD_PREVIEW=4096`: a first pass over the high 2 bits of the Q6_K output matrix, then the best rows scored exactly; quality checked, KL divergence 0.00003 with the same top token every time) and `0028` holds fast memory for the KV cache during a load (`CORSIA_KV_FIRST=1`).

## Loading (0022, 0025) and uneven memory

With the pool on, the loader no longer prefetches the expert tensors (`0022`): the pool reads them straight from the drive, and prefetching them too read every expert twice on a cold start. The weights every token uses (attention, the output layer: under a gigabyte, but half of what a token reads) load first, and the pool can warm in tiers of use (`0025`).

That order matters on machines whose memory modules differ in size: only the matched part runs dual-channel, so part of the memory reads at about half speed. `membw.c` here measures it without root (`local-agent.sh setup` runs it). When it is uneven, `local-agent.sh start` runs `membw guard` during the load: the guard takes the free memory, gives the fast part back and lets go of the slow part only as memory runs out, so the model fills the fast part first. On the test desktop (32 GB and 8 GB modules) decode went from 9.2 tok/s with the model in the slow part to 11.0 to 11.2 in the fast part.

## 0001: load through Corsia

`0001-read-tensors-through-corsia.patch` makes llama.cpp (build b12283) load a model's tensors through Corsia's engine socket instead of reading the file itself. Corsia spreads every read over the original and the copies on your extra drives, and the data lands straight in memory the two share.

## What it changes

- Two new headers: `src/corsia-client.h` (the C client, the same file as `clients/c/corsia-client.h`) and `src/llama-corsia.h` (a small reader).
- About 20 lines in `src/llama-model-loader.cpp`: when loading without mmap, each tensor is read through Corsia in 32 MiB ranges, several per request.
- Nothing changes unless `CORSIA_SOCKET` is set. Corsia is only used when it serves a file of exactly the model's size, for single-file models. Any failure falls back to llama.cpp's own read for that tensor, with a warning, so a Corsia problem never stops a model from loading.

## Use it

```bash
git -C llama.cpp apply /path/to/corsia/integrations/llama.cpp/0001-read-tensors-through-corsia.patch
corsia serve model.gguf --mount ~/corsia --copy-to /media/usbdisk     # prints the socket
CORSIA_SOCKET=/run/user/1000/corsia/model.gguf.sock llama-completion -m model.gguf -lm none -p "Hello"
```

`-lm none` turns mmap off (this llama.cpp has no `--no-mmap`). `CORSIA_ARENA_MIB` sets the shared memory, 256 by default. llama.cpp logs `reading tensors through Corsia` when it is in use.

## 0002: stream experts

`0002-stream-experts-through-corsia.patch` is for MoE models that do not fit in RAM. llama.cpp memory-maps the model from Corsia's mount (its default load mode), so the kernel drops and refetches expert pages through Corsia as memory runs short. On top of that, as soon as a layer's router has picked its experts (`ffn_moe_topk-<layer>`), the patch asks Corsia in one request for exactly those experts' slices of that layer's `ffn_gate_exps`, `ffn_up_exps` and `ffn_down_exps` (or fused `ffn_gate_up_exps`). Corsia reads them from all its drives at once and stores them straight into the kernel's page cache for the mounted file, so the expert matmuls find them in memory instead of faulting them in page by page.

- One new header, `common/corsia-experts.h`, and two lines in `common/common.cpp`. It uses llama.cpp's eval callback, so nothing in the compute code changes, and it stays out of the way if a program installs its own callback.
- It reads the expert tensors' offsets from the GGUF itself, checks Corsia serves a file of the same size, and warns if the model was not opened from Corsia's mount (prefetching only helps there).
- Any failure turns prefetching off with a warning; the model keeps running the usual way.

```bash
corsia serve model.gguf --mount ~/corsia --copy-to /media/usbdisk
CORSIA_SOCKET=/run/user/1000/corsia/model.gguf.sock llama-completion -m ~/corsia/model.gguf -p "Hello"
```

## Measure it

`stream-on-40gb.sh MODEL.gguf CAP_GIB [COPY_DIR ...]` measures generation speed under a memory cap smaller than the model, three ways: memory-mapped from the SSD (stock), from Corsia's mount, and from the mount with expert prefetch. Corsia and llama.cpp share one systemd scope so the cap covers both.

`try-on-40gb.sh MODEL.gguf [COPY_DIR ...]` builds Corsia and patched llama.cpp, serves the model with a copy in each folder you name, waits for the copies, then times cold loads both ways, 3 times each, alternating. It refuses machines with less than 24 GiB of RAM.

## Status (2026-11-22)

- Corsia's PREFETCH (dev box, 2 GiB stand-in): three expert-sized ranges went from 0% to 100% resident in one 45 ms request, a range nobody asked for stayed at 0%, the bytes matched the original, and touching prefetched pages ran at 4,359 MB/s against 129 MB/s for pages faulted in through the mount. A layer's worth (24 ranges) took 291 ms. Ranges are rounded out to 1 MiB blocks (90 MiB fetched for about 66 MiB wanted); exact ranges are a follow-up.
- Patch 0002 applies after 0001 on a fresh b12283 and the patched `common.cpp` compiles with -Wall -Wextra, no warnings. `corsia_prefetch` in the C client was checked end to end (0% to 100% resident, bytes match).
- Patch 0001 applies cleanly to a fresh b12283 and the patched loader compiles with `-Wall -Wextra` and no warnings.
- The C client passed byte-for-byte checks against a running `corsia serve` (512 ranges with odd offsets and lengths, batches of 1, 4 and 8).
- Neither patch has run inside llama.cpp with a real model yet: `try-on-40gb.sh` and `stream-on-40gb.sh` do that on the 40 GB machine.

## Agent mode

`capture-agent-request.py OUT_DIR` stands in for an OpenAI-compatible server and saves what a coding assistant sends. `agent-bench.sh MODEL.gguf CAPTURE_DIR [CAP_GIB [COPY_DIR ...]]` replays it through llama-server (built from this fork with `-DLLAMA_BUILD_SERVER=ON`) with and without `corsia agent` in front, and with `ARMS="pp256 pp2048"` times prompt batches. With CAP_GIB everything runs in one memory-capped scope and the model streams through Corsia's mount. See the main README's "Coding assistants" section.
