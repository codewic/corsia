# Corsia: task tracker

Spec: [SPEC.md](SPEC.md). Update as work lands.

> This is the project's working log, kept in the open: what was tried, on which machine, what it measured, and what did not work. It is long and written as the work happened. For a way in, start with the [README](README.md) and the guides in [docs/](docs/).

## Phase 1: lanes and bench (done 2026-11-22)

- [x] Rust project, two dependencies (io-uring, libc), builds in about 18 s at `-j 2`
- [x] `corsia lanes`: drives to controllers from sysfs (SATA link, USB port, speed and protocol, NVMe and controller PCIe links, SD readers), shared bandwidth per lane, `--json`
- [x] Advice: USB 3 device on a USB 2.0 port, USB 2.0-only device, drives sharing a lane, free lanes, Bulk-Only on fast links
- [x] `corsia bench`: one thread and one io_uring queue per drive, O_DIRECT reads of a random-bytes test file, four solo patterns plus all drives at once, p50/p99 waits, split shares, `--json`
- [x] Test files removed after a normal run, after Ctrl+C and with `--json`
- [x] Fixed on real hardware: SCSI vendor field splitting USB bridge model names, USB sticks claiming to be rotational, long pci.ids names

### First results (dev box, i5-2450M, 2026-11-22, 512 MiB files, 3 s per test)

| Drive | Lane | Seq 1 MiB x8 | Rand 64 KiB x8 | Rand 1 MiB x1 | Rand 4 MiB x4 | 1 MiB wait p50/p99 |
|---|---|---|---|---|---|---|
| sda SSD 512GB | SATA 6 Gb/s | 504 MB/s | 238 MB/s | 307 MB/s | 492 MB/s | 3.4 / 4.3 ms |
| sdc WD5000LPLX (USB disk) | USB 2.0 #1 | 35 MB/s | 8 MB/s | 27 MB/s | 31 MB/s | 38.6 / 70.2 ms |

After moving the WD to the NEC USB 3 port (5 Gb/s), 1024 MiB files, 4 s per test:

| Drive | Lane | Seq 1 MiB x8 | Rand 64 KiB x8 | Rand 1 MiB x1 | Rand 4 MiB x4 | 1 MiB wait p50/p99 |
|---|---|---|---|---|---|---|
| sda SSD 512GB | SATA 6 Gb/s | 510 MB/s | 246 MB/s | 308 MB/s | 497 MB/s | 3.4 / 4.5 ms |
| sdc WD5000LPLX (USB disk) | USB 3 (NEC) | 115 MB/s | 9 MB/s | 66 MB/s | 94 MB/s | 16.0 / 21.0 ms |

All at once: 590 MB/s total (1.19x the SSD alone), split 84/16, neither drive slowed. The WD is now at its own mechanical limit. The swap left its NTFS volume dirty (unplugged while mounted); the owner cleared it with `ntfsfix -d`.

Before the move, all at once: 523 MB/s total, neither drive slowed (separate lanes). The WD enclosure (Innostor IS621, bcdUSB 2.10) is USB 3 capable but plugged into USB 2.0; the NEC USB 3 controller holds only the Vorynza OS installer stick (USB 2.0 device). Not benched: the installer stick.

## Phase 2: replay bench (first version done 2026-11-22)

- [x] Trace format (`src/trace.rs`): text, `# key=value` header, one line per token, comma-separated expert ids per layer
- [x] `corsia trace synth`: Zipf popularity per layer plus token-to-token repeats; defaults shaped like Qwen3-30B-A3B (48 × 128, top 8)
- [x] `corsia replay`: LRU RAM cache decides the misses once, then each strategy replays the same misses on the real drives, layer by layer, through one io_uring queue
- [x] Strategies: one drive, sliced by speed, whole experts by speed, copies + soonest drive, and the same with the next 1 / 3 / all layers known (perfect predictor), plus one drive with the whole token known as the matching baseline
- [x] Each in-flight read owns its buffer: sharing buffers made btrfs fail direct reads with checksum errors (sda2's btrfs counter went from 0 to 168 corruption errors at 11:08:55, all from Corsia's own test file; the kernel logged only the first 10 because btrfs rate-limits these messages; disk data unaffected; the owner reset the counter with `btrfs device stats -z`)
- [ ] Record a real routing trace on the 40 GB machine and replay it
- [ ] Add co-firing layout and hot-expert copies on the slow lane
- [ ] Model compute time per layer so prefetch can overlap with it

### First results (dev box, synthetic trace, 2026-11-22)

Qwen3-30B-A3B shape, 2.75 MiB experts (16.5 GiB in all), 4 GiB RAM cache (24% of experts), 64 warm-up tokens, 16 tokens per strategy. Cache hits 53%; 180 of 384 experts per token come from the drives (495 MiB). Drives: sda SSD 494 MB/s, 1.3 ms per read; sdc WD on USB 3, 96 MB/s, 5.5 ms per read.

| Strategy | Tokens/s | MB/s | Split |
|---|---|---|---|
| All on sda | 0.91 | 471 | sda 100% |
| Each expert sliced by speed | 0.55 | 284 | 84 / 16 |
| Whole experts spread by speed | 0.66 | 340 | 83 / 17 |
| Copies + soonest drive | 0.91 | 471 | 92 / 8 |
| ... + next layer known | 1.02 | 530 | 87 / 13 |
| ... + next 3 layers known | **1.05** | 543 | 85 / 15 |
| All on sda + whole token known | 0.94 | 486 | sda 100% |
| ... + whole token known | 1.01 | 525 | 83 / 17 |

What it means:
- Without prediction, a slow second drive adds nothing, and naive spreading (by bandwidth share) costs 28 to 40%: each layer waits for its slowest read, and the hard disk's fixed cost per read makes it the slowest.
- With the next 3 layers known, the two drives reach 1.05 tokens/s: 1.15x one drive, and about 93% of the pair's combined bandwidth (589 MB/s would be 1.13 tokens/s). Prediction alone gives one drive only 1.03x.
- Rule for the scheduler: never put a read on a drive that would finish after the others; plan by finish time (fixed cost plus bytes over bandwidth), not by bandwidth share.
- Bigger gains need drives closer in speed (a USB 3 SSD on the free NEC port would be about 400 MB/s) or a better cache hit rate.

## Phase 2b: routing predictor (first version done 2026-11-22)

- [x] `src/predict.rs`: online predictor from routing alone, three signals (layer-to-layer affinity, previous token's repeat, popularity), ranks experts for the next 1 to 8 layers
- [x] `corsia predict TRACE`: recall per signal and lookahead, no drives involved
- [x] `trace synth --affinity`: optional layer-to-layer structure in synthetic traces
- [x] `replay` strategy `predicted`: live cache, prefetch of the next layers while the current layer reads, `--compute-ms` to model CPU time per token, `--only` to pick strategies
- [x] Prefetch rules learned on the way: rank only experts not already in RAM; prefetched experts wait in a small staging area instead of the cache; prefetches only go to a drive that is idle at that moment
- [ ] Hidden-state predictor in the engine patch (run the next layers' routers early on the current hidden state)
- [ ] Re-run `predict` and `replay` on a real trace

### Results (dev box, synthetic trace, affinity 0, 4 GiB cache, 16 tokens)

Predictor recall with 8 guesses (random would be 6%): combined 36%, repeat only 36%, affinity only 23%, popularity only 22%; 50% with 16 guesses. With `--affinity 0.5` it did not improve: the synthetic affinity spreads each expert's successors over 32 candidates, too diffuse to learn. Real routing needs a real trace.

| Setup | One drive (sda) | Predicted prefetch, first version | With staging + idle-drive rule |
|---|---|---|---|
| No compute | 0.89 to 0.91 tok/s | 0.70 (40% of guesses used) | 0.85 (34% used) |
| 250 ms compute per token | 0.73 to 0.74 tok/s | 0.65 (38% used) | 0.71 (24% used) |

What it means:
- A predictor this weak cannot beat one drive here: wrong guesses cost bandwidth, and the SSD is busy about 80% of the time even with compute.
- The first version also polluted the cache (566 guesses used but only about 160 fewer demand reads); staging fixed that.
- With perfect knowledge of the next 1 to 3 layers the pair reaches 1.12x to 1.15x (Phase 2 table), so that is the prize for a good predictor on this hardware. Getting there needs much higher accuracy, which points at the hidden-state predictor in the engine patch.

## Phase 3: daemon and blind mode (first version done 2026-11-22)

- [x] `corsia serve MODEL --mount DIR --copy-to DIR...`: the model appears read-only at DIR/<name> (FUSE through fuser 0.19, pure Rust, `fusermount3`, 8 request threads); the original file stays where it is as the full copy
- [x] Speed copies (`src/store.rs`): append-only 1 GiB packs plus an index, data synced before its index line; works on any filesystem; reused across restarts (checked: 512 of 512 chunks picked up, nothing copied again)
- [x] Copier: fills copies only while nothing has read for 400 ms, busiest chunks first; measures a copy once it holds 64 MiB (WD measured 92 to 104 MB/s)
- [x] Reads (`src/engine.rs`): 1 MiB blocks from whichever source will finish soonest (fixed cost plus bytes over bandwidth, queue-aware), kept in a RAM cache (`--cache-mib`, default 256); readers of a block already on its way wait for that one read; a failing copy goes offline and the read is retried from the original
- [x] Live API (`src/api.rs`): `GET /api/status` (JSON) and `GET /api/events` (SSE, one status event per second with MB/s per drive), 127.0.0.1 only, Host header checked (a foreign Host gets 403)
- [x] Ctrl+C or SIGTERM unmounts cleanly and keeps the copies
- [ ] Pull a drive while serving and confirm the failover (not tried: pulling the WD dirtied its NTFS volume last time)
- [ ] An engine on the mount (llama.cpp with and without mmap) on the 40 GB machine
- [ ] Close the gap to direct reads (below)

### Results (dev box, 2 GiB random stand-in model on the SSD, full copy on the WD over USB 3)

Correctness: the SHA-256 of the file read through the mount matched the original on every run (5 runs, cold cache).

Fair speed test, every 4 MiB range read exactly once in random order:

| Readers | Direct from the original (O_DIRECT) | Through Corsia | WD copy's share |
|---|---|---|---|
| 1 | 436 MB/s | 201 to 231 MB/s | 0% |
| 4 | 482 MB/s | 306 to 365 MB/s | 13 to 15% |
| 8 | 483 MB/s | 339 to 390 MB/s | 11 to 13% |

How it got there, and what failed:
- Reading from the drive per FUSE request: about 100 MB/s. The kernel sends 128 KiB requests; each became its own small drive read.
- Whole 4 MiB chunks in RAM: 224 to 344 MB/s, but 63% over-fetch, because the kernel's readahead peeks into the next chunk.
- 1 MiB blocks: over-fetch down to 12%, best numbers above.
- Sequential read-ahead inside Corsia (fetch the next 2 blocks when a reader moves on): worse, 228 MB/s at 4 and 8 readers, 44% over-fetch, 42,022 queued fetches going stale. Removed.

What it means: blind mode works and is correct, and the scheduler does put a slow copy to use under load, but the FUSE path itself costs 20 to 50% against direct reads on this machine. Ideas to close it, to measure one by one:
- [x] Raise the FUSE mount's readahead (tested 2026-11-22, owner ran `echo 4096 | sudo tee /sys/class/bdi/<mount dev>/read_ahead_kb`). Requests grew from 124 KiB to 1024 KiB, over-fetch fell to 0%, and throughput rose 10 to 25%:

  | Readers | Direct | 128 KiB readahead | 4 MiB readahead | Share of direct |
  |---|---|---|---|---|
  | 1 | 430 MB/s | 213 | 266 | 62% |
  | 4 | 481 MB/s | 338 | 389 (WD copy 16%) | 81% |
  | 8 | 481 MB/s | 348 | 384 (WD copy 17%) | 80% |

  Corsia itself used 60% of one core during a 4-reader run, so the rest of the gap is the kernel's FUSE copies (the PC was also busy: load 5.3 on 4 threads, 5 minutes after a reboot). `corsia serve` now prints the exact command when the mount is below 1 MiB. The setting belongs to the mount and goes away when it unmounts.
- Hinted mode reads through Corsia's own interface in expert-sized ranges, skipping FUSE
- FUSE passthrough for ranges served from the original
- io_uring in the engine's fetch path instead of pread per thread

## Phase 4: hinted-mode reads (first version done 2026-11-22)

- [x] Engine socket (`src/hint.rs`): `corsia serve` listens on `$XDG_RUNTIME_DIR/corsia/<model>.sock` (folder 0700, socket 0600, peer uid checked); protocol v1 documented at the top of the file
- [x] Zero copy: the engine hands Corsia a memfd arena once; each READ batch of (offset, len, arena offset) is cut at 4 MiB chunk boundaries, every piece goes to the soonest source, and io_uring reads land straight in the shared arena with O_DIRECT; the reply says where each range starts
- [x] A failed copy read is retried from the original inside the same batch
- [x] Reference client (`src/hintclient.rs`) and `corsia hint-bench SOCKET [--threads] [--batch] [--verify ORIGINAL]`
- [x] C client (`clients/c/corsia-client.h`, header only) and `clients/c/corsia-test.c`: 512 ranges with odd offsets and lengths, batches of 1, 4 and 8, 0 mismatches against a running daemon
- [x] llama.cpp patch (`integrations/llama.cpp/`, build b12283): with `CORSIA_SOCKET` set and `-lm none`, tensors load through Corsia in 32 MiB ranges, several per request; size check, single-file models only, falls back to the file on any failure. Applies cleanly to a fresh b12283; the patched loader compiles with -Wall -Wextra, no warnings (checked with g++ directly; cmake is not installed here)
- [ ] Run `integrations/llama.cpp/try-on-40gb.sh` on the 40 GB machine: cold load time direct vs through Corsia, with a copy on a second drive
- [ ] Expert fetching during generation (models bigger than RAM): the larger follow-up
- [ ] Queue-depth-aware cost model: the serial model under-rates the SSD with many reads in flight, so big batches from many readers hand the copy too much (see below)
- [ ] Prefetch messages (warm ranges into the daemon's cache ahead of a READ)

### Results (dev box, 2 GiB random stand-in, full copy on the WD over USB 3)

Correctness: `--verify` with odd offsets and lengths, 1 reader / 4 readers batch 8 / 8 readers batch 3: all 512 ranges matched the original byte for byte, every run.

Each 4 MiB range read once, random order:

| Readers | Direct (O_DIRECT) | Blind mode (FUSE) | Hinted, 1 range per request | Hinted, 8 per request |
|---|---|---|---|---|
| 1 | 441 MB/s | 213 to 266 | 428 (97%) | 454 (103%) |
| 4 | 494 MB/s | 306 to 389 | 499 (101%) | 395 |
| 8 | 499 MB/s | 339 to 390 | 452 (91%) | 394 |

About 14 to 16% of ranges started on the WD copy whenever more than one read was in flight.

What it means:
- Hinted mode removes the FUSE cost: it matches direct reads and is 1.6 to 2x faster than blind mode.
- One reader asking for a batch of 8 beats a direct reader: Corsia spreads the batch over both drives at once. This is the shape an engine should use (a layer's experts per request).
- With many readers and big batches (32 to 64 reads of 4 MiB in flight) it drops to about 395 MB/s: the cost model books SSD reads as if served one at a time, so the WD gets more than it can finish before the SSD, and batches wait on it.

## Phase 5: expert streaming (first version done 2026-11-22)

- [x] Protocol: PREFETCH (0x02) / PREFETCH_OK (0x82). Corsia reads the 1 MiB blocks a batch of ranges touches (8 in parallel, soonest drive, block cache) and stores them into the kernel's page cache for the mounted file (FUSE notify store); replies once resident
- [x] `corsia hint-prefetch SOCKET OFFSET:LEN...`, Rust `Client::prefetch`, C `corsia_prefetch`
- [x] llama.cpp patch 0002 (`common/corsia-experts.h` + 2 lines in `common/common.cpp`): eval callback on `ffn_moe_topk-<layer>` prefetches exactly the chosen experts' slices of that layer's expert tensors (offsets from the GGUF); no compute changes; off unless `CORSIA_SOCKET` is set
- [x] `integrations/llama.cpp/stream-on-40gb.sh`: tokens per second under a memory cap, SSD mmap vs mount vs mount + prefetch, Corsia and llama.cpp in one scope
- [ ] Run `stream-on-40gb.sh` on the 40 GB machine (the real verdict)
- [ ] Prefetch exact ranges instead of whole 1 MiB blocks (36% over-fetch on expert-sized ranges)
- [ ] Prefetch the next layer too (predicted), so reads overlap compute instead of only batching demand
- [ ] Use the copies for prefetch under load (queue-depth-aware cost model)

### Results (dev box, 2 GiB stand-in, copy on the WD)

| Check | Result |
|---|---|
| 3 expert-sized ranges before / after one PREFETCH (45 ms) | 0% / 100% resident (`mincore`, nothing read them) |
| A range not asked for | stayed 0% |
| Prefetched bytes vs the original | identical |
| Touching prefetched pages / faulting cold pages through the mount | 4,359 MB/s / 129 MB/s |
| A layer's worth: 24 ranges of 2.75 MiB | 291 ms (90 MiB of whole blocks for about 66 MiB wanted) |
| C client `corsia_prefetch` | 0% to 100%, bytes match |

## First real model run (dev box, 2026-11-22)

Qwen3-30B-A3B UD-Q2_K_XL (11.8 GB, 128 experts, top 8, 48 layers) under a 6 GB cap (systemd scope, no swap), llama.cpp fork `corsia` (b12283 + 3 commits), i5-2450M AVX only, SATA SSD plus a full copy on the WD over USB 3, 32 generated tokens, `-c 2048 -fa on -ctk q8_0 -ctv q8_0 -b 256 -ub 256 -t 4`.

| Setup | Prompt | Generating | vs stock |
|---|---|---|---|
| Stock llama.cpp, mmap from the SSD | 0.34 tok/s | 0.05 tok/s (18.8 s/token) | 1x |
| Corsia mount (blind mode), 3 runs | 0.33 to 0.36 | 0.52 to 0.74 tok/s | 10 to 15x |
| Mount + demand prefetch (0002) | 0.34 | 1.38 tok/s (725 ms/token) | about 27x |
| Mount + prefetch + next-layer guess (0003) | 0.34 | 1.11 tok/s (899 ms/token) | about 22x |

- Next-layer guessing from the hidden state hit 97% (11,345 of 11,656 experts used were among the 12 guessed), against 36% for the routing-history predictor.
- Guessing was still slower than demand prefetch alone here: 12 guesses for 8 used is 50% more bytes under a tight cap, and on 2 cores the daemon's workers compete with llama.cpp's threads. To try: `CORSIA_PREDICT_EXTRA=0`, fewer prefetch workers, `-t 3`. Single runs: the gaps are provisional.
- The first round silently ran without prefetch (`sscanf` matched every tensor as a router bias); fixed in commit cc7b231. Lesson recorded: check the engine's own log for "streaming experts" before trusting a number.
- Why the plain mount beats stock mmap by 10x is a hypothesis (smaller reads waste less memory under pressure), not yet measured.

## Second machine: testbox (2026-11-22)

i7-6700 (4 cores / 8 threads, AVX2), 38 GB RAM, one spinning 320 GB laptop disk (Hitachi), no copy drive. Same model and 6 GB cap, 16 tokens, `-t 8`, fork built natively there. Reached over Tailscale as `user@testbox` (the link was relayed via London, about 120 KB/s, so the fork went over on a USB stick).

| Setup | Generating | vs stock | Dev box, same setup |
|---|---|---|---|
| Stock llama.cpp, mmap from the disk | 196 s/token (stopped after 10 tokens; disk 98% busy, 250 ms waits) | 1x | 18.8 s/token |
| Corsia mount | 15.7 s/token | 12x | 1.35 to 1.9 s/token |
| Mount + demand prefetch | 6.4 s/token | 31x | 0.73 s/token |
| Mount + prefetch + next-layer guess | 9.4 s/token | 21x | 0.90 s/token |

- The pattern holds across two very different machines: demand prefetch gives 27 to 31x over stock mmap under memory pressure, and the next-layer guess is 97 to 98% right but still slower than demand prefetch alone.
- On a hard disk the guess's extra reads cost more (12 fetched for 8 used). Next: `CORSIA_PREDICT_EXTRA=0`, and only guessing ahead when the drive is idle, so guesses never queue in front of demand reads.

## Real traces (2026-11-22)

Recorded on testbox with fork commit 5e8585f (`CORSIA_TRACE`), model in RAM, 256 generated tokens each: `traces/qwen3-30b-q2-prose.trace` (open, for tuning) and `traces/sealed/qwen3-30b-q2-code.trace` (sealed: only for accepting a change, never for tuning). The recorder wrote one incomplete first line (at prompt time llama.cpp computes only the last token in the final layer); stripped on fetch. Fixed in fork commit 17224fb (patch `0007-complete-trace-lines.patch`): a line is written only when every layer recorded its experts; checked with 8 capped tokens on the dev box, 7 complete lines.

First readout on the open trace (`corsia predict`, 8 guesses): affinity alone 64%, repeat 48%, popularity 42%, combined 49%. On synthetic traces affinity was 23%: real routing has strong layer-to-layer structure, and the combined weights are wrong for it. The hidden-state guess reached 88 to 97% in the live runs.

## Expert-major copies (first version done 2026-11-22)

Why: in a GGUF one expert's slices of a layer (`ffn_gate_exps`, `ffn_up_exps`, `ffn_down_exps`) sit in three tensors far apart, so one expert costs three seeks. On testbox's lone hard disk the demand-prefetch run measured 5.96 s/token, and the disk was bound by seeks. Corsia controls how a copy is laid out, so a copy can hold each expert in one piece. Nothing changes in llama.cpp.

- [x] `src/layout.rs`: reads the expert tensors' offsets from the GGUF header (version 2 and 3, the same tensors the fork's `common/corsia-experts.h` finds). Copy space: layer by layer, experts in a chosen order, each expert's slices side by side, then everything else in file order. Each slice is stored as the whole pages it touches, so a page two slices share is stored twice (11.07 GiB for the 11.00 GiB Qwen3 Q2 file, +0.6%). An original range maps to copy pieces by taking, at each page, the stored run that goes furthest, so one slice asked for alone is one piece, and every read stays page aligned (direct reads, whole pages into the page cache).
- [x] Co-firing order: per layer, pair counts from a trace, greedy path building (heaviest pairs first), then 2-opt. A layer costs as many reads as its wanted experts form runs, which is the count minus the neighbouring pairs, so this maximises exactly what matters.
- [x] `corsia serve --copy-layout experts [--copy-order TRACE]`: folder `<model>-<bytes>-experts` with a `layout` file (a fingerprint of the expert offsets, plus the order). A started copy keeps its layout; one made for another file is refused. It fills in copy order, so what is copied is one stretch. It may sit beside the original on the same drive (testbox's case), and sources on one drive now share their busy time in the scheduler.
- [x] Engine: `plan()` maps a batch into the copy, joins pieces whose gap is less than what the drive moves in one seek (`per_read_ms` × MB/s, kept between 64 KiB and 4 MiB), up to 8 MiB per read, and takes the copy when it finishes sooner than the plain sources; the rest goes as before. `exec()` runs a job and falls back to the original if the copy fails. `prefetch_batch` (a small change in hint.rs) and blind-mode block reads use it. `choose()` skips expert copies, so hinted READ is unchanged.
- [x] Tests (all 16 unit tests pass): random page ranges read through a built copy equal the original; one expert is one read; the layout file round-trips and refuses another model; co-firing experts become neighbours; the real Qwen3 header parses (48 layers × 3 tensors × 128 experts). Engine, on a 9 MB stand-in whose slices start and end mid-page, with the copy on the original's own drive and filled by the real copier step: 300 random reads through the engine equal the original, one expert's prefetch is one job from the copy, neighbours in the order merge into one job, every wanted page arrives once, reopening keeps the layout, and a partial copy falls back to the original.
- [x] `corsia replay TRACE --model GGUF` (or `--disk-model` for a synthetic layout): no drives. The RAM cache picks the misses, each layer's misses are planned the way a prefetch batch is, and a hard disk is modelled as a fixed seek per read plus bytes over bandwidth. The co-firing order is learned from the warm-up tokens only (or `--order-from`), so the replayed tokens are unseen. `--measured-s` fits the seek cost to a measured s/token for the file layout.

### Results (disk model, Qwen3-30B-A3B UD-Q2_K_XL offsets, 2026-11-22)

RAM cache 800 MiB, chosen because it gives about 4 fetched experts per layer per token (191 per token), close to the 4.25 seen with the trace. Order learned from tokens 0 to 127 of the open trace, replayed on tokens 128 to 254. Outputs in `results/expert-layout/`.

| Layout | Reads per token | MB per token | 10 ms, 100 MB/s | Fitted to 5.96 s, 100 MB/s (4.61 ms per read) | Fitted, 80 MB/s (3.08 ms) |
|---|---|---|---|---|---|
| File layout (today) | 538 | 350 | 8.88 s | 5.96 s | 5.96 s |
| Expert-major, id order | 185 | 340 | 5.25 s | 4.25 s | 4.82 s |
| Expert-major, co-firing order | 133 | 340 | 4.72 s (1.88x) | 4.01 s (1.48x) | 4.66 s (1.28x) |

Final check on the sealed code trace (order learned from the whole open trace, 10 ms and 100 MB/s, then fitted): 587 reads to 203 (id order) and 164 (co-firing); 9.73 s to 5.37 s (1.81x); fitted 5.96 s to 4.33 s (1.38x). The file layout's 608 pieces per token match the 612 estimated from the trace.

- Seeks per token fall about 4x. One read per expert gives 3x of that on its own; the co-firing order adds the rest. On prose, 3.55 of a token's 8 experts per layer sit next to a partner (0.41 in id order); an order learned on prose carries over to code only in part (1.93).
- After the change the disk is bound by bandwidth: 3.4 of the 4.0 seconds is moving 340 MB per token. So the expert layout is worth having, 1.3x to 1.9x depending on the disk, but it is not the big step. The floor is the bytes: more cache hits, smaller experts (bit-sliced experts, backlog item 6) or a second drive move it; the layout cannot.
- The fitted cost per read (3 to 5 ms, not 10) suggests testbox's disk already reorders a layer's parallel reads, which fall in one band of about 230 MB. `corsia bench` on testbox gives its real MB/s and per-read cost; rerun the replay with `--seek-ms` and `--disk-mb-s` from it before trusting the seconds.

### Live test: the dev box's WD hard disk (2026-11-23)

The test box is down; the owner's ruling: "use this pc to run everything". The dev box's spinning disk is sdb, a WD5000LPLX (500 GB, 7200 rpm) in a USB 3 enclosure at 5 Gb/s, NTFS through ntfs3. The enclosure speaks Bulk-Only, one request at a time, so unlike testbox's SATA disk it cannot reorder a layer's reads: the fixed-seek model fits it better.

Design: with the original on the SSD, Corsia's soonest-drive choice would mostly avoid the disk, so the GGUF itself is copied onto the WD and served from there. Two layouts, same 6 GB cap, same stream arm (demand prefetch), cold caches (the model and the copy's packs are dropped from the page cache before each):

- original: the GGUF on the WD, nothing else (three seeks per expert). A plain copy beside it is not an arm: Corsia refuses a second plain copy on one drive, and it would be the same layout anyway.
- experts: the same plus an expert-major copy beside it on the WD, co-firing order from the open trace.

`integrations/llama.cpp/hdd-expert-test.sh check | prepare | run` does it: `check` (done 2026-11-23: mounted rw, 229.7 GiB free of 24.2 needed, direct reads work on ntfs3), `prepare` (copies the GGUF to `Skywondrive/corsia-hdd-test/`, spot-checks 64 slices, fills the expert copy outside the cap; no model run), `run` (both layouts through stream-on-40gb.sh, Corsia's status sampled so the summary shows which source served the bytes; `ROUNDS=2` repeats them in the other order). Corsia now drops the pages of any read that could not bypass the page cache, so a filesystem without O_DIRECT neither doubles memory under the cap nor warms the next arm.

Prediction (disk model, same trace window and 800 MiB cache as above, WD numbers from Phase 1: 94 MB/s on random 4 MiB reads, 16 ms for a random 1 MiB read, so 4.8 to 6.9 ms per read):

| WD model | File layout | Expert-major, id order | Expert-major, co-firing |
|---|---|---|---|
| 4.8 ms per read, 94 MB/s | 6.28 s | 4.50 s | 4.25 s (1.48x) |
| 6.9 ms per read, 115 MB/s | 6.76 s | 4.23 s | 3.87 s (1.75x) |

Disk time only: the dev box's two cores add their compute (about 0.6 s/token in the SSD runs, not overlapped with demand reads), so expect roughly 6.9 to 7.4 s/token for the original and 4.5 to 4.9 s/token with the expert copy. The test box's prediction was 5.96 to 4.0 to 4.7 s/token.


#### Round 1 (09:06 to 09:16, one round, stream arm, 32 tokens)

`prepare` copied the GGUF to the WD (64 slices spot-checked) and filled the expert copy in 7.5 minutes (2835 chunks, 11.07 GiB, about 25 MiB/s reading and writing the same disk). Corsia measured the WD original at 91 MB/s and 6.3 ms per read; both sources showed as sdb, so they shared the drive's time. Logs in `results/expert-layout/wd-live-round1*`.

| Layout | Prompt (10 tokens) | Generating | Read from the WD over the run |
|---|---|---|---|
| File layout (WD original only) | 57.8 s | 1.56 s/token | 15.9 GiB in 20,210 reads, all from the original |
| Expert copy beside it | 75.7 s (31% slower) | 1.48 s/token (5% faster) | 16.5 GiB in 37,315 reads, all from the copy |

Today's runs with the original on the SSD generate at 0.5 to 0.65 s/token, so the WD adds about 0.95 s/token of disk wait with the file layout and about 0.88 with the expert copy: an 8% cut, not the 37% predicted.

- The prediction's miss rate was wrong for this box: it assumed testbox's 191 experts fetched per token (the 800 MiB cache setting). The measured disk wait matches a 3 GiB effective cache instead (94% hits, 24 experts per token, 0.92 s predicted for the file layout), where the model predicts 0.61 s for the expert copy. The rest of the gap is the bug below.
- Bug found: blind reads through the mount (the prompt, and pages no prefetch covered) ask for 1 MiB blocks laid out like the file. In the expert copy such a block spans several experts' records, and `plan()` chose the source group by group, so the copy won ties and one read became several (37,315 reads against 20,210). Fixed the same day: one decision per batch, the copy's reads against the same bytes read as they lie in the file, with a test that a 1 MiB stretch of the file is planned from the original while a scattered expert prefetch still goes to the copy.
- The test box's demand-prefetch run was 6.4 s/token on its disk against 1.56 here, under the same cap and model. Not explained yet.
- [x] Round 2 with the planner fix, below

#### Round 2 with the planner fix (11:22 to 12:38, two rounds in alternating order, stream arm, 32 tokens)

Each arm waited up to 15 minutes for the CPU to drop below 65 C; it never got below 89 C, because the box was not idle (load about 3 from other sessions), so every arm ran hot (package mean 92 C, 76 C in the last). Both layouts saw the same conditions, in both orders. Logs in `results/expert-layout/wd-live-round2*`.

| Arm | Prompt | Generating | Reads from the WD |
|---|---|---|---|
| original 1 | 64.2 s | 2.07 s/token | 16.4 GiB in 23,952 reads |
| experts 1 | 62.6 s | 1.55 s/token | original 11.4 GiB in 11,681 + copy 5.4 GiB in 5,005 |
| experts 2 | 62.1 s | 1.83 s/token | original 11.2 GiB in 11,497 + copy 5.5 GiB in 6,233 |
| original 2 | 71.5 s | 2.14 s/token | 16.9 GiB in 23,557 reads |
| **File layout, mean** | **67.8 s** | **2.10 s/token** | |
| **Expert copy, mean** | **62.4 s (8% faster)** | **1.69 s/token (20% faster)** | 28% fewer reads |

- The fix works: the prompt is no longer slower, and reads split the way they should. The copy serves the expert prefetches (about one expert per read, 1.1 MB on average), and the original serves the blind 1 MiB blocks as they lie in the file.
- The 0.41 s/token saved is in line with the disk model at the measured miss rate (0.31 s of disk time saved at a 3 GiB effective cache), given that everything ran slower than round 1 under heavier heat and load.
- Two samples per layout, spread about 8% for the expert copy and 4% for the file layout, so the 20% is real but its exact size is rough. Absolute times are slower than round 1 (2.10 against 1.56 s/token for the same file layout), which points at heat and the busy box, not the layout.
- The dev box cannot cool below about 89 C while other sessions keep it busy, so a cool-down wait alone does not give cool runs here; quiet machine time is needed for clean absolute numbers.

#### Round 3: with the fork's expert pool (18:27 to 19:01, two rounds, the second reversed, 32 tokens)

The fork's expert pool (`CORSIA_POOL_MIB=3584`, fork 43028c0 plus the prompt-batch bypass, swapping on by default with it) keeps experts in memory Corsia fills directly, warm-started from a usage profile. It reads through hinted READ, which ignored expert copies until this round: `read_batch` now plans each batch the way prefetch does (`Engine::place`), so pool misses come from the expert copy with one seek per expert. Before the run, 200 random expert slices read from the copy's packs and 32 random 1 MiB slices of the WD original matched the original after the day's two power cuts. The machine was quiet: every arm started at 54 to 64 C and ran at 55 to 59 C on average. Logs in `results/expert-layout/wd-live-pool*`.

| Layout | Generating (run 1, run 2) | Mean | Prompt, mean | Pool warm-up (3.2 GiB) | Chosen experts already in the pool |
|---|---|---|---|---|---|
| File layout | 2.78, 2.44 s/token | 2.61 s/token | 60.1 s | | |
| Expert copy | 2.16, 2.16 | 2.16 (17% faster) | 62.9 s | | |
| File layout + pool | 1.16, 1.68 | 1.42 (1.8x) | 51.4 s | 54.6 s | 83%, 88% |
| Expert copy + pool | 1.00, 1.00 | **1.00 (2.6x)** | 40.1 s | 39.4 s (28% faster) | 85%, 86% |

- The pool is the big step on a hard disk, and the expert copy adds to it: each remaining miss and each warm-up expert is one seek instead of three. Expert copy + pool was the fastest arm in both rounds and the steadiest (1.00 both times), 14% to 40% faster than the pool on the file layout, 2.6x faster than the file layout alone.
- The slow file-layout pool run (1.68) was not heat: it had 2,540 major page faults against 815 in its twin, so more of what lives outside the pool (attention weights and the like) had to come back from the disk. The expert copy arms did not show that spread.
- Predicted for the pool: about 0.45 s/token of disk time on the file layout and 0.30 with the expert copy (3.5 GiB cache in the disk model). The measured gap between the two pool layouts, 0.16 s/token in round 1, matches.
- The file layout alone was slower than in round 2 (2.61 against 2.10 s/token) although cooler; that arm uses neither the pool nor the expert copy, so the difference comes from elsewhere (the fork was rebuilt in between). Comparisons within a round hold; absolute numbers across days need the same build. Checked since: Corsia does the same for that arm in both rounds (without an expert copy, block reads take the unchanged path, prefetch plans the same plain pieces, and the READ change is not used without the pool). The drive is within about 5% of Phase 1 (`corsia bench`, 2026-11-23 evening: sequential 108 against 115 MB/s, random 4 MiB 91 against 94, 1 MiB wait p50 16.4 against 16.0 ms), but its slowest 1% of reads doubled (p99 53 against 21 ms) after the day's NTFS repairs. The fork comparison so far (43028c0 against cf6944b, unchanged) starts after round 2's build (17224fb plus uncommitted work), so 81af8c5 (expert pointers from a table) and the rest in between are not yet ruled out.
- The test box's spinning disk with the pool: 2.13 s/token cold (the coordinating session's run). This disk with the pool and the expert copy: 1.00.
- [ ] Join an expert's slices into one request on the READ path (they go as separate, back-to-back pieces today: the copy's read count is per slice)
- [ ] Keep the pool's warm-up order in copy order, so the 3.2 GiB warm-up reads the copy front to back
- [x] `hdd-expert-test.sh`: the status sampler could outlive a failed run and look busy to every test queue; it now always stops with the script, and the summary prints the real eval lines
- [ ] Expert copies fill in order and ignore heat; a partial expert copy on a small drive would rather hold the busiest experts
- [ ] Learn the order from misses instead of all fired experts (what the disk actually reads)

## Cool running (done 2026-11-22)

The fans were loud because the processor turned waiting into heat, not because of Corsia. The fork is built with OpenMP, so llama.cpp's `--poll` does nothing: waiting threads follow OpenMP's own spin (300,000 times by default) before sleeping, and a streamed token waits on the drives dozens of times.

- [x] Fork: cool waiting (`common/corsia-threads.h`, commit e165bbd, patch `0006-cool-waiting.patch`). With `CORSIA_SOCKET` set, the program restarts itself once at launch with `GOMP_SPINCOUNT=10000`; builds without OpenMP get polling level 0. Your own `OMP_WAIT_POLICY` or `GOMP_SPINCOUNT` wins, `CORSIA_COOL=0` turns it off. The startup log says what waiting threads do
- [x] Fork: `CORSIA.md` (every change, setting and how to move to a new release) and `scripts/corsia-build.sh` (commit 225303d)
- [x] Test script: no more `-t $(nproc)` (llama.cpp already uses physical cores); every arm reports CPU seconds (user and system) and package temperature; `LLAMA_ENV`, `CPU_QUOTA`, `CORSIA_BIN`
- [x] Corsia: background guesses sleep on a condvar until demand reads finish, instead of waking every 250 µs (not measured on its own yet)

### Results (dev box, i5-2450M 2 cores / 4 threads, 6 GiB cap, predict arm, 32 tokens, one run each)

| Run | Threads, waiting | tok/s | CPU s | Start, peak, mean °C |
| --- | --- | --- | --- | --- |
| A | 4, OpenMP default spin | 1.61 | 125 | 73, 87, 70 |
| B | 2, OpenMP default spin | 1.10 | 95 | 67, 94, 78 |
| C | 2, cool waiting (spin 10,000) | **1.81** | 96 | 73, **84**, 69 |
| D | 2, passive (no spin) | 1.37 | 96 | 79, 84, 70 |

- C is the fastest run this machine has done and the coolest, despite starting 6 °C warmer than B, with 23% fewer CPU seconds than A. Spin 10,000 is the default.
- Passive waiting cools as well as C but is slower: a short spin catches work that arrives within microseconds.
- System time is 50 to 60 of the 96 CPU seconds: page-cache stores and FUSE, the next place to cut (hinted mode or fewer copies).
- The very first A run (straight after the agent session's long replay) refetched 23 GB instead of about 3 GB and ran at 0.16 tok/s; A again was normal. Cause not found; watch for it after long runs on the same mount.
- Single runs with a warming room: treat differences under about 15% as noise.

## Fewer, cheaper expert loads (built 2026-11-23, in testing)

Three opt-in features in the fork, all off by default and uncommitted until the queued tests pass (`~/.cache/corsia-build/{swap,pool,skip}-queue.sh`):

- [x] Residency-aware routing (`CORSIA_SWAP=1`): a chosen expert that is not in memory is swapped for the best runner-up that is, within 8 places and at least half its router score, at most 2 per layer. Rewrites `ffn_moe_topk` before the weights are gathered, so llama.cpp re-weights and renormalises by itself
- [x] Expert pool (`CORSIA_POOL_MIB`): ggml's expert matmul takes per-expert pointers from a table (`ggml_cpu_set_expert_ptrs`, two call sites: `ggml-cpu.c` and `tiled/tiled.cpp`); `common/corsia-pool.h` fills slots in shared memory through Corsia's READ (straight from the drives, no page cache, no FUSE, no reclaim), evicts least recently used, falls back to the mapped file on any failure
- [x] Pool profile and warm start: usage is saved per model at exit (`~/.cache/corsia/<model>.pool-profile`, older runs count half) and the most used experts are loaded before the first token
- [x] Confidence-based skipping (`CORSIA_SKIP=0.05`): chosen experts under that share of the layer's weight are skipped (at least 4 kept), weights renormalised; the skipped slot points at the heaviest expert, so it costs no read and shares that expert's weight reads in the matmul
- [x] `integrations/llama.cpp/swap-quality.sh`: KL divergence, same top token and perplexity against the unchanged model, micro-batches of one token under the cap; settings RATIO:MAX[:POOL_MIB[:SKIP]]
- [x] Results (2026-11-23, below): the pool is exact and halves prompt time; pool + swap is 22% faster than the best page-cache setup; swap becomes the default with the pool; skip fails the quality gate and stays off
- [x] Bug found on the way: ggml names a reshaped view after its tensor ("ffn_moe_probs-3 (reshaped)"), which the callback took for the router probabilities and so swapping and skipping never fired; names now match exactly
- [x] Pool phase 2, tested and rejected: keeping guessing on in pool mode (guesses filtered to experts not in the pool, copied in from the page cache) ran at 1.46 tok/s against 1.93 with guessing off and 1.40 for the baseline, same hot conditions (92 °C mean, two runs each). With about 90% of chosen experts already in the pool, guesses cost more drive time than the misses they hide. The pool now turns guessing off; the copy from the page cache stays (free, about 30 experts per run)
- [x] Committed in the fork: 81af8c5 (expert pointer table in ggml, patch `0008-expert-pointer-table.patch`) and 43028c0 (pool, routing, skipping, `0009-expert-pool-and-routing.patch`)
- [x] Verified as committed (43028c0 built, CPU cool after the runaway `corsia watch` was stopped, start 64 °C, peak 79 °C, 32 tokens): guessing on the page cache 1.78 tok/s, prompt 19.2 s; `CORSIA_POOL_MIB=3584` with nothing else (guessing off and swapping on by default) **2.62 tok/s**, prompt 10.9 s, 93% of chosen experts in the pool. Fastest run on this machine so far, 52 times stock llama.cpp (0.05 tok/s). `stream-on-40gb.sh` now sets `CORSIA_SWAP` only in the swap arm, so the other arms get the default
- [x] `corsia watch` used 74% of a core while idle (four hours in a terminal, heating every run that day): its file watcher reacted to the access events its own reads raised, and re-read forever. It now reacts only to create, modify and remove, draws only on change (and once a second), and paces its loop; idle it measures 0% of a core

### Results (dev box, 6 GiB cap, Qwen3-30B-A3B UD-Q2_K_XL, copy on the WD)

Speed, 32 tokens, two interleaved runs each (single runs vary by about 30% here, and the CPU ran at 82 to 88 °C mean, so later runs may be throttled):

| Setup | tok/s (runs) | Prompt, 10 tokens | Read from drives |
| --- | --- | --- | --- |
| Guessing, page cache (baseline) | 1.87 (1.90, 1.84) | 16.8 s | about 3 GB |
| Expert pool 3,584 MiB, warm | 1.66 (1.55, 1.77) | 9.5 s | 3.0 GB |
| Guessing + swap | 1.93 (2.03, 1.83) | 16.1 s | |
| **Pool + swap** | **2.29** (2.25, 2.33) | **8.6 s** | 2.1 GB |
| Pool + swap + skip 0.1 | 2.29 (2.19, 2.39) | 9.0 s | 1.4 GB |

- The pool had 1,935 slots of 1.9 MiB (the largest layer sets the slot size), found 87 to 92% of chosen experts, warmed 3.2 GB from its profile in 11 to 14 s, and pointed all 144 expert tensors at itself.
- Without guessing the pool generates slower than the page cache with guessing; with swap it is the fastest setup and uses the fewest CPU seconds. Phase 2 brings guessing back to the pool.
- Skip adds no speed once the pool and swap remove the waits: the CPU is the limit and a skipped slot still goes through the matmul.

Quality, `swap-quality.sh`, 768 tokens in micro-batches of one, against the unchanged model (perplexity 8.34):

| Setting | KL divergence | Same top token |
| --- | --- | --- |
| Expert pool alone | 0.000001 | 100% |
| Swap (ratio 0.5, at most 2) | 0.012 | 97.1% |
| Skip under 0.1 (39.7% of experts) | 0.148 | 83.7% |
| Skip under 0.15 (50% of experts) | 0.243 | 80.1% |
| Pool + swap + skip 0.1 | 0.224 | 82.2% |

- The pool is exact (the bytes are the same; 0.000001 is rounding).
- Swap costs about what one step between neighbouring 4-bit quantizations costs, on a model already at 2 bits: on by default with the pool, `CORSIA_SWAP=0` turns it off.
- Skip at useful thresholds moves the output far too much; off by default. A threshold near 0.06 might be safe, not worth chasing while the CPU is the limit.

### What the open trace says (Qwen3-30B-A3B, 255 prose tokens)

- Only 3,256 of 6,144 (layer, expert) pairs were used at all; the top half of those took 91% of uses (top 10%: 39%).
- Pool hit rate by size: 1,024 experts (17% of the model) 69% LRU, 73% LFU, 86% best possible; 2,200 (36%, the 3.5 GiB pool) 94% LRU, 96% best possible; 3,072 (50%) 97%. Plain LRU is within two points of the best possible at useful sizes, so no fancier policy is needed.
- Warm start (profile from the first half of the trace, measured on the second half), 2,200 experts: first quarter of the run 84% cold, 98% warm; whole run 94% cold, 97% warm.
- Batch verification (speculative decoding) of 4 tokens needs 18.8 experts per layer against 32 one at a time: 41% less memory traffic per accepted token when the model is in RAM, nothing saved when streaming under a cap (4.69 per token batched against 4.25 with the last token cached).
- To check on a bigger model (256 experts and more): whether the used share and the skew hold, which decides how small a pool a 600 GB-class MoE needs.

## Agent mode (first version done 2026-11-23): fast with coding assistants

Why agents are slow on a local model: every request carries a large system prompt and tool list, and prompt processing dominates. The plan had five steps; what was built and measured:

- [x] `corsia agent` (src/agent.rs, src/json.rs): OpenAI-compatible `/v1/chat/completions` proxy (streaming) in front of llama-server. Moves changing lines and blocks (`<env>`, dates, working folder, git status, branch; `--volatile LABEL` for more) from the system prompt to the front of the first user message, puts tools in name order, pins each long system prompt to its own slot and short side requests (titles) to the last slot, saves each long prefix's slot state once (`--snapshots DIR`, llama-server `--slot-save-path`, file named by an FNV-1a hash of system prompt plus tools) and restores it when a slot does not hold it (checked against llama-server's `/slots`, so a restarted server is noticed). Reports every request on stderr, `/api/agent`, live on `/api/agent/events` (SSE), and as JSON lines (`--log`). `--plain` only measures. `--snapshots auto` picks a folder on the fastest writable drive (here `~/.cache/corsia/kv` on the SSD).
- [x] Why in the `corsia` binary and not a separate program: no new dependencies (standard-library threads and a 300-line JSON module, all on 127.0.0.1), one thing to install, and it belongs beside the daemon, which can later keep snapshots on its fastest lane and prefetch them like weights.
- [x] `integrations/llama.cpp/capture-agent-request.py` (records what an assistant really sends) and `agent-bench.sh MODEL CAPTURE_DIR [CAP_GIB]` (replays a side request, the first request, a follow-up, a new session in another folder on another day, then restarts llama-server and replays the new session again; `ppN` arms time prompt batches of N). Captures kept in `~/.cache/corsia-build/agent-capture/{full,lean}` (they name local folders and installed skills: not for publishing).
- [x] 8 unit tests (prefix moving, same key across folders and days, code fences left alone, timings found in streams split anywhere or that do not parse, slot choice, JSON round trip). Checked end to end against a stand-in llama-server and then the real one.
- [ ] `--cache-reuse` (KV shifting for near matches) not measured: with the prefix made stable it is not needed, and shifted chunks are approximate (computed after a different context).
- [ ] testbox, full 23,000-token prompt (model in RAM, 8 threads, 16 GB scope): the run died with the machine after 12,288 tokens. Side request 14.8 s (546 tokens). The first request's 2,048-token batches took 87, 194, 286, 437, 626 and 812 s as the context grew (attention, q8_0 KV cache): 2,443 s for the first 12,288 tokens, about 2.6 hours for the whole prompt by extrapolation. A second try with agent mode (2026-11-23, same settings) gave the same batch times (side request 15.0 s; 12,288 tokens at 4.43 tok/s on average) and was stopped there because the machine was needed. Still to run there: the agent arm in full (about 3 hours, almost all of it the first request) for the full-size snapshot and the new-session time; also try an f16 KV cache, which may make CPU attention cheaper at the cost of memory.

### Step 1: what a real assistant sends (opencode 1.19.6, captured 2026-11-22)

- System prompt 70,697 characters, 61,684 of them the list of installed skills; 10 tools (21 KB of JSON). About 22,000 to 22,900 tokens with Qwen3's template (llama-server's progress readout).
- An `<env>` block (working folder, workspace root, git repo yes/no, platform, date) sits 8,679 characters in, about 2,000 tokens. Stock llama-server can reuse only those first 2,000 tokens when the folder or the date changes: the other 20,000 are recomputed for every new session.
- Before each session opencode sends a short side request (a title, 546 tokens) with a different system prompt. With one slot it would evict the long prompt.

### Step 2: prompt batches under streaming (dev box, 6 GB cap, copy on the WD, 2026-11-22)

| Batch (-b/-ub) | Tokens | Prompt time | Tokens per second | Memory peak |
|---|---|---|---|---|
| 256 | 1,024 | 287.8 s | 3.56 | 6,144 MiB (the cap) |
| 2048 | 2,048 | 489.2 s | 4.19 | 6,144 MiB (the cap) |

- Big batches gain only 18% here: prefill on this CPU is compute-bound, not read-bound. The earlier "0.34 tok/s prompt" (First real model run) came from a 10-token prompt where loading dominated, not from batch size.
- Prompt processing slows as the context grows (attention over the tokens already processed, with a q8_0 KV cache on an AVX-only CPU). The full opencode prompt in 2,048-token batches: 444 s, 1,033 s, 1,946 s for the first three batches (context 0 to 6,000). Extrapolated, the 23,000-token prompt takes more than 12 hours on the dev box; stopped after three batches. Recomputing it is not an option here, so reuse and snapshots are the whole game.
- testbox's capped batch test was killed by systemd-oomd (memory pressure, not the cap) while another llama-server there held 5 GB locked; not repeated.

### Steps 3 and 4: stable prefix, slots, snapshots (dev box, 6 GB cap, no copy drive, 2026-11-23)

The same opencode request trimmed to a smaller assistant: skills list removed, 6 tools (read, edit, glob, grep, write, bash): 5,153 tokens. `-c 16384 -fa on -ctk q8_0 -ctv q8_0 -b 2048 -ub 2048`, threads left to llama.cpp (2, with the fork's cool waiting), 16 tokens generated per request. "Client waited" is the whole request as the assistant sees it.

Plain and agent mode back to back on 2026-11-23 afternoon, same build (fork 43028c0) and conditions (a runaway `corsia watch` that heated the morning runs had been fixed):

| Request | Plain: reused / computed | Plain: client waited | Agent mode: reused / computed | Agent mode: client waited |
|---|---|---|---|---|
| Side request (title) | 0 / 546 | 118.7 s | 0 / 546 (slot 1) | 122.0 s |
| First request | 0 / 5,153 | 2,930.7 s (49 min) | 0 / 5,153, snapshot saved (270 MB, 0.5 s) | 2,933.2 s (49 min) |
| Follow-up, same session | not run (same as agent mode) | | 5,153 / 33 | 89.8 s |
| New session, other folder and day | 1,819 / 3,225 | 2,308.6 s (38 min) | 4,971 / 73 | 133.6 s (17x faster) |
| Same, after restarting llama-server | not run (recomputes all 5,153: about 2,930 s) | | 4,971 / 73, restored in 0.6 s | 173.0 s (17x faster) |

- Plain reuses only the 1,819 tokens before the `<env>` block; agent mode reuses everything but the 73 tokens of moved details and the new question.
- The morning run of agent mode (hotter CPU) gave the same shape: 3,127 s first request, 122 s new session. Its after-restart figure (207 s) does not count: the restarted llama-server could not reach Corsia's engine socket (`connect: No such file or directory`) and ran from the mount without expert prefetch. The socket path is one per model (`$XDG_RUNTIME_DIR/corsia/<model>.sock`), so a second `corsia serve` of the same model, from another test, can take it over or remove it; `agent-bench.sh` now stops a capped run that is not streaming instead of warning (its old check also matched the failure line).
- Prompt processing reached 4.2 tok/s in the first 2,048-token batch and fell to 1.78 tok/s over the whole 5,153 tokens: attention grows with the context.

- With the fork's expert pool (`CORSIA_POOL_MIB=3584 CORSIA_SWAP=0`, fork 43028c0), prompt processing got slower, not faster: the side request took 154.8 s against 101.0 s, and the first 2,048-token batch of the first request 840 s against 490 s. A prompt batch touches nearly every expert in each layer, more than the pool's 1,935 slots, so it churned the pool. Stopped there (the cause was clear). The fork's working tree now hands large batch misses back to the page-cache prefetch (`CORSIA_POOL_BATCH_MAX`, default 32); rerun this arm once that is built, to check that prefill is back to the no-pool time and that follow-ups keep the pool's generation gain (built since as fork cf6944b; a rerun was started and stopped to free the machine for a bisect; still to do). From fork abeb13c the pool sizes itself from memory by default, so `CORSIA_POOL_MIB=0` is needed for a no-pool arm.
- Snapshot of the 5,153-token prefix: 270 MB (52 KB per token, q8_0 KV), saved in 0.5 s, restored from the SSD in 0.6 s after the page cache was dropped. The 23,000-token prompt would be about 1.2 GB: 2 to 3 seconds to restore against more than 12 hours to recompute.
- The follow-up and the new session still take 60 to 134 s of prompt time for 33 to 73 new tokens: a small batch touches most experts of every layer, which must stream in under the cap, and the first generated tokens after a restart find no experts in memory.
- Moving the `<env>` block did not change the answers' form in these runs (16 tokens each); a quality check over longer agent sessions is still to do.

### Step 5, next

- Measured (sibling session, testbox, model in RAM, 4 cores, a 256-token copy-and-edit request on Corsia code): no drafting 7.49 tok/s, `--spec-type ngram-simple` 16.70 tok/s, `ngram-map-k` 16.68 tok/s, all 227 drafts accepted: 2.2 times faster, no extra model, same output. On the dev box under the cap with the pool it is slower (2.03 without, 1.74 with): two compute-bound cores have nothing spare to verify drafts with. So: turn it on in llama-server's flags when the model is in RAM or there are spare cores, not on the laptop. It is a server flag, not something `corsia agent` can switch per request; the README's command shows where it goes.
- Speculative decoding for the long code outputs agents write. Two kinds in this llama-server build: a small draft model, or self-speculative n-gram drafts (`--spec-type ngram-simple`, ngram-map-k, ngram-mod, ngram-cache), which suit agents because they copy a lot from their context (files, diffs, tool output). From the real trace (sibling session's estimate): verifying 4 drafted tokens needs 18.8 experts per layer instead of 32, about 41% less memory traffic per accepted token with the model in RAM; under the 6 GB streaming cap it saves no disk reads (4.69 experts per token batched against 4.25 one at a time), so measure before assuming a win on the dev box.
- Prefetch the snapshot's pages and the experts a new session will need while the assistant is still sending its first request.

## Live watch: `corsia watch` (first version done 2026-11-22)

A live terminal for long runs, in the spirit of the Intelligence project's vi-watch (same event format, same look), with a second source: the running daemon.

- [x] `watch/`: its own crate (`corsia-watch`, ratatui + notify + serde_json) so Corsia's main build keeps three dependencies and never resolves the UI's; `corsia watch` runs it (next to `corsia`, then `watch/target/release/`, then `PATH`)
- [x] Runs: follows `~/.cache/corsia-build/progress/*.jsonl` (`CORSIA_PROGRESS` overrides) through file-change events. Per run: arms done, what it is doing now and for how long, the best result, a results table in arm order with every metric and the headline against the first arm ("vs ssd"), log lines. A run whose script died without an end event shows as stopped
- [x] Daemon: SSE client for `/api/events` (plain HTTP over a TcpStream, reconnects with a growing pause). MB/s per drive and in total as graphs on one scale, bytes served, reads, errors and offline drives (glyph plus word), copy fill per drive, prefetch stored / already there / joined / dropped guesses, cache hit rate, copier state. Says plainly when nothing answers, with the start command and the ssh -L line; `--url` for another port
- [x] `integrations/progress.sh`: `prog_open`, `prog EVENT key=value...`, `prog_llama_metrics` (tok/s, s/token, prompt tok/s, hit %, prefetch MiB from llama-completion's log). Both scripts emit start, one step per arm (or per timed load in try-on-40gb.sh), log lines for failures, and an end event from an EXIT trap, so a failed or interrupted run ends as failed. Writes never stop a script; `CORSIA_PROGRESS=off` writes nothing
- [x] `--snapshot WxH [--html]`, `--light`, `NO_COLOR`; checked in dark and light at 132x38 and at 60 columns; light faint text and accent raised to 4.5:1
- [x] Verified live: a real `stream-on-40gb.sh` run (`FORCE=1 SKIP_BUILD=1 TOKENS=4 ARMS=mount`, cap 6) showed up within a second, and the daemon panel followed that run's own `corsia serve` on 7714 (152 MB/s from the SSD while llama.cpp loaded); a stand-in daemon with a copy on the WD showed the copy filling to 100%
- [ ] The scripts' own build step and the daemon inside a run both use port 7714: running two scripts at once would collide (they already would without the watch)
- [ ] A remote run (testbox) is watched by running `corsia watch` there over ssh; reading a remote progress folder from here is not built

## Research backlog (2026-11-22): beyond the usual approach

Ranked for this kind of machine (slow drives, AVX CPU, tight memory). Sources in the chat log of that day.

1. **Next-layer router on the current hidden state, prefetched async.** Applying layer L+1's router to layer L's hidden state predicts L+1's experts well (Eliseev and Mazur 2023; Speculating Experts, arXiv 2603.19289; FloE reports hidden-state similarity above 0.95 in most layers). The router is a tiny matmul (2048 x 128 for Qwen3-30B-A3B), cheap on the CPU in the eval callback. Prefetch L+1 while L computes, fire-and-forget, so reads overlap compute and the idle copy drive gets work. Needs: an async PREFETCH flag, router weights read from the GGUF.
2. **Cache-aware routing.** When router scores are nearly tied, prefer experts already in memory (Prefetch-Steered Routing, arXiv 2610.34721; DyMoE skips low-weight experts). A small per-layer bias tensor added to the router logits, updated from Corsia's residency map. Quality must be measured (perplexity) before it is on by default.
3. **userfaultfd instead of FUSE for expert pages.** Map expert tensors as anonymous memory registered with userfaultfd; Corsia resolves a fault by installing the whole expert slice in one UFFDIO_COPY. No FUSE copies, no 128 KiB limit, no double page cache, and Corsia chooses what to evict (by heat) instead of the kernel LRU. Unprivileged for user faults. llama.cpp users are hitting the page-cache side of this already (issue #20757).
4. **Expert-major copies** (first version built, see Expert-major copies above). In the GGUF, one expert's gate, up and down slices sit in three tensors far apart: three reads per expert. Corsia controls the copies' layout, so a copy can store each expert's three slices together, one read per expert, which matters most on hard disks and USB (the bundling idea of "LLM in a flash").
5. **Speculative decoding with a small draft model.** Verifying several tokens per step shares expert reads across them. Off the shelf in llama.cpp; worth one measured run.
6. **Bit-sliced experts.** Store experts so the low-bit version is a prefix of the high-bit one and read only the top slice on a miss (SliceMoE, arXiv 2512.12990; HOBBIT, arXiv 2411.01433). This is the "progressive weights" idea from the spec, now with published evidence. Needs a custom quant format and kernels: later.
7. **FUSE over io_uring** (Linux 6.14+): fewer syscalls per request. Lower priority than 3, which removes FUSE from the hot path altogether.
8. **Remote experts.** Keep experts in another machine's RAM and send it only the hidden state per layer (about 8 KB each way), with local streaming as the fallback. Pays only on a LAN (round trip under about 10 ms); testbox is across the internet today (137 ms). Design, budget and phased plan: `docs/remote-experts.md`; round-trip probe: `integrations/remote-experts/rtt.py`.

## Phase 6 onwards

See SPEC.md section 5: daemon and blind mode, hinted mode, progressive weights, management UI, layout rewrite, parity, LAN tier.

## Owner decisions open

- [x] Licence: MIT OR Apache-2.0 (published with the repository on 2026-11-24)
- [ ] Name: published as Corsia on 2026-11-24; a trademark check is still to do
- [ ] Management UI as a local web UI served by the daemon (recommended) or a desktop app
- [ ] Mark and reserved hue when the UI starts

## Colab T4 test kit (2026-11-23, not yet run)

- [x] `colab/corsia-t4.ipynb` and `colab/corsia-colab.tar.gz` (Corsia source plus patches 0001 to 0009, 128 KB): builds Corsia and llama.cpp b12283 with CUDA for the T4, downloads Qwen3-30B-A3B Q2 and Q4_K_M, and runs (1) Q2 entirely on the GPU, (2) Q4 with experts on the CPU (`--cpu-moe`), stock, (3) the same with Corsia's expert pool, cold then warm. The series 0001 to 0009 applied to a fresh b12283 reproduces fork commit 43028c0 file for file (0009 re-exported without the fork-only `CORSIA.md`)
- [x] Results (2026-11-23, run from the browser pane on a Colab T4: 15 GB GPU, 12.7 GB RAM, 2 Xeon cores at 2.0 GHz; the CUDA build took about 65 minutes; all nine patches applied cleanly):

| Run | Generation | Prompt (10 tokens) |
| --- | --- | --- |
| Q2 (11.8 GB) entirely on the T4, stock | 72.7 tok/s | 0.5 s |
| Q4_K_M (18.6 GB, bigger than GPU and RAM), experts on the CPU, stock, cold / second run | 1.08 / 1.02 tok/s | 20.0 / 20.5 s |
| Same with Corsia's expert pool (6000 MiB), cold / warm | 2.47 / 2.55 tok/s | 19.7 / 7.9 s |

  Corsia is 2.4 times stock for a model that fits neither the GPU nor the RAM, and the warm pool makes the prompt 2.6 times faster. The gap to the 72.7 tok/s of a model that fits is the case for the GPU expert pool: here the experts are computed on two Xeon cores
- Idea for GPUs: the pool and the pointer table live in the CPU backend. A CUDA version (experts in a pool in GPU memory, filled over PCIe from Corsia) would bring the same to models bigger than the GPU's memory
- [x] Pool flaw found by agent mode (2026-11-23): a large prompt batch was slower with the pool (side request 154.8 s against 101.0 s): a batch touches nearly every expert, more than the pool's 1,935 slots, so the pool churned. Fix in the fork working tree (not yet built or measured): a layer missing more than `CORSIA_POOL_BATCH_MAX` (32) experts loads none into the pool and fetches them through the page cache as before; small batches still load into the pool

## Speculative decoding (2026-11-23, dev box, 6 GiB cap, pool 3,072 MiB, llama-server, 256 tokens of a copy-and-edit request on real Corsia code)

| Arm | Generation | Drafts accepted |
| --- | --- | --- |
| No drafting | 2.06, 2.03 tok/s | |
| n-gram `ngram-simple`, verification batches bypassing the pool | 1.21 tok/s | 224 of 224 |
| n-gram `ngram-map-k`, same | 1.18 tok/s | 227 of 227 |
| n-gram `ngram-simple`, verification batches loading into the pool (`CORSIA_POOL_BATCH_TOKENS` 64) | 1.74 tok/s | 219 of 219 |
| Draft model Qwen3-0.6B Q8_0 (`--spec-type draft-simple`) | 1.06 tok/s | 188 of 200 (94%) |

- Drafts were near perfect, but on two compute-bound cores verifying 16 tokens costs about 16 tokens of compute, and a second model costs more: speculation stays off on machines like this one. It is for machines with spare compute where memory bandwidth limits (many cores, GPUs); testbox's model-in-RAM run tests that.
- The first runs exposed that verification batches (16 tokens) tripped the prompt-batch pool bypass (12,300 experts around the pool, hits 94% to 85%). Bypass now applies only to batches over 64 tokens. Verification batches loading into the pool churn it (47% hits, 26 GB read) yet still beat the page-cache route here.
- A draft model needs `--spec-type draft-simple` as well as `-md` in this build.
- Test-script race found and fixed on the way: a stale serve log let `CORSIA_SOCKET` come back empty (silently stock llama.cpp); the scripts now delete it first and fail if the socket is missing. Corsia now also refuses to take over a live server's socket (found by agent mode); 17 tests pass.

## testbox, spinning disk (Hitachi 320 GB), 6 GiB cap, Qwen3-30B-A3B Q2, 32 tokens

| Arm | s per token | Prompt (10 tokens) | Pool hits |
| --- | --- | --- | --- |
| Guessing, page cache (baseline) | 5.04 | 96 s | |
| Expert pool, cold (first run, no profile) | 2.13 | 108 s | 81% |
| Pool + background loader, warm (profile warm-up 98 s from the disk) | 2.08 | 81 s | 88% |

Round two: baseline 5.21, pool warm **1.61** (prompt 39 s, 92% hits), pool + loader 2.59 s/token. The warm pool makes the spinning disk **3.2 times faster**; the background loader hurts on a spinning disk (guesses compete for the same head as demand reads) and stays off.

### testbox, model in RAM (no cap), llama-server, 4 cores, same copy-and-edit request, 256 tokens

| Arm | Generation | Drafts accepted |
| --- | --- | --- |
| No drafting | 7.49, 7.50 tok/s | |
| n-gram `ngram-simple` | **16.70 tok/s** | 227 of 227 |
| n-gram `ngram-map-k` | 16.68 tok/s | 227 of 227 |

Speculation pays where compute is spare: **2.2 times faster** on copy-heavy coding work, no extra model, and no change to the output (accepted drafts are the tokens the model would produce). The dev box (two compute-bound cores) is the opposite case. For agent mode with the model in RAM: run llama-server with `--spec-type ngram-simple`.

### The laptop's WD (expert-major session, round 1, 6 GiB cap)

| Layout | s per token |
| --- | --- |
| File layout | 2.78 |
| Expert-major copy | 2.16 |
| File layout + pool | 1.16 |
| Expert-major copy + pool | **1.00** |

Both spinning-disk fixes stack: 2.8 times faster than neither; the pool's warm-up is 39.8 s on the expert copy against 54.5 s (one read per expert instead of three).

### Bisect: did today's fork slow the plain path? (2026-11-23, dev box SSD, 6 GiB cap, same Corsia binary, cool starts)

| Fork | Run 1 | Run 2 | Mean |
| --- | --- | --- | --- |
| 43028c0 (this morning, verified) | 1.59 | 2.01 | 1.80 tok/s |
| cf6944b (today's loader and batch fixes) | 1.60 | 1.89 | 1.75 tok/s |

No: the plain demand-prefetch path is the same in both (3% is run-to-run spread). The first run of each pair is slower with more major faults (2,566 to 2,838 against about 1,500). The slower plain file-layout figure on the WD (2.61 against 2.10 s/token) therefore comes from outside the fork: Corsia's read planning on that disk or the drive after two NTFS repairs; still open.

The gap commit was checked too: 17224fb ran 1.66 and 1.72 tok/s, the same as both sides of it. No commit today slowed the plain path.

### Live model panel in `corsia watch` (2026-11-23)

`corsia agent` streams what the model writes on `/api/agent/live` (Server-Sent Events: `start`, `think` and `text` pieces, `end` with tokens, speed and prompt reuse); `corsia watch --agent HOST:PORT` shows it in a panel under the drives: the thinking muted, then the answer, the last lines that fit. Checked with the real model on the dev box (6 GiB cap, pool): 40 events for a 40-token answer, the thinking arriving word by word, the end line "40 tokens, 2.52 tok/s, prompt 11 new + 3 reused". Not yet on testbox (waits for agent mode's run to finish).

### More memory: 10 GiB cap against 6 GiB (2026-11-23, dev box SSD, pool, swapping on, llama-server, two 256-token answers, temperature 0)

| Cap | Pool | Experts held | Start temp | Decode | Prompt |
| --- | --- | --- | --- | --- | --- |
| 6 GiB | 3,584 MiB | 1,935 (31%) | 89 C | 2.72, 2.68 tok/s | 1.36, 1.41 tok/s |
| 10 GiB | 7,680 MiB | 4,147 (67%) | 56 C | 3.78, 3.80 tok/s | 2.78, 3.84 tok/s |
| 10 GiB | 7,680 MiB | 4,147 (67%) | 75 C | 3.83, 3.78 tok/s | 3.83, 3.80 tok/s |

Decode is 41% faster and prompts 2.7 times faster with 4 GiB more; temperature made no difference at 10 GiB. Pool warm-up takes 23 s against 11 s. Speed grows with the share of experts held, so testbox (38 GB, spinning disk) should give the pool 12 GiB or more there. Script: `~/.cache/corsia-build/cap-bench.sh CAP_GIB POOL_MIB LABEL`.

### The pool sizes itself (2026-11-23, fork 1190ceb, patch 0011)

`CORSIA_POOL_MIB` now defaults to `auto`: the pool takes the cgroup's memory cap (or the memory free at start, less a tenth of the machine and at least 1 GiB for everything else), less the weights that are not experts, the KV cache (from the model's metadata and the context and cache types asked for) and 1,152 MiB for compute buffers, the program and Corsia. It never takes more than every expert. Under a 6 GiB cap it picks 3,606 MiB and under 10 GiB 7,702 MiB, the sizes tuned by hand. Measured with a first version (1,280 MiB reserve): 10 GiB 3.87 and 3.72 tok/s (the same as by hand); 6 GiB 2.26 and 2.46 (by hand 2.72 and 2.68, a 3% smaller pool, most likely spread). `0` turns the pool off; a number still fixes it.

### testbox at 16 GiB (2026-11-23, spinning disk, auto pool, llama-server, two 256-token answers)

| Model | Pool | Decode | Note |
| --- | --- | --- | --- |
| Qwen3-30B-A3B Q2 (11.8 GB) | 5,556 of 6,144 experts (a cap bug, fixed in 1190ceb) | 4.49, then **6.64 tok/s** | warm-up 4.7 GB from the disk in 151 s |
| Qwen3-Coder-30B Q4_K_M (18.6 GB) | 4,714 of 6,144 | killed for memory | see below |

Q2 at 16 GiB: 10 times the 6 GiB pool on the same disk (0.62 tok/s) and within 12% of the model in RAM (7.5). The Q4_K_M run found a real fault: llama.cpp repacks Q4_K weights for AVX2 at load, which read the whole 18.6 GB file (13 minutes on the disk) into 14.6 GB of anonymous memory beside the pool, and the pool cannot serve repacked experts anyway. Fork c1b2d50 (patch 0012) turns repacking off while streaming (`CORSIA_REPACK=1` keeps it). Q2 models were never repacked, so earlier results stand.

### Coding assistants on the dev box (2026-11-23)

`integrations/llama.cpp/local-agent.sh start|stop|status|logs` runs `corsia serve`, the fork's llama-server (pool sized from memory, 32k context, two slots, snapshots in `~/.cache/corsia/kv`) and `corsia agent` on port 7720 as one systemd user service (`corsia-local`) under `CAP` GiB. opencode 1.19.6 is set up with a `corsia` provider and a lean `local` agent (read, edit, write, glob, grep, bash). With a browser open the pool took 3,923 MiB of the 10 GiB cap: the free-memory rule kept a tenth of the machine for the desktop.

A small coding agent (`~/.cache/corsia-build/agent-flow/`, four tools, a buggy sandbox project) fixed the bug in 4 steps and 15 minutes under a 10 GiB cap, prompt reuse 57 to 91% between steps; the live panel now also shows the question or tool result each step answers and the tool calls (`→ run_tests {}`). The model said the tests passed without running them (they did pass).

### Past the CPU: testbox's iGPU (2026-11-23, HD 530 through Vulkan, Mesa ANV, Qwen3-30B-A3B Q2 in RAM)

The iGPU reaches 26 GiB of memory, has fp16 and int8 storage, but no 8-bit dot product (`int dot: 0`). The Vulkan build needs `libvulkan-dev glslc spirv-headers` and `-DLLAMA_USE_PREBUILT_UI=OFF`.

| Where | Writing (128 tokens) | Prompt (2,048 tokens, 1,024-token batches) |
| --- | --- | --- |
| CPU, 4 cores | 8.29 tok/s | 38.9 tok/s (37.6 at 512-token batches) |
| All on the iGPU | 4.00 | 50.9 (2,048-token batches: the GPU hang check reset it) |
| Attention on the iGPU, experts on the CPU | 3.31 | |
| Half the layers on each | | 11.8 (no pipelining, copies) |

Handing work between CPU and iGPU costs about 3.7 ms each time, so splitting inside a token loses. An earlier "CPU 10.6 tok/s prompt" figure came from the Vulkan build with `-ngl 0`, which ships prompt batches to the iGPU and copies the weights each time: use `--no-op-offload` (or the CPU build) for CPU figures.

Drafting, 256 tokens, temperature 0 (copy-and-edit request / fresh writing):

| Arm | Copy-and-edit | Fresh writing |
| --- | --- | --- |
| CPU alone | 8.25 | 10.25 |
| n-gram drafts | **15.68** (227/227) | 10.21 (no drafts) |
| 0.6B draft model on the CPU | 7.67 (93%) | 8.95 (72%) |
| 0.6B draft model on the iGPU | 4.77 (95%) | 5.05 (72%) |

On a CPU only free drafts pay: a draft model costs more than it saves even at 95% acceptance, and the iGPU makes it worse. This iGPU's only gain is reading prompts (1.3 times the CPU) through a whole handoff with a second copy of the model. A newer iGPU with 8-bit dot products (Intel Xe, AMD RDNA) is a different case. Next levers: drafts built into the model (multi-token prediction heads; testbox's Qwen3.6 GGUF from unsloth has none), drafts from the whole project (an n-gram table of the repository), and hybrid-attention models (Qwen3.6-35B-A3B, mostly linear attention) for long agent contexts.

### Project-wide drafts (2026-11-23, fork 29083a0, patch 0013; `corsia agent --corpus`)

Idea: free n-gram drafts from the whole project, not only the context. Simulated on real history first (each commit's added lines as the output, the edited file before the commit as the context, the rest of the repository at an older snapshot as the corpus; ngram-simple-style drafting, rough tokens):

| Repository | Lookup 12: context / + project | Lookup 6: context / + project |
| --- | --- | --- |
| vorynza-cloud (TypeScript, 120 commits, 211k tokens written) | 8.9% / 14.6% (+64%) | 18.3% / 30.5% (+67%) |
| speech-to-speech (Python, 300 commits, 353k tokens) | 12.2% / 17.3% (+42%) | 25.8% / 36.3% (+41%) |

Built: `corsia agent` reads the working directory from the assistant's prompt, tokenizes the project's source files through llama-server's `/tokenize` in the background (1,149 files, 1.0 M tokens in 6 s) and writes `~/.cache/corsia/corpus/current.tok` (i32 tokens, -1 between files); the fork's ngram-simple looks there when the context has no match (`CORSIA_DRAFT_CORPUS`). Confidence gate: of up to 8 occurrences the one agreeing furthest back is used, only past `min_match` tokens of agreement, the draft no longer than the match, and `min_match` grows while project drafts are rejected.

Dev box, 10 GiB cap, streamed, ngram-simple N=8 M=16, writing a routes file whose pattern lives in a sibling file not shown (400 tokens, temperature 0, identical answer in every arm):

| Arm | Speed | Drafted | Accepted |
| --- | --- | --- | --- |
| Context-only drafts | 2.37 tok/s | 374 | 196 (52%) |
| + project, first version (last occurrence, 8-token match) | 1.53 | 725 | 227 (31%) |
| + project, confidence gate | 1.98 | 422 | 196 (46%) |

The gate cut wasted drafts from about 500 to 48, but the model wrote the routes in its own style rather than the project's, so the project added nothing here; the dev box is also the worst case for drafts (two cores, streamed experts). Still to do: replay a recorded opencode session on testbox (model in RAM, where n-gram drafts already gave 1.9x) with and without the project, and tune N and M there (the tuning run was cut off when testbox went offline).

### Past the CPU on the dev box (2026-11-24, i5-2450M: AVX only, no AVX2, FMA or F16C; 10 GiB cap)

At 10 GiB the dev box is compute-bound: 99% of chosen experts were already in the pool and the drives added about 5 MB per token (about 10 of 263 ms). Per token, from kernel timings and the model's shapes: experts about 120 ms, attention projections about 70, the output layer (Q6_K, 151,936 x 2048) about 31, the rest about 40.

Expert matmul per format on this CPU (`test-backend-ops perf`, 8 of 128 experts, 768 x 2048, one token): q2_K 776 us, q4_K 826, q4_0 912, iq4_xs 1008, q8_0 1067, q6_K 1101, **q3_K 1280**, iq2_xs 1573, f32 3216, f16 7360. Q2_K is already the fastest; Q3_K is the slow one, and the model (Unsloth UD-Q2_K_XL) stores 37 of its 48 down projections in it.

**Per-CPU formats** (`integrations/llama.cpp/cpu-tune.sh`): times each format on the CPU it runs on, rewrites each tensor into the fastest format with at least its bits per weight (at most 1.35 times the bytes, at least 1.10 times faster), copies everything else byte for byte with `llama-quantize --tensor-type-file`, and carries the pool's usage profile over. Here: the 37 Q3_K down tensors to Q4_K, 11,267 to 12,211 MiB, written in 7.4 minutes. Pool slots were already sized for the 11 Q4_K down layers, so the pool holds the same 4,159 experts.

| 10 GiB, auto pool, two 256-token answers | Run 1 | Run 2 | Mean |
| --- | --- | --- | --- |
| Original | 3.84, 3.91 (68 C start) | 3.62, 3.46 (92 C) | 3.71 tok/s |
| Tuned copy | 4.02, 4.27 (91 C) | 4.02, 4.13 (89 C) | **4.11 tok/s (+11%)** |

Quality against the original (256-token chunks, 3 chunks): perplexity 8.3454 / 8.3459, KL divergence 0.0085, same top token 94.8% (residency-aware routing, on by default, is 0.012). The re-rounding starts from the 3-bit values; from the full-precision weights Q4_K would be strictly better. `local-agent.sh` now uses `<model>-cpu.gguf` when it exists.

Tried and dropped, all measured:
- A rewritten AVX Q3_K dot product (one multiply-add per 16 weights, constant through bsums): correct, no faster (367 to 385 against 353 to 443 ns per row).
- 2 MiB huge pages for ggml buffers: 10 times slower in the kernel test; expert reads are sequential, so translation is only a few percent in theory; the pool (shared memory, `shmem_enabled=never`) cannot get them without root.
- 4 threads (hyperthreading): 4 to 8% slower than 2 in every format.
- Spinning waits (CORSIA_COOL=0): 4.14 against 4.09 tok/s, noise: cool waiting stays.
- Power limits: PL1 is 35 W, already the firmware maximum. The clock holds 2.9 GHz up to 76 C; it throttles above about 90 C, which long runs reach (93 C seen).

Free n-gram drafts at 10 GiB with the tuned model (llama-server, 256 tokens, temperature 0): copy-and-edit request 2.77 tok/s without, **3.55 with `--spec-type ngram-simple`** (defaults N=12 M=48, 227 of 227 accepted, +28%), 2.80 with N=8 M=16 (87%); fresh writing 3.76 without, 4.06 with the defaults (no drafts made), 3.56 with N=8 M=16 (6% accepted). Under the 6 GiB cap drafts lost on this machine (every verify pass pulled experts off the SSD); with 99% of experts in the pool they win. `local-agent.sh` now runs with drafts on (DRAFTS=0 turns them off); project drafts stay opt-in (PROJECT_DRAFTS=1).

### Drafts that earn their length, grouped decode attention (2026-11-24, fork 82f25d7 and c9d094f, patches 0014 and 0015)

Fixed 48-token n-gram drafts collapsed in a real agent session on the dev box: 9 and 17% of drafted tokens accepted in thinking text, one step at 0.89 tok/s, the session 2,347 s against 900 without drafts. Drafts now start at 8 tokens, double after a fully accepted one (up to 48) and drop to one more than what was accepted after a rejection (`CORSIA_DRAFT_ADAPT=0` keeps the fixed size). On testbox's agent session: 57 to 64% accepted, 4 to 8 tok/s, no collapse. A clean benchmark on the dev box is still to do (the first try ran beside another session's Rust build).

Agent sessions slow down as the conversation grows (testbox: 7.8 tok/s at step 2, 4.2 at step 5, about 6,000 tokens of context): attention. With a q8_0 cache, ggml's CPU decode takes the plain per-head loop (the split path needs an f32/f16 cache, the tiled one several query tokens), so each KV head is read and its V rows converted once per query head: 8 times in Qwen3-30B-A3B. The grouped path walks each KV head once for all its query heads, with each head's operations unchanged, so the output is identical (Qwen3-0.6B, 1,594-token prompt, 200 tokens at temperature 0: same 825 bytes; 6.24 against 5.34 tok/s, +17%, with only 2 query heads per KV head). Kernel benchmark (q8_0 cache, 12 query heads per KV head, dev box, heat between 69 and 95 C): 4k context 4.1 to 6.2 ms against 6.3; 16k 4.7 to 5.9 against 9.6 to 10.4; 32k 6.4 to 9.5 against 12.8. Still to measure: Qwen3-30B decode at long context on a quiet machine.

Also measured: f16, llama.cpp's default KV cache type, is 4 to 6 times slower for attention than q8_0 on CPUs without F16C (this dev box): Corsia's scripts use q8_0. Prompt reading on the dev box runs near 9% of the cores' int8 peak; ggml's AVX-only tiled kernel reuses each unpacked weight slice over 4 tokens; a Q2_K pre-scaled tile (2-bit value times 4-bit scale fits a byte) would save about one operation in six. Not built yet; measure the expert share of prompt time first.

Lessons from the noisy day: the dev box reaches 94 C and throttles thousands of times in long runs, and the Claude desktop app takes half a core during long sessions; long benchmarks belong on testbox, and nothing else may run beside a timing test.

### Where the time goes, and prompt attention fixed (2026-11-24, fork 53913a8 to aa9e479, patches 0016 to 0018)

**Op timer** (`CORSIA_OP_PROFILE=1`, patch 0017): the main thread times each graph node up to the barrier after it and prints the totals per op and weight at exit. testbox, Qwen3-30B-A3B Q2 (tuned copy), 4 threads, q8_0 cache, llama-bench:
- Prompt (512 tokens, 33.5 tok/s): flash attention **23.6%**, down experts 19.9, gate experts 14.9, up experts 14.7, attention output 8.8, attention q 8.7, everything else under 2% each. By arithmetic attention is about 5% of the work at 512 tokens, so it ran 4 to 5 times less efficiently than the matmuls, and it grows with the square of the context.
- Decode (64 tokens, 10.2 tok/s): down experts 19.3%, **output layer 15.2%** (14.7 ms a token for the 151,936 x 2048 Q6_K vocabulary), attention output 14.8, up experts 13.7, attention q 13.6, gate experts 13.2, router 2.9, attention 1.1.

**Tiled prompt attention over a quantized cache** (patch 0018): with a q8_0 cache, prompt attention took ggml's per-row path, where each query row walks the whole cache alone and converts each V row again (512 times for a 512-token batch). The tiled path, which converts each 64-row K and V tile to f32 once and multiplies it against 64 query rows, only took f16 and f32 caches; it now takes any cache type with a row converter. `CORSIA_FA_TILED_QUANT=0` keeps the old path.

| testbox, 30B, llama-bench, two rounds each | Old path | Tiled |
| --- | --- | --- |
| 512-token prompt | 33.5, 33.6 | **40.6, 41.2 tok/s (+22%)** |
| 2,048-token prompt | 20.0, 19.9 | **35.8, 36.0 (+80%)** |
| 512 tokens after 4,096 of context | 7.0, 6.9 | **21.3, 21.5 (3.1x)** |
| dev box, Qwen3-0.6B, 2 threads: 512 after 2,048 | 5.15, 5.22 | **16.8, 16.6 (3.2x)** |

Quality against an f16 cache (llama-perplexity, 3 chunks of 2,048 tokens of llama.cpp C++): old q8_0 path mean KL divergence 0.00293, maximum 0.736, same top token 98.57%; tiled 0.00280, maximum 0.355, 98.89%. Perplexity 2.4158 and 2.4176 against 2.4126 (±0.10). The tiled path works in f32 instead of rounding queries to q8_0, so it sits a little closer to the reference.

**Built without OpenMP** (patch 0016, `scripts/corsia-build.sh`, `OPENMP=ON` for the old build): testbox decoded 15 to 23% faster with ggml's thread pool; on the dev box the race (two rounds each, alternating) gave 0.48 against 0.46 tok/s, same direction. `local-agent.sh` uses `build-noomp` when it exists.

That dev box race also found the real limit of the day: a browser (5 GB), an editor (2.5) and two assistant apps (3.3) left 3.9 GB available, so the auto pool, which takes the smaller of the cap and free memory less a reserve, came out at 0 MiB and every expert streamed from the SSD (0.46 tok/s against 4.1). The sizer was right not to take the cap, which would have pushed the model's own pages out. On a laptop people also use, free memory matters as much as the cap.

Next levers seen in the profile: the output layer (15% of decode; a smaller format or a cheaper first pass over the vocabulary), and the attention projections (q and output, 28% of decode together).

### Long-context attention: decode, draft checks, short tool results (2026-11-24, fork 05e7b7a to e73c867, patches 0019 to 0021)

Grouped decode attention on Qwen3-30B-A3B (testbox, llama-bench, 4 threads, q8_0 cache): after 8,192 tokens of context, single-token decode **4.28 and 4.29 tok/s against 2.17 and 2.18** for ggml's per-head loop (1.97x). Decode-path KL divergence against an f16 cache (llama-perplexity with a batch of 1, so every token takes the single-token kernel): the grouped loop and the per-head loop gave the same numbers to every digit (KL 0.002509, perplexity 1.909097), as designed.

Op timer at that depth (tags "1" for single-token nodes): attention is 116 ms of a roughly 208 ms token (56%); in the 8,192-token prefill, tiled attention is still 50% of prefill time, at about 31% of the cores' f32 peak.

Tried for decode, kept opt-in or dropped:
- Blocked softmax (64 rows, one vectorised softmax and rescale per head, V rows as one small matrix product; `CORSIA_GQA_BLOCK=1`, patch 0019): 4.85 and 4.86 against 4.63 and 4.60 tok/s (+5%). KL 0.0038 ± 0.0012 against 0.0025 ± 0.0006 on 1,024 tokens, and maximum KL 0.53 against 0.24 at one position: within noise on the mean but not a clean win, so it stays opt-in.
- Scoring all 8 heads with one f32 matrix product over a converted, transposed K block: 4.34 and 4.30 against 4.72 and 4.70: slower (the transpose writes at a 256-byte stride and the generic product handles 8 rows as 6 plus 2). Reverted. A dedicated 8-row by 8-head kernel with a transposed reduction is still possible.

**Short batches** (patch 0021): the tiled path needs 64 query tokens and grouped decode took exactly one, so every batch of 2 to 63 tokens (each n-gram draft check, each short tool result or message) took the per-row loop, which reads the whole cache once per query head and token (at 8k context four KV heads are 8.8 MB, past testbox's 8 MB L3). Grouped attention now takes those batches, each KV head read once for its 8 heads and all the tokens, each token with its own mask row; with more threads than KV heads the tokens are split between threads. Output identical (Qwen3-0.6B, batches of 16: the same KL and perplexity to every digit as the old path against its own saved output).

| testbox, 30B, after 8,192 tokens | Old path | Grouped |
| --- | --- | --- |
| 16-token batch | 3.11, 3.18 | **8.01, 7.94 tok/s (2.5x)** |
| 48-token batch | 3.25, 3.21 | **8.18, 8.20 tok/s (2.5x)** |

The old numbers explain the draft losses in long agent sessions: checking a batch of drafts at depth ran at 3.1 tok/s, slower than plain decode (4.7), so every draft check past a few thousand tokens cost more than it could earn. Next lever here: batches of 8 or more tokens give 64 to 504 (token, head) rows per KV head, enough for the tiled matrix product (each K and V tile converted once for every head and token of the group).

### Tiled grouped attention for short batches (2026-11-24, fork 225c529)

Batches whose KV group has 64 or more (token, head) rows (8 or more tokens in Qwen3-30B-A3B) now take a tiled product: each 64-row K and V tile is converted once and multiplied against every row of the group (`CORSIA_GQA_TILED=0` keeps the row loop). testbox, 30B, after 8,192 tokens, two rounds each: 8-token batch 12.0 against 7.0 tok/s, 16-token 14.6 against 7.9, 48-token 16.9 against 8.1 (the per-row path this morning: 3.1 to 3.2). Quality on Qwen3-0.6B against an f16 cache, batches of 48: KL 0.00088 against 0.00108 for the row loop, same top token 99.80% against 99.61%. The 30B KL check was cut short by an agent session and is still to run.

Real agent session on testbox with all of it (Coder Q4_K_M, thinking off, 24 GiB cap): the bug fixed in 142 s, tests passing. The step that read 541 new tokens took 17.7 s (30.6 tok/s) against 41.6 s (13.0 tok/s) in the same session the day before.

### Cold start: the experts were read twice (2026-11-24, fork eff76cc)

A cold start of the Coder Q4_K_M model on testbox (spinning disk, 24 GiB cap) took about 26 minutes and Corsia served 36.8 GB for an 18.6 GB file: the pool read the experts straight from the drive, then llama.cpp's loader mapped the whole file with MAP_POPULATE and pulled every byte through the mount again. With `CORSIA_SOCKET` set the loader now maps the expert tensors as lazy ranges, so only the other weights (about 1 GB) are prefetched (`CORSIA_PREFETCH_ALL=1` keeps the old behaviour). Same cold start: **484 s, 17.5 GB served**, the first 64-token answer 12.6 s later. All of the 484 s is now the pool warming at the disk's 36 MB/s.

### Uneven memory: the half-speed part (2026-11-24, `integrations/llama.cpp/membw.c`)

A read-bandwidth probe (4 threads, 64 MiB chunks, no root) shows testbox's memory has two speeds: of 29 to 31 GiB measured, about 13 GiB reads at 14 to 16 GB/s and about 8 to 10 GiB at 26 to 30 GB/s, the rest mixed. It has 40 GB (a 32 GB and an 8 GB module): only the matched 16 GB runs dual-channel. Huge pages change nothing for streaming (27.3 against 26.9 GB/s). The dev box is even (about 17 GB/s everywhere, ratio 1.2) and uses about 5 GB/s of it while decoding: its limit is arithmetic, not memory.

Decode depends on which part the weights sit in (llama-bench, Qwen3-30B-A3B tuned copy, 4 threads; the probe holds one kind of memory so the model's page cache lands in the other):

| testbox | Prompt (512) | Decode (64) |
| --- | --- | --- |
| Model in the fast memory | 41.9 tok/s | **11.00 tok/s** |
| Model in the slow memory | 42.0 tok/s | 9.23 tok/s |
| Left to chance (the morning's runs) | 40.6 to 41.2 | about 10.2 |

Nothing else was capped: turbo on, 3.7 to 3.9 GHz, no thermal throttling counted, the power limit at the chip's rated 65 W.

Wired in: `membw guard N` takes the free memory, gives the fast chunks back and holds the slow ones, letting go 1 GiB at a time (fastest first) when free memory drops under 1 GiB. `local-agent.sh start` runs it when setup measured uneven memory (`GUARD=0` turns it off) and tells the pool sizer what it holds (`CORSIA_GUARD_MIB`). The fork reads the weights every token uses before the pool fills (`CORSIA_FIRST=0` turns it off) and warms the pool in four tiers of use (`CORSIA_WARM_TIERS`), so the fast memory goes to what is read most.

### Setup for any machine (2026-11-24, `local-agent.sh setup`)

One step per machine and model: reports the processor's instruction sets, cores, memory and the model's drive; builds corsia and the fork when missing (`TESTS=ON scripts/corsia-build.sh` for the tuner's programs); writes the per-CPU copy of the model (`TUNE=0` skips); measures the memory map; saves `~/.config/corsia/machine.env`. `start` now picks the cap itself (what the model needs, or what is free less a tenth of the memory, at least 2 GiB) and says so when that is less than the model needs: the hard-coded 10 GiB default gave a pool of 0 MiB on a morning when other programs held 12 GB.

Tried and dropped today: skipping low-weight experts again (already failed the gate on 2026-11-23: KL 0.148 at a 0.1 threshold, and a skipped slot still runs its matmul).

### Table-lookup kernel for 2-bit experts: prototyped, dropped (2026-11-24, dev box)

The dev box is arithmetic-bound (5 GB/s of weights against 17 GB/s of memory), so a cheaper 2-bit kernel was tried standalone (`~/.cache/corsia-build/lutq2.c`, linked against the fork's ggml): rows in groups of 16, each pair of 2-bit weights a 4-bit index into a table of the 16 possible partial sums of the input (built once per input vector, shared by a layer's 16 expert matrices), `pshufb` looking 16 rows up at once, entries split into low 6 bits and a signed high part so the result equals ggml's own Q2_K kernel (checked: largest difference 0.00004 on outputs up to 158). Same 2.75 bits per weight. Single thread, 768 x 2048: 0.073 against 0.092 ns per weight with the weights in the third-level cache (1.26x), 0.095 against 0.099 streaming 64 MiB from memory (1.04x). Sandy Bridge has two ports for integer vector work and both kernels fill them; the estimate of 2x counted four operations a cycle. Not worth a new weight layout. A lossy variant (8-bit tables, half the lookups) was not tried.

What is left on the dev box is fewer weights per token (drafts; the output layer's 311 M of the 3,030 M weights a token reads) and heat (cool starts measured about 10% faster than starts at 92 C).

### Worker wake-ups: a false lead (2026-11-24, dev box and testbox)

The dev box's op timer showed 69 us per ADD and 408 us per ARGSORT, and a first pair of runs gave 1.28 and 1.42 tok/s with cool waiting against 2.35 and 2.82 with `CORSIA_COOL=0`. That looked like the cost of putting workers to sleep between the roughly 144 groups of nodes a token's graph is split into by the eval callback. Two changes were written for it (a timed 200 us poll before a worker sleeps; a barrier that naps after a short spin) and then tested properly, five alternating arms on each machine:

| Dev box, 10 GiB cap, auto pool | Pool | Read from the SSD | tok/s |
| --- | --- | --- | --- |
| Sleep at once (first arm) | 3,978 MiB | 24.3 GB | 1.79, 2.74 |
| 200 us poll and nap | 4,628 MiB | 17.3 GB | 3.34, 3.54 |
| `CORSIA_COOL=0` | 4,729 MiB | 15.6 GB | 3.47, 3.13 |
| 200 us poll and nap | 4,658 MiB | 18.4 GB | 2.98, 3.07 |
| Sleep at once | 4,750 MiB | 17.5 GB | 3.35, 3.64 |

With equal pools the old behaviour is as fast as the new. The slow arm was the first one both times: free memory was lower then, so the auto pool was smaller and more experts came from the drive, and the slow nodes in the profile were other programs taking the two cores. On testbox (in use by its owner at the time) the two changes together were slower: 9.65, 9.30, 9.86 against 10.33, 10.63, 10.86 tok/s. Both changes were reverted. Lessons: fix the pool size by hand when comparing on a machine whose free memory moves, and never trust a first arm.

Two bugs found on the way and fixed: `CORSIA_COOL=0` also turned weight repacking back on, because the streaming check read the same variable (6.7 GB of anonymous memory, the service killed at its 16 GiB cap; fork acb2a9a), and `local-agent.sh start` could not recover from the stale mount a killed run leaves behind (it now always unmounts first).

### Guard from an unlucky start; a low-rank output preview does not work (2026-11-24, testbox)

Service decode (Qwen3-30B-A3B tuned copy, 16 GiB cap, three 128-token requests) from the same unlucky state, slow memory freed last: 9.70, 9.91, 9.83 tok/s without the guard, **10.83, 11.28, 11.14 with it**. Back-to-back starts reach 11.3 to 12.3 either way, because a start inherits the memory the last one freed.

Two-stage output layer, checked offline before writing any kernel (`~/.cache/corsia-build/headprev.c`: the output matrix dequantized, its top 512 singular directions by orthogonal iteration, real hidden states from `llama-embedding --pooling none` over 1,627 tokens of C++): the matrix is not low-rank. The top 64, 128, 256 and 512 directions hold 15, 20, 29 and 43% of its energy, and the exact best token (19.5 above the mean logit, spread 2.4) ranks at a median of 20,025 in a 256-direction preview and 7,451 at 512; keeping 16,384 rows still misses it in 843 and 703 of 1,627 positions. A preview by projection is out for this model. Not tried: a low-precision copy of the whole matrix as the preview (2 bits, about 80 MB against 243 MB a token), which keeps every direction.

The decode thread-count test (2, 3 and 4 threads) did not run: its wait loop matched its own command line.

### Sparse V accumulation in decode attention: measured, dropped (2026-11-24, testbox)

Idea: at long context, score every cached row first, then do the V work (convert the row, an exponential and a 128-value weighted add per head) only for rows within T of each query's best score. Measured on real text (7,004 tokens of C++ as the prompt, 96 tokens written, Qwen3-30B-A3B, 4 threads, q8_0 cache), with counters on:

| Threshold | Pairs kept | V rows needed | Decode |
| --- | --- | --- | --- |
| Row loop as it is | all | all | 4.86 tok/s |
| Two passes, nothing skipped | 100% | 100% | 4.53 |
| 17 (under what a float can add) | 86.5% | 99.0% | 4.27 |
| 12 | 42.8% | 81.9% | 4.41 |
| 8 | 7.7% | 27.1% | 5.04 |

Attention here is not sparse on the scale that matters: 86.7% of (row, head) pairs sit within 17 of the best score, so the safe threshold skips little, and one V row serves 8 heads, so nearly every row is still needed. Even dropping 92% of the V work gains 4%: the V work is the small part, and scoring every cached row against 8 heads (plus streaming the cache) is the cost. Code removed. What would help at depth is a cheaper K scoring kernel, which the one-product attempt earlier today did not deliver.

### Two-stage output layer and shared K scoring (2026-11-24, testbox; fork f28ca04 and working tree)

**High-bits preview of the output layer** (`CORSIA_HEAD_PREVIEW=N`, opt-in, in the fork's working tree, not committed). Q6_K stores each weight's low 4 bits and high 2 bits in separate arrays. A first pass over the high bits alone (the low bits taken as their middle value) ranks the vocabulary; the N best rows are then scored exactly and the others keep their preview value. Checked offline first (`headhi.c`, 1,627 real hidden states): the exact best token never ranked below 133 in the preview, the exact top 5 never below 467, the top 20 never below 1,607. In the model (llama-perplexity, batches of one token, 3 chunks of 1,024, against the exact layer):

| Rows scored exactly | Mean KL | Maximum KL | Same top token |
| --- | --- | --- | --- |
| 1,024 | 0.000428 | 0.0194 | 100% |
| 4,096 | 0.000032 | 0.0027 | 100% |
| 16,384 | 0.000002 | 0.0004 | 100% |

Speed is not there yet. Reading the high bits in place, the first pass cost as much as the whole matrix (16.4 ms a call either way; 17.1 to 17.6 ms with 4,096 rows rescored, 19.9 with 16,384): the output layer runs at memory speed on this machine (243 MB in 16.3 ms is 15 GB/s) and the prefetcher pulls the skipped low bits through memory anyway. A version that copies the high bits once into their own array (82 of every 210 bytes, about 100 MB) is written and was queued for timing when testbox went offline.

**One K row scored for all its query heads** (fork f28ca04, `CORSIA_Q8_SCORES=0` turns it off; AVX2 with FMA only): in the grouped decode loop each K row's blocks are loaded, made absolute and their scales converted once instead of once per query head, with ggml_vec_dot_q8_0_q8_0's arithmetic in the same order. Same text at temperature 0 over 128 tokens after a 7,004-token prompt (twice). Decode there: 4.90 against 4.56 tok/s, then 4.41 against 4.38 (the machine was in use: between nothing and +7%). The KL check against the per-query calls was cut off when testbox went offline.

Reading these with the sparse test: at about 7,000 tokens of context roughly half of attention's time is neither scoring nor V arithmetic but streaming the KV cache (about 365 MB a token), which is allocated after the weights have taken the fast memory. Next idea: hold fast memory for the KV cache during the load (reserve it after the hot weights, release it just before the context is created).

**Waiting for testbox (it went offline at 17:56 on 2026-11-24, mid-run):** in the fork's working tree, built on the dev box and not committed: the output preview with its own high-bits array (`ggml-cpu.c`; timing script `head2-sq.sh` was queued on testbox), and a hold for the KV cache (`corsia-experts.h`, `CORSIA_KV_FIRST=0` turns it off: the cache's size is taken right after the hot weights and given back when the pool is warm, so with the guard the cache lands in fast memory; never run). Still owed: the KL check of shared K scoring, the 30B KL check of tiled grouped attention for short batches (on by default since 225c529), and decode with 2, 3 and 4 threads.

### Two-stage output layer: faster with its own array (2026-11-25, testbox; fork 4fa5a97, patch 0030)

With the high bits copied once into an array of their own (82 of every 210 bytes, about 100 MB), llama-bench on Qwen3-30B-A3B, 4 threads, repacking off: the output layer 16.1 to 17.1 ms a token exact, 7.4 to 7.7 with 4,096 rows rescored, 7.9 with 8,192; decode 8.18 and 8.79 tok/s exact against 9.94 and 9.57 (4,096), 9.46 (2,048) and 10.22 (8,192). KL against the exact layer with the copy: 0.000011, the same top token in every position. On by default on AVX2 with 8,192 rows; the dev box (AVX only) keeps the exact layer, since its first pass would be the scalar fallback. The service-level confirmation is still owed.

Shared K scoring passed its exactness check: KL 0.000000 against the per-query calls, maximum 0.000046 (the level of the old path against its own saved output).

**The guard squeezed the machine once, fixed.** With memory full of file cache the guard took more than the two minutes `start` waited for it, the service was started anyway and loaded beside a guard still holding nearly all the memory (272 MiB free, 805 MiB read in 290 s). Now the guard stops taking memory when under 3 GiB would be left available, and `start` waits five minutes and then stops the guard instead of loading beside it. Under a full file cache (15.6 GB cached) the guard measured in 30 s without huge pages and left 9.6 GB free while holding.
