//! The replay bench: plays an expert routing trace against real drives and measures how
//! fast each placement strategy would feed a model bigger than RAM.
//!
//! A RAM cache (LRU) decides which experts must come from the drives. The same misses are
//! replayed for every strategy, layer by layer, the way an engine without prediction asks
//! for them: a layer's experts are known only when the layer before has run.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::time::Instant;

use io_uring::{opcode, types, IoUring};

use crate::bench::{self, AlignedBuf, Target, SOLO, STOP};
use crate::layout::{group_reads, ExpertLayout, ExpertMap, Seg};
use crate::trace::Trace;

const ALIGN: u64 = 4096;

pub fn align_up(n: u64) -> u64 {
    n.div_ceil(ALIGN) * ALIGN
}

/// How a drive behaves for expert reads: a fixed cost per read plus bytes over bandwidth.
#[derive(Clone, Debug)]
pub struct DriveModel {
    pub name: String,
    pub mb_s: f64,
    pub per_read_ms: f64,
}

impl DriveModel {
    pub fn read_ms(&self, bytes: u64) -> f64 {
        self.per_read_ms + bytes as f64 / (self.mb_s.max(0.1) * 1e6) * 1e3
    }
}

pub fn measure_drive(t: &Target, secs: f64) -> io::Result<DriveModel> {
    let big = bench::run(t, SOLO[3], secs, 0xA11)?; // random 4 MiB, 4 deep
    let one = bench::run(t, SOLO[2], secs, 0xB22)?; // random 1 MiB, 1 deep
    let transfer_ms = (1u64 << 20) as f64 / (big.mb_s.max(0.1) * 1e6) * 1e3;
    let per_read_ms = (one.p50_us as f64 / 1e3 - transfer_ms).max(0.0);
    Ok(DriveModel { name: t.drive.clone(), mb_s: big.mb_s, per_read_ms })
}

// ---------------------------------------------------------------------------
// RAM cache

/// Experts each replayed token has to fetch, per layer, after the RAM cache.
pub struct Misses {
    pub per_token: Vec<Vec<Vec<u32>>>,
    pub hit_rate: f64,
    pub cache_experts: usize,
    pub needed_per_token: f64,
}

struct Lru {
    cap: usize,
    clock: u64,
    stamp: Vec<u64>,
    order: BTreeMap<u64, u32>,
}

impl Lru {
    fn new(cap: usize, total: usize) -> Lru {
        Lru { cap: cap.min(total), clock: 0, stamp: vec![0; total], order: BTreeMap::new() }
    }

    fn contains(&self, g: u32) -> bool {
        self.stamp[g as usize] != 0
    }

    fn touch(&mut self, g: u32) -> bool {
        if self.cap == 0 {
            return false;
        }
        let gi = g as usize;
        let hit = self.stamp[gi] != 0;
        if hit {
            self.order.remove(&self.stamp[gi]);
        } else if self.order.len() >= self.cap {
            if let Some((_, old)) = self.order.pop_first() {
                self.stamp[old as usize] = 0;
            }
        }
        self.clock += 1;
        self.stamp[gi] = self.clock;
        self.order.insert(self.clock, g);
        hit
    }
}

pub fn simulate_cache(trace: &Trace, cache_experts: usize, warmup: usize, tokens: usize) -> Misses {
    let total = trace.layers * trace.experts;
    let mut lru = Lru::new(cache_experts, total);
    let (mut hits, mut needed) = (0u64, 0u64);
    let mut per_token = Vec::new();
    for (i, tok) in trace.tokens.iter().enumerate().take(warmup + tokens) {
        let replay = i >= warmup;
        let mut layers = Vec::with_capacity(trace.layers);
        for (l, group) in tok.iter().enumerate() {
            let mut miss = Vec::new();
            for &e in group {
                let g = (l * trace.experts + e as usize) as u32;
                let hit = lru.touch(g);
                if replay {
                    needed += 1;
                    if hit {
                        hits += 1;
                    } else {
                        miss.push(g);
                    }
                }
            }
            layers.push(miss);
        }
        if replay {
            per_token.push(layers);
        }
    }
    let n = per_token.len().max(1) as f64;
    Misses {
        hit_rate: if needed == 0 { 0.0 } else { hits as f64 / needed as f64 },
        needed_per_token: needed as f64 / n,
        cache_experts: lru.cap,
        per_token,
    }
}

// ---------------------------------------------------------------------------
// Strategies

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Strategy {
    /// Every expert on the fastest drive: what a one-drive streamer does.
    One,
    /// Every expert cut into slices sized to each drive's bandwidth.
    Sliced,
    /// Whole experts spread over the drives in proportion to bandwidth.
    Whole,
    /// A full copy on every drive; each read goes to the drive that will finish first.
    Scheduled,
    /// Scheduled, with the experts of the next N layers known in advance (hinted mode with a
    /// perfect predictor), so their reads go out together. `u8::MAX` means the whole token.
    Ahead(u8),
    /// One drive with the same advance knowledge, so prediction's share of a gain shows apart
    /// from the extra drives' share.
    OneAhead(u8),
    /// Copies + soonest drive, prefetching what the predictor expects in the next `ahead`
    /// layers (`pick` guesses each) while the current layer's reads run. Live cache.
    Predicted { ahead: u8, pick: u8 },
}

impl Strategy {
    pub const ALL: [Strategy; 8] = [
        Strategy::One,
        Strategy::Sliced,
        Strategy::Whole,
        Strategy::Scheduled,
        Strategy::Ahead(1),
        Strategy::Ahead(3),
        Strategy::OneAhead(u8::MAX),
        Strategy::Ahead(u8::MAX),
    ];

    pub fn label(&self, fastest: &str) -> String {
        match self {
            Strategy::One => format!("All on {fastest} (one drive)"),
            Strategy::Sliced => "Each expert sliced by speed".into(),
            Strategy::Whole => "Whole experts spread by speed".into(),
            Strategy::Scheduled => "Copies + pick the soonest drive".into(),
            Strategy::Ahead(u8::MAX) => "...+ whole token known".into(),
            Strategy::Ahead(1) => "...+ next layer known".into(),
            Strategy::Ahead(n) => format!("...+ next {n} layers known"),
            Strategy::OneAhead(u8::MAX) => format!("All on {fastest} + whole token known"),
            Strategy::OneAhead(n) => format!("All on {fastest} + next {n} layers known"),
            Strategy::Predicted { ahead, pick } => format!("...+ predicted ({ahead} ahead, {pick} each)"),
        }
    }

    pub fn key(&self) -> String {
        match self {
            Strategy::One => "one".into(),
            Strategy::Sliced => "sliced".into(),
            Strategy::Whole => "whole".into(),
            Strategy::Scheduled => "scheduled".into(),
            Strategy::Ahead(u8::MAX) => "ahead_token".into(),
            Strategy::Ahead(n) => format!("ahead{n}"),
            Strategy::OneAhead(u8::MAX) => "one_ahead_token".into(),
            Strategy::OneAhead(n) => format!("one_ahead{n}"),
            Strategy::Predicted { .. } => "predicted".into(),
        }
    }

    /// How many layers' reads go out together.
    fn window(&self, layers: usize) -> usize {
        match self {
            Strategy::Ahead(n) | Strategy::OneAhead(n) => (*n as usize + 1).min(layers.max(1)),
            _ => 1,
        }
    }
}

/// At most this many reads are in flight; a bigger batch streams through them as reads
/// finish. Every in-flight read owns its buffer: btrfs checks checksums on direct reads in
/// the destination buffer, so two reads sharing a buffer fail with EIO (seen 2026-11-22).
const MAX_IN_FLIGHT: usize = 48;

pub struct StratResult {
    pub strategy: Strategy,
    pub tokens: usize,
    pub secs: f64,
    pub bytes: u64,
    pub reads: u64,
    pub p50_token_ms: f64,
    pub drive_bytes: Vec<u64>,
}

impl StratResult {
    pub fn tokens_per_s(&self) -> f64 {
        self.tokens as f64 / self.secs.max(1e-9)
    }
    pub fn mb_s(&self) -> f64 {
        self.bytes as f64 / self.secs.max(1e-9) / 1e6
    }
}

/// Drives are ordered fastest first; `files[d]` and `spans[d]` belong to `models[d]`.
pub fn run_strategy(
    strategy: Strategy,
    files: &[File],
    spans: &[u64],
    models: &[DriveModel],
    expert_bytes: u64,
    misses: &Misses,
    compute_ms_per_layer: f64,
) -> io::Result<StratResult> {
    let n = files.len();
    let expert = align_up(expert_bytes);
    let total_bw: f64 = models.iter().map(|m| m.mb_s).sum::<f64>().max(1e-9);
    let shares: Vec<f64> = models.iter().map(|m| m.mb_s / total_bw).collect();
    let slice_len: Vec<u64> = shares.iter().map(|s| if *s > 0.0 { align_up((expert as f64 * s) as u64) } else { 0 }).collect();
    let mut cumulative = Vec::with_capacity(n);
    let mut acc = 0.0;
    for s in &shares {
        acc += s;
        cumulative.push(acc);
    }

    let layers = misses.per_token.first().map_or(1, |t| t.len());
    let window = strategy.window(layers);
    let max_batch = misses
        .per_token
        .iter()
        .flat_map(|t| t.chunks(window).map(|w| w.iter().map(|l| l.len()).sum::<usize>()))
        .max()
        .unwrap_or(1)
        .max(1);
    let max_reads = if strategy == Strategy::Sliced { max_batch * n } else { max_batch };
    let in_flight = max_reads.min(MAX_IN_FLIGHT);
    let mut ring = IoUring::new((in_flight as u32).next_power_of_two().max(8))?;
    let mut bufs: Vec<AlignedBuf> = (0..in_flight).map(|_| AlignedBuf::new(expert as usize)).collect();
    let mut batch: Vec<u32> = Vec::with_capacity(max_batch);
    let slot = |g: u32, d: usize, len: u64| -> u64 { (g as u64 % (spans[d] / len).max(1)) * len };

    let mut drive_bytes = vec![0u64; n];
    let (mut bytes, mut reads) = (0u64, 0u64);
    let mut token_ms = Vec::with_capacity(misses.per_token.len());
    let mut plan: Vec<(usize, u64, u64)> = Vec::with_capacity(max_reads);
    let t0 = Instant::now();
    for tok in &misses.per_token {
        if STOP.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
        }
        let ts = Instant::now();
        for group in tok.chunks(window) {
            batch.clear();
            batch.extend(group.iter().flatten());
            let layer = &batch;
            if layer.is_empty() {
                compute(compute_ms_per_layer * group.len() as f64);
                continue;
            }
            plan.clear();
            match strategy {
                Strategy::One | Strategy::OneAhead(_) => {
                    for &g in layer.iter() {
                        plan.push((0, slot(g, 0, expert), expert));
                    }
                }
                Strategy::Sliced => {
                    for &g in layer {
                        for d in 0..n {
                            if slice_len[d] > 0 {
                                plan.push((d, slot(g, d, slice_len[d]), slice_len[d]));
                            }
                        }
                    }
                }
                Strategy::Whole => {
                    for &g in layer {
                        let u = (mix(g as u64) >> 11) as f64 / (1u64 << 53) as f64;
                        let d = cumulative.partition_point(|c| *c < u).min(n - 1);
                        plan.push((d, slot(g, d, expert), expert));
                    }
                }
                Strategy::Predicted { .. } => {
                    return Err(io::Error::other("the predicted strategy runs through run_predicted"));
                }
                Strategy::Scheduled | Strategy::Ahead(_) => {
                    let mut finish = vec![0.0f64; n];
                    for &g in layer.iter() {
                        let d = (0..n)
                            .min_by(|a, b| {
                                let fa = finish[*a] + models[*a].read_ms(expert);
                                let fb = finish[*b] + models[*b].read_ms(expert);
                                fa.total_cmp(&fb)
                            })
                            .unwrap_or(0);
                        finish[d] += models[d].read_ms(expert);
                        plan.push((d, slot(g, d, expert), expert));
                    }
                }
            }
            read_all(&mut ring, files, &mut bufs, &plan)?;
            for &(d, _, len) in &plan {
                drive_bytes[d] += len;
                bytes += len;
            }
            reads += plan.len() as u64;
            // These strategies read, then compute; nothing overlaps.
            compute(compute_ms_per_layer * group.len() as f64);
        }
        token_ms.push(ts.elapsed().as_secs_f64() * 1e3);
    }
    let secs = t0.elapsed().as_secs_f64();
    token_ms.sort_by(f64::total_cmp);
    let p50_token_ms = token_ms.get(token_ms.len() / 2).copied().unwrap_or(0.0);
    Ok(StratResult { strategy, tokens: token_ms.len(), secs, bytes, reads, p50_token_ms, drive_bytes })
}

pub struct PredictStats {
    pub demand_reads: u64,
    pub prefetched: u64,
    /// Prefetched experts the model then used (in time or still arriving).
    pub useful: u64,
    /// Of the useful ones, those still in flight when their layer needed them.
    pub late: u64,
    pub hit_rate: f64,
}

/// Replays with a live cache and predicted prefetch. Drives ordered fastest first, as for
/// `run_strategy`; every drive holds a copy of every expert.
#[allow(clippy::too_many_arguments)]
pub fn run_predicted(
    files: &[File],
    spans: &[u64],
    models: &[DriveModel],
    expert_bytes: u64,
    trace: &Trace,
    cache_experts: usize,
    warmup: usize,
    tokens: usize,
    ahead: usize,
    pick: usize,
    compute_ms_per_layer: f64,
) -> io::Result<(StratResult, PredictStats)> {
    use crate::predict::{Predictor, Weights};
    use std::collections::{HashMap, VecDeque};

    let n = files.len();
    let expert = align_up(expert_bytes);
    let experts = trace.experts;
    let gid = |l: usize, e: u16| (l * experts + e as usize) as u32;
    let mut lru = Lru::new(cache_experts, trace.layers * experts);
    let mut pred = Predictor::new(trace.layers, experts, ahead, Weights::COMBINED);
    for tok in trace.tokens.iter().take(warmup) {
        for (l, group) in tok.iter().enumerate() {
            pred.observe(l, group);
            for &e in group {
                lru.touch(gid(l, e));
            }
        }
    }

    let mut ring = IoUring::new((MAX_IN_FLIGHT as u32).next_power_of_two())?;
    let bufs: Vec<AlignedBuf> = (0..MAX_IN_FLIGHT).map(|_| AlignedBuf::new(expert as usize)).collect();
    let mut free: Vec<usize> = (0..MAX_IN_FLIGHT).rev().collect();
    // Buffers held back from prefetch so a layer's own reads never wait for one.
    let reserve = trace.topk.max(1).min(MAX_IN_FLIGHT / 2);
    let mut inflight: HashMap<u32, bool> = HashMap::new(); // expert -> is a prefetch
    let mut staged: VecDeque<u32> = VecDeque::new(); // prefetched, not yet used
    let mut busy = vec![0.0f64; n]; // ms since t0 when each drive's queue empties
    let mut drive_bytes = vec![0u64; n];
    let mut stats = PredictStats { demand_reads: 0, prefetched: 0, useful: 0, late: 0, hit_rate: 0.0 };
    let (mut needed, mut hits) = (0u64, 0u64);
    let mut token_ms = Vec::new();
    let mut guess = Vec::new();
    let mut waiting: Vec<u32> = Vec::new();
    let slot = |g: u32, d: usize| -> u64 { (g as u64 % (spans[d] / expert).max(1)) * expert };
    let t0 = Instant::now();
    let now_ms = |t0: Instant| t0.elapsed().as_secs_f64() * 1e3;

    // Puts one read on the drive that will finish it soonest. Prefetches only go to a drive
    // that is idle right now, so guesses never delay a read the model is waiting for.
    let issue = |ring: &mut IoUring, g: u32, prefetch: bool, free: &mut Vec<usize>, busy: &mut [f64], drive_bytes: &mut [u64], inflight: &mut HashMap<u32, bool>| -> io::Result<bool> {
        let now = now_ms(t0);
        let pick_drive = |idle_only: bool| {
            (0..n)
                .filter(|d| !idle_only || busy[*d] <= now)
                .map(|d| (d, busy[d].max(now) + models[d].read_ms(expert)))
                .min_by(|a, b| a.1.total_cmp(&b.1))
        };
        let Some((d, finish)) = pick_drive(prefetch) else { return Ok(false) };
        if prefetch && free.len() <= reserve {
            return Ok(false);
        }
        let Some(b) = free.pop() else { return Ok(false) };
        busy[d] = finish;
        drive_bytes[d] += expert;
        let entry = opcode::Read::new(types::Fd(files[d].as_raw_fd()), bufs[b].ptr, expert as u32)
            .offset(slot(g, d))
            .build()
            .user_data(((b as u64) << 32) | g as u64);
        // SAFETY: buffer b belongs to this read alone until its completion is reaped, and
        // every read is reaped before this function's caller returns.
        unsafe { ring.submission().push(&entry) }.map_err(|_| io::Error::other("submission queue full"))?;
        inflight.insert(g, prefetch);
        Ok(true)
    };
    // Reaps finished reads (waiting for at least one when `wait`). Reads the model asked for
    // enter the cache; prefetched ones wait in a small staging area (oldest dropped first),
    // so a wrong guess never pushes a useful expert out of the cache.
    let staging_cap = (ahead * pick * 2).max(16);
    let reap = |ring: &mut IoUring, wait: bool, free: &mut Vec<usize>, inflight: &mut HashMap<u32, bool>, lru: &mut Lru, staged: &mut VecDeque<u32>| -> io::Result<()> {
        if wait {
            ring.submit_and_wait(1)?;
        } else {
            ring.submit()?;
        }
        let done: Vec<(u64, i32)> = ring.completion().map(|c| (c.user_data(), c.result())).collect();
        let mut err = None;
        for (ud, res) in done {
            let (b, g) = ((ud >> 32) as usize, (ud & 0xFFFF_FFFF) as u32);
            free.push(b);
            if res < 0 {
                err.get_or_insert(io::Error::from_raw_os_error(-res));
            }
            if inflight.remove(&g) == Some(true) {
                if staged.len() >= staging_cap {
                    staged.pop_front();
                }
                staged.push_back(g);
            } else {
                lru.touch(g);
            }
        }
        err.map_or(Ok(()), Err)
    };

    let mut result: io::Result<()> = Ok(());
    'tokens: for tok in trace.tokens.iter().skip(warmup).take(tokens) {
        if STOP.load(Ordering::Relaxed) {
            result = Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
            break;
        }
        let ts = Instant::now();
        for (l, group) in tok.iter().enumerate() {
            pred.observe(l, group);
            waiting.clear();
            // Prefetches that landed during the last layer's compute are in RAM now.
            if let Err(e) = reap(&mut ring, false, &mut free, &mut inflight, &mut lru, &mut staged) {
                result = Err(e);
                break 'tokens;
            }
            for &e in group {
                let g = gid(l, e);
                needed += 1;
                if lru.contains(g) {
                    hits += 1;
                    lru.touch(g);
                } else if let Some(pos) = staged.iter().position(|s| *s == g) {
                    // A guess that came true: it moves from staging into the cache.
                    staged.remove(pos);
                    hits += 1;
                    stats.useful += 1;
                    lru.touch(g);
                } else if let Some(pf) = inflight.get_mut(&g) {
                    if *pf {
                        stats.useful += 1;
                        stats.late += 1;
                        *pf = false;
                    }
                    waiting.push(g);
                } else {
                    while free.is_empty() {
                        if let Err(e) = reap(&mut ring, true, &mut free, &mut inflight, &mut lru, &mut staged) {
                            result = Err(e);
                            break 'tokens;
                        }
                    }
                    match issue(&mut ring, g, false, &mut free, &mut busy, &mut drive_bytes, &mut inflight) {
                        Ok(_) => stats.demand_reads += 1,
                        Err(e) => {
                            result = Err(e);
                            break 'tokens;
                        }
                    }
                    waiting.push(g);
                }
            }
            for d in 1..=ahead {
                pred.rank(l, d, &mut guess);
                let mut issued = 0;
                for &e in &guess {
                    if issued == pick {
                        break;
                    }
                    let g = gid(l + d, e);
                    if lru.contains(g) || inflight.contains_key(&g) || staged.contains(&g) {
                        continue;
                    }
                    match issue(&mut ring, g, true, &mut free, &mut busy, &mut drive_bytes, &mut inflight) {
                        Ok(true) => {
                            stats.prefetched += 1;
                            issued += 1;
                        }
                        Ok(false) => break,
                        Err(e) => {
                            result = Err(e);
                            break 'tokens;
                        }
                    }
                }
            }
            if let Err(e) = ring.submit() {
                result = Err(e);
                break 'tokens;
            }
            while waiting.iter().any(|g| inflight.contains_key(g)) {
                if let Err(e) = reap(&mut ring, true, &mut free, &mut inflight, &mut lru, &mut staged) {
                    result = Err(e);
                    break 'tokens;
                }
            }
            // The layer computes; prefetches keep streaming meanwhile.
            compute(compute_ms_per_layer);
        }
        token_ms.push(ts.elapsed().as_secs_f64() * 1e3);
    }
    let secs = t0.elapsed().as_secs_f64();
    // Drain every read still in flight: the kernel owns those buffers until then.
    while !inflight.is_empty() {
        if let Err(e) = reap(&mut ring, true, &mut free, &mut inflight, &mut lru, &mut staged) {
            result = result.and(Err(e));
            if free.len() == MAX_IN_FLIGHT {
                break;
            }
        }
    }
    result?;
    stats.hit_rate = if needed == 0 { 0.0 } else { hits as f64 / needed as f64 };
    token_ms.sort_by(f64::total_cmp);
    let bytes: u64 = drive_bytes.iter().sum();
    let res = StratResult {
        strategy: Strategy::Predicted { ahead: ahead.min(255) as u8, pick: pick.min(255) as u8 },
        tokens: token_ms.len(),
        secs,
        bytes,
        reads: stats.demand_reads + stats.prefetched,
        p50_token_ms: token_ms.get(token_ms.len() / 2).copied().unwrap_or(0.0),
        drive_bytes,
    };
    Ok((res, stats))
}

/// Issues the reads in `plan` with up to `bufs.len()` in flight, each in its own buffer, and
/// waits for all of them.
fn read_all(ring: &mut IoUring, files: &[File], bufs: &mut [AlignedBuf], plan: &[(usize, u64, u64)]) -> io::Result<()> {
    let push = |ring: &mut IoUring, i: usize, b: usize, buf: &AlignedBuf| -> io::Result<()> {
        let (d, off, len) = plan[i];
        let entry = opcode::Read::new(types::Fd(files[d].as_raw_fd()), buf.ptr, len as u32)
            .offset(off)
            .build()
            .user_data(((i as u64) << 16) | b as u64);
        // SAFETY: buffer b is used by this read alone until its completion arrives, and this
        // function waits for every completion before returning.
        unsafe { ring.submission().push(&entry) }.map_err(|_| io::Error::other("submission queue full"))
    };
    let mut next = 0;
    for b in 0..bufs.len().min(plan.len()) {
        push(ring, next, b, &bufs[b])?;
        next += 1;
    }
    let mut left = plan.len();
    let mut error = None;
    while left > 0 {
        ring.submit_and_wait(1)?;
        let done: Vec<(u64, i32)> = ring.completion().map(|c| (c.user_data(), c.result())).collect();
        for (ud, res) in done {
            left -= 1;
            let (i, b) = ((ud >> 16) as usize, (ud & 0xFFFF) as usize);
            if res < 0 {
                error.get_or_insert(io::Error::from_raw_os_error(-res));
            } else if (res as u64) < plan[i].2 {
                error.get_or_insert(io::Error::other("short read: the span file is smaller than expected"));
            }
            // After an error, stop issuing new reads but keep draining the ones in flight.
            if next < plan.len() && error.is_none() {
                push(ring, next, b, &bufs[b])?;
                next += 1;
            } else if next < plan.len() {
                left -= plan.len() - next;
                next = plan.len();
            }
        }
    }
    error.map_or(Ok(()), Err)
}

/// Stands in for the CPU running a layer. Sleeping keeps this PC idle, which is the point:
/// the drives are what is being measured.
fn compute(ms: f64) {
    if ms > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(ms / 1e3));
    }
}

fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ---------------------------------------------------------------------------
// Disk model: how many seeks a token costs with the file's layout and with an
// expert-major copy. No drives involved.

/// What one layout costs per replayed token under the disk model.
#[derive(Clone, Debug)]
pub struct SeekResult {
    pub label: String,
    pub key: String,
    /// Experts fetched per token (the cache's misses).
    pub experts: f64,
    /// Separate pieces asked for per token, after joining what touches in the file.
    pub requests: f64,
    /// Reads the drive does per token: each costs a seek.
    pub reads: f64,
    pub mb: f64,
    pub seek_s: f64,
    pub transfer_s: f64,
}

impl SeekResult {
    pub fn s_per_token(&self) -> f64 {
        self.seek_s + self.transfer_s
    }
}

/// Bytes worth reading through rather than seeking past: the bytes the drive transfers in
/// the time one seek takes (the same rule the daemon plans with).
pub fn read_through(disk: &DriveModel) -> u64 {
    ((disk.per_read_ms * disk.mb_s * 1e3) as u64).clamp(64 << 10, 4 << 20) / 4096 * 4096
}

/// Replays the cache's misses layer by layer, the way a prefetch batch asks for them:
/// each expert's slices rounded out to pages, joined where they touch in the file, then
/// (expert-major copy) mapped into the copy and joined again where they touch there.
/// Reads closer than `read_through` are one read. Each read costs one seek plus its bytes.
pub fn seek_replay(label: &str, key: &str, misses: &Misses, experts: usize, map: &ExpertMap, layout: Option<&ExpertLayout>, disk: &DriveModel) -> SeekResult {
    let gap = read_through(disk);
    let (mut fetched, mut requests, mut reads, mut bytes) = (0u64, 0u64, 0u64, 0u64);
    let mut runs: Vec<(u64, u64)> = Vec::new();
    let mut segs: Vec<Seg> = Vec::new();
    for tok in &misses.per_token {
        for (l, miss) in tok.iter().enumerate() {
            if miss.is_empty() || l >= map.layers.len() {
                continue;
            }
            fetched += miss.len() as u64;
            runs.clear();
            for &g in miss {
                runs.extend(map.slices(l, g % experts as u32));
            }
            runs.sort_unstable();
            let mut joined: Vec<(u64, u64)> = Vec::new();
            for &r in &runs {
                match joined.last_mut() {
                    Some(m) if r.0 <= m.1 => m.1 = m.1.max(r.1),
                    _ => joined.push(r),
                }
            }
            requests += joined.len() as u64;
            segs.clear();
            match layout {
                Some(lay) => {
                    for &(a, b) in &joined {
                        lay.cover(a, b, &mut segs);
                    }
                }
                None => segs.extend(joined.iter().map(|&(a, b)| Seg { orig: a, copy: a, len: b - a })),
            }
            for (c0, c1, _) in group_reads(&mut segs, gap, u64::MAX) {
                reads += 1;
                bytes += c1 - c0;
            }
        }
    }
    let n = misses.per_token.len().max(1) as f64;
    SeekResult {
        label: label.to_string(),
        key: key.to_string(),
        experts: fetched as f64 / n,
        requests: requests as f64 / n,
        reads: reads as f64 / n,
        mb: bytes as f64 / n / 1e6,
        seek_s: reads as f64 / n * disk.per_read_ms / 1e3,
        transfer_s: bytes as f64 / n / (disk.mb_s.max(0.1) * 1e6),
    }
}

#[cfg(test)]
mod seek_tests {
    use super::*;
    use crate::layout::{identity_order, ExpertLayout, ExpertMap};

    #[test]
    fn expert_major_needs_one_read_per_expert() {
        let map = ExpertMap::synthetic(4, 16, 600 << 10);
        let lay = ExpertLayout::new(&map, identity_order(&map)).unwrap();
        // Experts 1, 5 and 9 in every layer: far apart in id order.
        let tok: Vec<Vec<u32>> = (0..4).map(|l| [1u32, 5, 9].iter().map(|e| (l * 16) as u32 + e).collect()).collect();
        let misses = Misses { per_token: vec![tok], hit_rate: 0.0, cache_experts: 0, needed_per_token: 12.0 };
        let disk = DriveModel { name: "hdd".into(), mb_s: 100.0, per_read_ms: 10.0 };
        let file = seek_replay("file", "file", &misses, 16, &map, None, &disk);
        let em = seek_replay("experts", "experts", &misses, 16, &map, Some(&lay), &disk);
        assert_eq!(file.experts, 12.0);
        assert_eq!(file.reads, 36.0, "three slices per expert, none touching");
        assert_eq!(em.reads, 12.0, "one read per expert");
        assert!((em.mb - file.mb).abs() / file.mb < 0.02, "same bytes, give or take shared pages");
        // Neighbours in the copy merge: 4, 5 and 6 are one read per layer.
        let tok: Vec<Vec<u32>> = (0..4).map(|l| [4u32, 5, 6].iter().map(|e| (l * 16) as u32 + e).collect()).collect();
        let misses = Misses { per_token: vec![tok], hit_rate: 0.0, cache_experts: 0, needed_per_token: 12.0 };
        assert_eq!(seek_replay("experts", "experts", &misses, 16, &map, Some(&lay), &disk).reads, 4.0);
        // In the file they touch within each tensor: three reads per layer.
        assert_eq!(seek_replay("file", "file", &misses, 16, &map, None, &disk).reads, 12.0);
    }
}
