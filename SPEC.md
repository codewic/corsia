# Corsia spec

Started 2026-11-22 from the llm-engine Phase 3 research (InferSSD prior art). Tracker: [task.md](task.md).

## 1. The idea

Running a model bigger than RAM, speed is roughly the drives' combined bandwidth divided by the bytes each token has to fetch. Corsia attacks both sides:

- **More bandwidth:** use every drive at once, spread by measured speed, planned per controller ("lane") because drives on one lane share it.
- **Fewer bytes and less waiting:** caching, prediction, progressive weights and a layout that keeps reads long and sequential.

It is a storage adapter, not an inference engine. Any engine reads the model as a normal file; Corsia decides where every byte lives, when it moves and in what form.

InferSSD (WiCi, Apache-2.0) streams MoE experts from one NVMe drive inside its own llama.cpp fork. Corsia differs in three ways: it works under any engine, it is built for every kind of drive (slow ones included), and it uses many drives at once.

## 2. Architecture

```
engine (llama.cpp, Ollama, ...)
   │ reads a normal file, or sends hints over a shared-memory ring
   ▼
corsia daemon ── local API + live events ──► management UI, CLI
   │ one thread and one io_uring queue per lane
   ▼
lanes: SATA │ USB 3 │ USB 2.0 │ SD │ NVMe │ (later) LAN peers
```

- **Blind mode:** the model is mounted with FUSE passthrough. Corsia learns the access pattern from the reads themselves and prefetches.
- **Hinted mode:** a small llama.cpp patch tells Corsia which layer and experts come next. Same daemon, better prediction.
- Rust, no async runtime, few dependencies. No kernel module.

## 3. Techniques

1. **Lane-aware spreading, planned by finish time.** Every read goes where it will finish soonest, counting each drive's fixed cost per read as well as its bandwidth. Splitting by bandwidth share alone was measured to be 28 to 40% slower than one drive (replay bench, 2026-11-22): each layer waits for its slowest read, and a hard disk's fixed cost makes it that read. A slow drive earns its place when reads are known ahead (hinted mode) or when it serves the background work in technique 2.
2. **Slow lanes off the critical path.** Fast lanes hold hot experts and the coarse half of each weight block; slow lanes hold cold experts and the fine half.
3. **Progressive weights.** Each quantised block is stored as a coarse part plus a correction that adds up exactly to the original. Under pressure the engine gets the coarse block now and the full one later.
4. **Copies and hedged reads.** Hot chunks live on several lanes; each read goes to whichever lane will answer first.
5. **Parity.** Chunks stored as k data pieces plus parity, so any k lanes rebuild a read and a stalled stick is simply skipped.
6. **Removable drives are speed copies.** The main drive always holds the whole model. Plugging a drive in adds a lane after it fills in idle time; pulling it out loses nothing. Corsia only reads while serving (its chunks sit in a read-only mount or its own partition), so yanking a drive never leaves a dirty filesystem behind. Learned on 2026-11-22, when a USB swap left an NTFS disk marked dirty and unmountable.
7. **Layout that improves with use.** A heat map of expert use drives an idle-time rewrite: co-firing experts side by side, hot data first, on hard disks on the faster outer tracks.
8. **Prediction that never hurts.** Prefetches rank only experts not already in RAM, land in a small staging area rather than the cache (a wrong guess evicts nothing useful), and go only to drives idle at that moment. A routing-only predictor (affinity, repeat, popularity) named about 36% of experts on a synthetic trace and broke even at best; the gain needs the hidden-state predictor in hinted mode, which runs the next layers' routers early. Measured 2026-11-22.
9. **Per-drive tuning.** Read size, queue depth and prefetch distance chosen from the measured drive, not guessed.
10. **More tiers.** Compressed RAM, spare GPU memory, other machines' RAM over the LAN.
11. **Honest limits.** When the CPU, not the drives, is the bottleneck, Corsia says so instead of suggesting more drives.

## 4. Management UI

The daemon owns the work; the UI is a client of it, so the engine stays headless and scriptable and the UI can never slow a read.

- **Delivery:** the daemon serves a small web UI and a JSON API on localhost (LAN access opt-in with a key). It works on a headless box, from another computer and on any OS, and a Vorynza desktop app can embed it later.
- **Live by default:** lane throughput, cache hits, fill progress and tokens per second are pushed over SSE with reconnect. No polling.
- **Screens:**
  - **Lanes:** the controller tree with each drive, live MB/s per lane and the placement tips from `corsia lanes`.
  - **Model placement:** how each model is spread, what share each lane carries, and what is copied where.
  - **Plug in a drive:** new drives appear live, with "Use this drive" and fill progress. Removal is shown as a lane going away, never as an error.
  - **Performance:** tokens per second, bytes per token, cache hit rate, and what the current limit is (a drive, a lane or the CPU).
  - **Bench:** run the lane bench from the UI and compare runs.
- **Design:** follows the Vorynza design principles: the three-zone header, every state designed, a bespoke mark and reserved hue (to be chosen from the free range in the colour map when the UI starts).

## 5. Phases

1. **Lanes and bench** (this release): lane finder, lane bench, advice. Done 2026-11-22.
2. **Replay bench:** replay an expert routing trace against real drives with each placement strategy and predict tokens per second. First version done 2026-11-22 with synthetic traces; real ones still to be recorded on the 40 GB machine.
3. **Daemon and blind mode:** FUSE mount, copies on extra drives, soonest-finish reads, RAM block cache, live API. First version done 2026-11-22: correct, reaches 46 to 81% of direct reads; closing that gap is the next step.
4. **Hinted mode:** engines read through a private socket straight into shared memory (protocol v1 in `src/hint.rs`). Daemon side, reference client and bench done 2026-11-22: matches direct reads, 1.6 to 2x blind mode. The llama.cpp patch is next.
5. **Expert streaming:** llama.cpp memory-maps the model from the mount; its router's choices (`ffn_moe_topk`) trigger one PREFETCH per layer that Corsia stores straight into the page cache. Built 2026-11-22 (protocol, clients, llama.cpp patch 0002, test script); the speed verdict needs the 40 GB machine.
6. **Progressive weights:** coarse and fine split, measured for size here and for quality on the 40 GB machine.
7. **Management UI** on the daemon's API.
8. **Layout rewrite, parity, LAN tier.**

## 6. Rules

- Never run models on the owner's dev box; it measures drives, not models. Model runs happen on the 40 GB machine.
- Builds with `-j 2`, no heavy dependency rebuilds.
- Test files hold random bytes, live in `.corsia-bench` or `~/.corsia-bench`, and are always removed (also on Ctrl+C) unless `--keep`.
- Never write to a drive the user did not choose or that holds installer media without asking.
- Plain words in output and docs, no em dashes.

## 7. Open questions

- Licence: settled, MIT OR Apache-2.0 (published 2026-11-24).
- Name: Corsia (crates.io free on 2026-11-22; 39 GitHub repositories use the word, none in storage). A trademark check is still to do.
- Auto mode in `corsia bench` picks one writable folder per drive, including removable ones. It might be better to ask before writing to removable drives.
