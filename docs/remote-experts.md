# Remote experts: borrowing another machine's RAM and cores

Status: design spike, 22 November 2026. Nothing here is built except the round-trip probe
(`integrations/remote-experts/rtt.py`). Nothing is committed.

## The idea in one paragraph

An MoE model is mostly experts: in Qwen3-30B-A3B UD-Q2_K_XL (11.8 GB) about 11 GB of the
file is the 48 x 128 expert FFNs, about 1.8 MB each, and every token uses only 8 of them
per layer. Today Corsia streams the chosen experts from every drive at once into a 6 GB
memory cap. Remote experts moves some or all of them onto an **expert host**, any machine
with spare RAM (testbox, the 38 GB i7-6700, holds all of them with 20 GB to spare). The
main machine keeps attention, the KV cache, the routers, the embeddings and the output
head. Per layer it sends the hidden state the experts would read (2048 numbers), the
chosen expert ids and their weights; the host runs those experts and sends back one
vector, their weighted sum. Weights never cross the network after the first copy; only
about 8 KB goes each way per layer.

"Virtual CPUs" in the literal sense cannot work: a remote memory access costs a network
round trip, about 100,000 times a local one, and a CPU stalls on every access. What can
work is shipping a whole unit of work (one layer's experts) per round trip, so the
network cost is paid 48 times per token instead of millions of times.

## 1. The budget

### Shapes

| | Value |
| --- | --- |
| Layers | 48, one after another: layer L+1 needs layer L's output |
| Hidden state | 2048 numbers: 8192 bytes as f32, 4096 as f16, 2336 as Q8_K |
| Chosen experts per layer | 8 of 128, each an id (u16) and a weight (f32): 48 bytes |
| Request per layer | header 24 bytes + 48 bytes ids and weights + hidden state |
| Reply per layer | header 16 bytes + one weighted sum (f32 8192 bytes, f16 4096) |
| Round trips per token | 48 per host used, strictly in sequence |
| Bytes per token, plain | 48 x (8.3 + 8.2) KB = about 790 KB |
| Bytes per token, compact | 48 x (2.4 + 4.1) KB = about 310 KB (Q8_K in, f16 out) |

Why Q8_K in is free: for K-quant weights, llama.cpp's CPU matmul converts the activation
to Q8_K before the dot product anyway. If the main machine sends the Q8_K form, the host
computes exactly what the local path would have computed for gate and up. The reply is
added to the residual stream, so it stays f16 or f32 (f16 is the default; f32 is a flag
for checking).

### What a token costs today, and what would be left

Measured (task.md): 725 ms per token at best (1.38 tok/s, demand prefetch), on the
i5-2450M with 2 cores and AVX only.

Estimated, to be measured in phase 1:

- **Local work that stays**: attention, routers, norms and the output head. That is about
  0.7 GB of weights per token (attention about 0.45 GB, output head about 0.25 GB) on
  DDR3 at about 8 GB/s: **about 150 ms per token**, call it 100 to 200.
- **Host work**: 8 experts x 1.8 MB = 15 MB of weights per layer, memory bound on the
  i7-6700's dual-channel DDR4: **about 0.7 ms per layer, 35 ms per token**.
- **Network**: 48 x (round trip + transfer time).

So with every expert remote:

```
token = 150 ms local + 48 x (round trip + 0.7 ms host + transfer)
```

| Link | Round trip | Transfer per layer | Per token | Tokens per second | Against today (725 ms) |
| --- | --- | --- | --- | --- | --- |
| Wired gigabit LAN | about 0.3 ms | 0.14 ms (16 KB at 117 MB/s) | about 205 ms | about 4.9 | 3.5x faster |
| Good Wi-Fi (5 GHz, close) | 2 to 5 ms | about 0.5 ms | about 300 to 450 ms | 2.2 to 3.3 | 1.6 to 2.4x faster |
| testbox over the internet, direct Tailscale path | 137 ms (measured) | unknown | at least 6.8 s | 0.15 | 9x slower |
| testbox through the DERP relay (fra) | 392 ms (measured) | 69 ms (8.3 KB at 120 KB/s) each way | about 25 s | 0.04 | 35x slower |

**Break-even**: all-remote beats today when `48 x (round trip + 0.7 + transfer) < 575 ms`,
that is **a round trip under about 10 ms**. Any wired or decent wireless LAN qualifies.
No internet path does, and a relayed path loses on bandwidth alone (790 KB per token at
120 KB/s is 6.6 s before counting a single round trip).

Mixed placement (some experts local, some remote) changes the break-even, because the
round trip then runs *while* the local experts compute (section 3). With half the experts
local, the network is hidden whenever the round trip is shorter than the local share's
compute, about 3 to 5 ms per layer on the i5.

### What about slow links, then

Over a 137 ms path the only shape that can pay is one crossing per token, not 48: put a
contiguous block of whole layers (attention, KV cache and experts) on the host, which is
a pipeline split. Two crossings per token (out and back) cost 274 ms before any compute,
and splitting at layer 24 leaves half the streaming on the laptop: the sum lands about
where today's 725 ms already is. Over the internet the honest answer is "run the model on
testbox"; remote experts is a LAN feature. This design leaves the pipeline split out.

### Measured here

`tailscale ping testbox` on 22 November 2026: 392 ms through DERP (fra) for the first
packet, then 137 ms on a direct path to a public address (105.x.x.x). So testbox is
**not on the laptop's LAN** today; it is reached across the internet. The figure of
120 KB/s through the relay is from an earlier session.

Probe results: section 8.

## 2. How this differs from llama.cpp's RPC backend

`ggml/src/ggml-rpc/ggml-rpc.cpp` (3432 lines in the fork) makes a remote machine look like
one more ggml device:

- **It ships tensors.** At load, weights assigned to the RPC device are uploaded with
  `SET_TENSOR` (or `SET_TENSOR_HASH` above 10 MB, so a reload can skip resending). At run
  time every tensor that crosses a device boundary is copied with `SET_TENSOR`,
  `GET_TENSOR` or `COPY_TENSOR`.
- **It works op by op.** The scheduler splits the graph wherever the device changes;
  each split's sub-graph is serialized and sent with `GRAPH_COMPUTE` (or `GRAPH_RECOMPUTE`
  for a cached graph, up to 1024 per device), and the server runs those ggml ops. The
  client only knows ops and tensors, not experts.
- **Placement is static.** Which tensors live where is fixed at load (`-ot` overrides or
  layer split). An expert tensor `blk.L.ffn_down_exps` holds all 128 experts of a layer,
  so a remote layer means all 128 experts of it are remote; the RPC device has no notion
  of "these 40 experts here, the rest there", and no way to fall back when the host is
  slow.
- **Per layer it costs several messages**: input copy, graph compute, output copy, with
  the scheduler synchronizing around them. Graph compute is sent async in recent
  versions, but the copies around it wait, so a remote MoE block costs at least two
  round trips per layer, and the whole run stops if the server goes away.

Remote experts is the opposite on every point: one request and one reply per layer per
host, expert granularity, placement decided per token by cost, the local path always
ready as a fallback, and weights copied once at setup, never through the run-time
channel. It also needs no ggml device: the host is a small server that knows the model's
expert layout.

## 3. Where it hooks into the fork

The fork (`~/.cache/corsia-build/llama.cpp`, branch `corsia`) streams experts from
`common/corsia-experts.h`, an eval callback that watches `ffn_moe_topk-L` (to prefetch the
chosen experts) and `ffn_inp-L` (to guess the next layer).

### Can a callback replace expert compute? Yes, on the CPU backend, with one trick

The scheduler (`ggml-backend.cpp`, `ggml_backend_sched_compute_splits`) runs the graph in
ranges ending at every node the callback asked for, synchronizes, and calls the callback
with the computed tensor. The callback can read and **write** that tensor's data before
the next range runs. Two facts make replacement possible without touching ggml:

1. **CPU `mul_mat_id` skips id -1** and writes zeros for that slot
   (`ggml-cpu.c`, `ggml_compute_forward_mul_mat_id`: `if (i02 == -1) { memset(...); continue; }`).
   Upstream's own MoE cache relies on it ("the cached experts and the misses computed from
   the host experts, each reads the others as zero").
2. **The expert ids are read by the weights first, the matmuls later.** In
   `build_moe_ffn` the order is: `ffn_moe_topk` (ids), `ffn_moe_weights` (a `get_rows` by
   those ids), `ffn_moe_weights_norm`, then the gate, up and down `mul_mat_id`s, then
   `ffn_moe_weighted` (down outputs times weights), then the sum. Once the weights are
   computed, nothing but the matmuls reads the ids.

So, per layer, during generation:

| Node (callback asked for it) | What the hook does |
| --- | --- |
| `ffn_norm-L` | Remember the tensor: it is the experts' input. |
| `ffn_moe_topk-L` | Ask Corsia where each chosen expert should run (section 4). Prefetch only the ones staying local (today's code, filtered). |
| `ffn_moe_weights_norm-L` | Send the request to the host: hidden state, remote ids, their normalized weights. Then overwrite the remote slots in the ids tensor with -1. Return at once. |
| (the local gate, up and down matmuls run here, skipping the -1 slots; the network runs at the same time) | |
| `ffn_moe_weighted-L` | Wait for the reply. Add the host's weighted sum into the (zero) slot of the first remote expert. The graph's own sum then adds everything up. |

The overlap comes for free: the round trip runs while the local experts compute.

Limits of the callback route, all acceptable for a prototype:

- **CPU backend only.** Other backends may not skip -1 ids. This fork runs CPU only.
- **Architectures without per-expert biases.** `ggml_add_id(down_exps_b, selected_experts)`
  would read the -1 ids (GPT-OSS has them; Qwen3-MoE does not). The hook checks the GGUF
  for `ffn_down_exps.bias` tensors and stays local when present.
- **Node names are a contract.** Names differ by architecture (`ffn_moe_weights_norm`
  exists only when `norm_w` is set; Qwen3-MoE sets it). Phase 1 targets Qwen3-MoE and
  refuses others.
- **Three more synchronized ranges per layer** (about 150 per token on top of today's
  96). Each costs a thread-pool wake, tens of microseconds: a few ms per token.
- **Prefill stays local.** With 256 prompt tokens per batch a layer's request is 2 MB;
  worth it on a wired LAN, not on Wi-Fi. Phase 1 sends only single-token batches.
- **Fallback must compute locally inside the callback.** If the reply misses its deadline,
  the -1 slots have already been skipped. The hook then runs those experts itself with a
  tiny private ggml graph (gate, up, SwiGLU, down for k experts) on the same mapped
  weights and the CPU backend, before returning from `ffn_moe_weighted-L`. The weights
  are still mapped from Corsia's mount, so this is today's streaming path for just those
  experts.

### The cleaner long-term hook: a graph change, still no ggml change

Phase 4 replaces the name matching with two custom ops built in `build_moe_ffn`
(`src/llama-graph.cpp`), using ggml's existing `ggml_custom_4d` (`GGML_OP_CUSTOM`):

```
ids_local = corsia_send(cur, selected_experts, weights)   // custom op: sends, returns ids with -1 for remote
experts   = build_experts(cur, ids_local)                 // unchanged local path
experts   = experts + corsia_recv(experts)                // custom op: waits, writes remote sums
```

This is a patch to llama.cpp (one more file in `integrations/llama.cpp/`), not to ggml,
works for every architecture that goes through `build_moe_ffn`, and survives renames.
It is the same shape upstream already uses for its MoE cache (`cache.slots` plus
`cache.cpu_ids`, joined with `ggml_add`).

## 4. Picking where each expert runs

Corsia already picks a drive per read by soonest finish: a fixed cost per request plus
bytes over bandwidth, plus what is queued. Hosts get the same model, fitted by the same
probe that measures them (`rtt.py` prints exactly these two numbers):

```
finish(host)  = now + fixed(host) + bytes / bandwidth(host) + k x compute_per_expert(host)
finish(local) = now + max(drive_finish(experts not in RAM)) + k_local x compute_per_expert(local)
```

The fixed cost is paid once per layer per host, however many experts go there, so the
choice is nearly all-or-nothing per host:

1. Experts already in the laptop's page cache start local (they cost only compute).
2. Every other chosen expert that a host holds goes to the host whose finish is soonest.
3. Move experts back to local, cheapest first, while that lowers
   `max(finish(local), finish(each host))`. The two sides run at the same time, so the
   layer ends at the later one.
4. Experts no host holds stay local and stream from the drives as today.

The host's figures are refreshed from live calls (a moving average of round trip and
compute time per host, as `replay.rs` keeps for drives), so a host that slows down
(someone else using testbox) gets less work without a rule for it.

Where the decision lives: in the Corsia daemon, not in the engine. The engine's hook
sends `PLACE` on the existing local socket (hint protocol, new message types) and gets a
bitmask back in microseconds; Corsia owns the host connections, the health figures and
the fallback policy, as it owns the drives. The request and the reply travel engine to
Corsia to host and back, two extra local hops of about 30 microseconds each: small next
to a LAN round trip, and it keeps all networking in Rust.

## 5. Wire protocol

Extends `src/hint.rs`'s framing for a stream socket: the same 8-byte header (magic
`CORS`, version u16, type u16), then a u32 payload length, since TCP has no packet
boundaries. Little endian throughout. A separate protocol name, `corsia-experts`, version
1, so the hint socket's version stays independent. The probe already speaks this framing.

```text
HELLO      main  0x20  proto u16, 0 u16, model_id [32]u8, n_layer u16, n_expert u16, n_embd u32
HELLO_OK   host  0xA0  cores u16, 0 u16, ram_free u64, then n_layer x ceil(n_expert/8) bytes:
                       a bitmap of the experts the host holds, per layer
RUN        main  0x21  req u32, layer u16, n_tok u16, k u16, in_type u8, out_type u8,
                       then n_tok x k x (expert u16, 0 u16, weight f32),
                       then n_tok hidden states of n_embd in in_type (f32, f16 or q8_k)
RUN_OK     host  0xA1  req u32, status i32, compute_us u32, 0 u32,
                       then n_tok weighted sums of n_embd in out_type (f32 or f16)
PING       either 0x10 payload of any size (probe and keep-alive)
PONG       either 0x90 the same payload
ERROR      host  0xFF  req u32, code i32, then a UTF-8 message
```

- `model_id` is a SHA-256 over the GGUF's header and tensor table (names, types, shapes,
  offsets) plus the file size, so two copies of the same file agree without hashing
  11 GB, and a different quant never pairs up.
- `req` matches replies to requests. A reply that arrives after its deadline is dropped
  by id (the expert was already computed locally).
- One connection per host, requests in order. Requests to several hosts for one layer
  go out together.
- The host checks every field against its own tables (layer below n_layer, expert below
  n_expert and held, n_tok and k within limits, lengths exact) and answers `ERROR`
  rather than trusting input.
- `compute_us` lets the main machine split the measured call into network and compute,
  which feeds the placement model.

## 6. How hosts get their share of the experts

Copy once, keep in RAM:

1. **Assign.** `corsia hosts add testbox` measures the link (`rtt`), asks the host its
   free RAM and core count, and assigns experts: all of them if they fit (testbox: 11 GB
   in 33 GB available), otherwise the most used ones first, using a routing trace from
   `CORSIA_TRACE` (Corsia already records which experts each token used).
2. **Copy.** Version 1: the host keeps its own copy of the GGUF (`rsync` it once; over a
   wired LAN 11.8 GB takes about 2 minutes). Version 2: Corsia pushes only the assigned
   slices over the same connection with a `PUT` message (layer, expert, tensor, bytes,
   checksum), so a host with 16 GB can hold part of a 60 GB model without the whole file.
   Either way the bytes arrive once, never during a run.
3. **Keep.** `corsia host` (the server on testbox) loads its experts into anonymous
   memory at start and pins them (`mlock`, within its limit), so a run never waits on the
   host's disk. Startup reads the file once; a restart takes as long as one read of its
   share.

The host's compute uses ggml's CPU backend, so it gets llama.cpp's quantized kernels
(AVX2 and FMA on the i7-6700) for free. The host program therefore lives in the fork as a
small C++ tool (`tools/corsia-host`), reading the GGUF with `gguf.h` and building one
small graph per request (gate, up, SwiGLU, down for k experts, weighted sum). Corsia
(Rust) starts and supervises it.

## 7. Failure handling and security

### Failure

The rule: **a host can make a token faster, never stop it.**

- **Deadline per call**: the predicted finish plus a margin (3 x the host's recent p90
  round trip, at least 5 ms). On a miss the hook computes those experts locally (section
  3) and marks the host slow.
- **Slow host**: after 3 misses in 10 calls the host gets no work for a back-off that
  doubles from 1 s up to 60 s; then one probe call decides.
- **Dead host** (connection reset, no `PONG` within 2 s while idle): local for everything,
  reconnect with doubling back-off. A run never waits for a reconnect.
- **Wrong host** (different `model_id`, missing experts): refused at `HELLO`, logged once.
- **Wrong answers**: once in every 1000 calls, and on the first call after connecting,
  the main machine also computes one remote expert itself and compares (relative error
  below 1e-3 for f16 replies). A mismatch drops the host for the run and says why.
- **Host restarts mid-run**: same as dead, and its share is local until `HELLO_OK` again.

### Security

- **Tailscale only.** The host binds its Tailscale address (100.x) and nothing else, so
  only machines on the owner's tailnet can reach it. It checks each connecting peer with
  Tailscale's LocalAPI `WhoIs` and accepts only nodes listed in
  `~/.config/corsia/hosts.toml`. No port on any LAN or public interface.
- **Or no port at all**: Corsia can start the host over ssh and talk on the ssh session's
  stdin and stdout (exactly what the probe's `ssh` path does). Authentication is the ssh
  key; nothing listens. It costs an extra layer of encryption on top of WireGuard; the
  probe measures both paths so the choice is made on numbers.
- **What crosses the wire is sensitive**: hidden states can reveal what the prompt was
  about. Hosts are the owner's own machines; the doc and the CLI say so plainly.
- **The host runs nothing it receives.** It is an unprivileged user process, reads only
  its model file, validates every field (section 5) and has no command beyond running
  experts it already holds.

## 8. Probe results

`integrations/remote-experts/rtt.py testbox --user USER` sends itself over ssh, starts
an echo server on testbox for the length of the run, and times request and reply pairs of
the same size in the framing above (8-byte `CORS` header plus length). It measures two
paths: through the ssh session, and a direct TCP connection to testbox's Tailscale
address (the server binds only that address, takes one connection and exits). For each it
fits a fixed cost plus bandwidth and times 48 calls in a row, one token's worth.

**Loopback check (laptop to itself)**: 0.12 ms median for 8 KB over TCP, 0.05 ms through a
pipe, 6 ms for 48 calls in a row. That is the probe's own floor (Python), far below any
real link.

**testbox, 22 November 2026: not measured yet.** The first run (200 samples per size,
up to 64 KB) was too long for a 137 ms internet path and was stopped at 5 minutes.
Before the shorter run could start, testbox dropped off the tailnet (Tailscale showed it
"offline"; ssh timed out). What was measured before that: `tailscale ping` 392 ms through
DERP (fra), then 137 ms direct. To repeat:

```bash
integrations/remote-experts/rtt.py testbox --user USER --sizes 64,8192,32768 --count 40 --warmup 5 --tokens 3 --json results/remote-experts/rtt-testbox.json
```

At 137 ms that run takes about 2 minutes. The expected result, from the ping alone:
roughly 140 ms per 8 KB round trip and 6.6 s for a token's 48 calls, which is why
section 1 rules the internet path out.

## 9. Plan

### Phase 0: this document and the probe (done)

### Phase 1: one layer, one host, measured against local (first milestone)

Build `tools/corsia-host` in the fork (C++, ggml CPU backend, own GGUF copy, all experts
in RAM) and a test client that replays recorded inputs. Recording: one memory-capped
32-token run on the laptop with a new `CORSIA_DUMP` that saves each layer's `ffn_norm`
input, the chosen ids and weights, and the local `ffn_moe_out` (about 25 MB for 32 tokens).

**Milestone 1, measurable**: for those 32 x 48 recorded calls, the host's weighted sum
matches the local `ffn_moe_out` (relative error below 1e-3 with f16 replies, below 1e-5
with f32), and the probe-style timing over the same link gives the per-call cost split
into network and host compute. Pass mark on a wired LAN: **under 2 ms per call, p50**.
On today's internet path the number is recorded, not passed.

This needs testbox and the laptop on the same LAN for the pass mark; correctness can be
checked over any link.

### Phase 2: end to end through the callback

The hook from section 3 in `corsia-experts.h` (patch 0006), Corsia's `PLACE` message and
host connection, all experts remote. **Milestone 2**: a greedy 32-token generation gives
the same tokens as the local run, and reaches at least 3 tok/s on a wired LAN (about
4.9 predicted, 1.38 today), measured with the same settings as task.md's table.

### Phase 3: mixed placement and failure

Soonest-finish placement per layer with the live host model, local fallback inside the
callback, deadlines, back-off, the spot check. **Milestone 3**: pulling testbox's network
cable mid-run loses no token and drops to today's speed within one token; plugging it
back in returns to remote speed within 60 s.

### Phase 4: the graph patch and pushed slices

The `ggml_custom_4d` pair in `build_moe_ffn` (any architecture, no name matching), `PUT`
for hosts without the GGUF, several hosts at once, Q8_K requests, prefill on wired links.

### Out of scope

The pipeline split for slow links (section 1), GPUs on the host (the CPU path first),
sharing one host between several main machines.
