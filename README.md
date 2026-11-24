# Corsia

Corsia streams the weights of a large language model from every drive in your machine at once: the internal SSD, a USB disk, a flash drive, an SD card. It spreads the model over them by measured speed, so a model bigger than your RAM runs as fast as all your drives together allow, not as fast as one.

*Corsia* is Italian for "lane". Drives that hang off the same controller share its bandwidth, so Corsia plans by lane, not by drive.

> Status: early. The daemon serves a model from several drives and is correct. Through the mount it is slower than reading the file directly; through the engine socket it matches direct reads. See [task.md](task.md) for measured numbers.

## What works today

```bash
corsia serve ~/models/qwen3.gguf --mount ~/corsia --copy-to /media/me/usbdisk
```

Mounts the model read-only at `~/corsia/qwen3.gguf` for any engine to open. Reads come from the original or from the copy on the USB disk, whichever will finish first. The copy fills while nothing is reading, busiest parts first, and is reused next time. Live status is at `http://127.0.0.1:7714/api/status`, with a one-per-second event stream at `/api/events`. Ctrl+C unmounts.

On a hard disk, the copy can be laid out expert by expert instead:

```bash
corsia serve ~/models/qwen3.gguf --mount ~/corsia --copy-to ~/ --copy-layout experts --copy-order prose.trace
```

In a GGUF, one expert's gate, up and down slices sit in three tensors far apart, so fetching it costs three seeks. An expert-major copy stores each expert's slices side by side (one read per expert) and orders each layer's experts so the ones that fire together in the routing trace are neighbours, so a token's experts often merge into a single read. The mount still shows the original file. Such a copy may sit on the original's own drive, which is the point on a machine with one hard disk. `corsia replay TRACE --model MODEL.gguf` predicts the gain for your disk without touching it.

Engines can also skip the mount: `corsia serve` opens a private socket (`$XDG_RUNTIME_DIR/corsia/<model>.sock`) where an engine asks for batches of byte ranges and Corsia reads them from the drives straight into memory the two share, with no copies. That matches reading the file directly, and a batch is spread over every drive at once. The protocol is documented at the top of `src/hint.rs`; `corsia hint-bench` is a working client. A C client is in `clients/c/`, and a llama.cpp patch that loads models through the socket is in [integrations/llama.cpp](integrations/llama.cpp/README.md).


```bash
corsia lanes
```

Shows each drive, the controller it hangs off and how much bandwidth that controller shares, then tells you how to place drives for speed: a USB 3 disk stuck in a USB 2.0 port, two drives fighting over one lane, lanes left empty.

```bash
corsia bench
```

Measures every drive alone, then all of them at once, and prints how Corsia would split each read so every drive finishes at the same moment. It writes one test file of random bytes per drive (1024 MiB by default), reads it back with the page cache bypassed, and deletes it afterwards. Pass folders to choose where the test files go:

```bash
corsia bench ~/ /media/me/usbdisk --size 512 --seconds 3
```

```bash
corsia trace synth qwen3.trace
corsia replay qwen3.trace ~/ /media/me/usbdisk
```

Plays an expert routing trace (which experts each token used in each layer) against your real drives. A RAM cache decides what has to be read; each placement strategy then reads it, layer by layer, and Corsia reports tokens per second from the drives alone. `trace synth` writes a synthetic trace shaped like Qwen3-30B-A3B until you record a real one.

All commands take `--json`. None needs root.

## Watching it live

```bash
corsia watch
```

A live terminal view of two things at once. Above, the test runs: each script in `integrations/` reports as it goes (one line per arm with its tokens per second, seconds per token, prompt rate, the guess hit rate and how much was prefetched), so a run that takes an hour can be followed from start to finish. Below, the running daemon: MB/s per drive as a moving graph, bytes served, prefetch totals, the block cache and the copies filling. When no daemon is running it says so, and how to start one.

Runs report to `~/.cache/corsia-build/progress/<run>.jsonl` (set `CORSIA_PROGRESS` to put them elsewhere, or `off` to skip). Nothing has to be watching: the scripts only append to that file. The view updates the moment a line is written (file-change events) and the daemon pushes its status every second over `/api/events`, so nothing is polled.

```bash
corsia watch --url 127.0.0.1:7715      # a daemon on another port
ssh -L 7714:127.0.0.1:7714 testbox      # a daemon on another machine: forward the same port
corsia watch --light                    # light terminal; NO_COLOR=1 for no colour
corsia watch --snapshot 120x36 --html   # one frame, for reviewing the design
```

To follow a run on another machine, run `corsia watch` there over ssh: the runs are in that machine's progress folder. Keys: up and down choose a run, f hides finished runs, q quits.

## Coding assistants: `corsia agent`

Coding assistants (opencode, Continue, Cline and the like) send their whole system prompt and tool list with every request: about 23,000 tokens for opencode. On a local model, processing that prompt is most of the wait. llama-server keeps the previous prompt's state and reuses it up to the first token that differs, but assistants put the date, the folder and the git status early in the system prompt, so a new session or another project throws most of it away.

`corsia agent` is an OpenAI-compatible endpoint in front of llama-server that fixes that:

- **Stable prefix.** Lines and blocks that change per session (`<env>`, dates, working folder, git status, branch) move from the system prompt to the front of the first user message. Nothing is dropped; the system prompt and tools are now identical across sessions, folders and days, so they are computed once. Tools are put in name order.
- **Slots.** Each long system prompt keeps its own llama-server slot; short side requests (thread titles, summaries) go to another, so they never push the long prompt out.
- **KV snapshots.** The first time a long system prompt has been processed, Corsia saves the slot's state to disk, named by a hash of the prompt. A new session after a restart, or the next day, restores it in seconds instead of recomputing it.
- **Reporting.** Every request's prompt size, how much was reused, prompt time and generation time, on the terminal, in `/api/agent` and pushed live on `/api/agent/events`.

```bash
mkdir -p ~/.cache/corsia/kv
corsia serve ~/models/qwen3.gguf --mount ~/corsia --copy-to /media/me/usbdisk   # for models bigger than RAM
CORSIA_SOCKET=/run/user/1000/corsia/qwen3.gguf.sock \
  llama-server -m ~/corsia/qwen3.gguf --jinja -c 32768 -fa on -ctk q8_0 -ctv q8_0 \
    -b 2048 -ub 2048 -np 2 --kv-unified -cram 0 --slot-save-path ~/.cache/corsia/kv --port 8080
corsia agent --upstream 127.0.0.1:8080 --snapshots ~/.cache/corsia/kv
```

Then point the assistant's OpenAI-compatible provider at `http://127.0.0.1:7720/v1`.

The same three, as one service under a memory cap, in one command (the pool sizes itself to what the cap or the free memory leaves):

```bash
CAP=10 integrations/llama.cpp/local-agent.sh start     # status, logs, stop
```

For opencode, add a provider with `"npm": "@ai-sdk/openai-compatible"` and `"baseURL": "http://127.0.0.1:7720/v1"`, model `local`, and give the local model a lean agent (read, edit, write, glob, grep, bash; the rest off) so its prompt stays near 5,000 tokens. Without `corsia serve` (a model that fits in RAM), leave out `CORSIA_SOCKET` and load the model from its own path. llama-server comes from the Corsia fork of llama.cpp (see [integrations/llama.cpp](integrations/llama.cpp/README.md)); stock llama-server works too, without expert streaming. On a machine with the model in RAM or cores to spare, add `--spec-type ngram-simple` to llama-server: assistants copy a lot from their context, and n-gram drafts made coding answers 2.2 times faster with the same output in a test on a 4-core desktop (on a 2-core laptop under a memory cap it was slower). `--plain` passes requests through unchanged and only measures, for comparing. `--snapshots auto` picks a folder on the fastest writable drive and prints it, for llama-server's `--slot-save-path`. `--volatile LABEL` adds a "Label: value" line your assistant uses to the ones that move.

It is part of the `corsia` binary rather than a separate program: it needs no new dependencies (standard-library threads and a small JSON module, since everything is on 127.0.0.1), and it belongs next to the daemon, which will keep the snapshots on its fastest lane and prefetch them like weights.

`integrations/llama.cpp/capture-agent-request.py` records what your assistant really sends, and `integrations/llama.cpp/agent-bench.sh MODEL CAPTURE_DIR [CAP_GIB]` replays it against llama-server with and without `corsia agent` (a side request, the first request, a follow-up, a new session in another folder, and the same after a restart) and measures prompt processing at several batch sizes. Measured numbers are in [task.md](task.md).

Measured on a 2012 laptop (i5-2450M, Qwen3-30B-A3B under a 6 GB memory cap, a 5,153-token opencode prompt): the first request takes 49 minutes either way. A new session in another folder on another day then answers in 2 minutes with `corsia agent` (4,971 of 5,044 tokens reused) against 38 minutes without it (1,819 reused), and after a restart the 270 MB snapshot comes back from the SSD in 0.6 s and the session answers in 3 minutes instead of about 49. The full 23,000-token opencode prompt would take more than 12 hours to compute once on that laptop, so there it is only usable through a snapshot (about 1.2 GB).

## Example

A 2012 laptop with a SATA SSD and a USB hard disk:

```
Each drive alone              sda             sdc
  Sequential 1 MiB, 8 deep    504 MB/s        35 MB/s
  Random 4 MiB, 4 deep        492 MB/s        31 MB/s

All drives at once: Random 4 MiB, 4 deep
  sda       492 MB/s      100% of its speed alone
  sdc       31 MB/s       103% of its speed alone
  Total     523 MB/s      1.06x the fastest drive alone
```

The hard disk sits on a USB 2.0 port. `corsia lanes` points out that its enclosure is USB 3 capable and the laptop has a free USB 3 controller.

## Build

Linux 5.6 or later (io_uring), Rust stable.

```bash
cargo build --release
./target/release/corsia lanes
cargo build --release --manifest-path watch/Cargo.toml   # for corsia watch
```

`corsia watch` is a crate of its own (`watch/`, ratatui and notify) so the main build keeps its three dependencies; `corsia watch` finds the binary next to `corsia`, in `watch/target/release/`, or on `PATH`.

## Guides

- [Choosing the right model file for your CPU](docs/choosing-a-model-file.md): why the weight format inside a model file can matter more than its size, and how to tune a file for the machine it runs on (+11% on a 2011 laptop at the same quality).
- [Remote experts](docs/remote-experts.md): a design for keeping experts in another machine's memory.
- [Contributing](CONTRIBUTING.md): how to report results and send changes.

## Licence

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
