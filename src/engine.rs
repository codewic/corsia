//! The daemon's read engine: serves byte ranges of a model from whichever drive will
//! finish them soonest, and fills speed copies on extra drives while nothing is reading.
//!
//! The original file always holds the whole model. A copy that fails a read (a drive
//! pulled out, a bad sector) is marked offline and the read is retried from the original.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::bench::{self, AlignedBuf, SOLO};
use crate::layout::{group_reads, Seg, PAGE};
use crate::replay::DriveModel;
use crate::store::{CopyStore, CHUNK};

const ALIGN: u64 = 4096;
/// The unit read from a drive and kept in RAM. Smaller than a copy chunk so the kernel's
/// read-past-the-end (readahead) costs at most this much extra.
pub const BLOCK: u64 = 1 << 20;
/// Copying waits until nothing has read for this long.
const IDLE_MS: u64 = 400;
/// A copy is measured once it holds this many chunks (64 MiB).
const MEASURE_AT: u64 = 16;
/// Largest read from the original or a plain copy in one planned job.
const PIECE: u64 = 1 << 20;
/// Largest read from an expert-major copy: several neighbouring experts in one go.
const MAX_JOB: u64 = 8 << 20;

/// One planned drive read and where its bytes belong in the model.
#[derive(Debug, Clone)]
pub struct Job {
    pub source: usize,
    /// preads that fill the buffer back to back: (fd, offset, length).
    pub reads: Vec<(i32, u64, u64)>,
    /// (model offset, length, offset in the buffer). Model offsets are page aligned.
    pub outs: Vec<(u64, u64, u64)>,
}

impl Job {
    pub fn span(&self) -> u64 {
        self.reads.iter().map(|r| r.2).sum()
    }
}

pub struct Source {
    pub drive: String,
    pub copy: Option<Arc<CopyStore>>,
    pub model: Mutex<DriveModel>,
    pub measured: AtomicBool,
    busy_until_ms: Mutex<f64>,
    pub online: AtomicBool,
    pub bytes: AtomicU64,
    pub reads: AtomicU64,
    pub errors: AtomicU64,
}

impl Source {
    fn new(drive: &str, copy: Option<Arc<CopyStore>>, prior: DriveModel) -> Source {
        Source {
            drive: drive.to_string(),
            copy,
            model: Mutex::new(prior),
            measured: AtomicBool::new(false),
            busy_until_ms: Mutex::new(0.0),
            online: AtomicBool::new(true),
            bytes: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    pub fn chunks(&self, total: u64) -> u64 {
        self.copy.as_ref().map_or(total, |c| c.present())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum CopierState {
    Waiting = 0,
    Copying = 1,
    Done = 2,
    None = 3,
}

pub struct Engine {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub chunks: u64,
    primary: File,
    /// False when the original's filesystem refused O_DIRECT: its reads then drop their
    /// pages from the page cache straight after, so they neither count twice against a
    /// memory cap nor warm the next run.
    primary_direct: bool,
    /// 0 is the original file; the rest are copies.
    pub sources: Vec<Source>,
    heat: Vec<AtomicU32>,
    last_read_ms: AtomicU64,
    pub t0: Instant,
    pub stop: AtomicBool,
    pub copied_bytes: AtomicU64,
    pub requests: AtomicU64,
    copier: AtomicU8,
    cache: ChunkCache,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    /// Bytes stored into the mount's page cache by PREFETCH, and bytes it found there already.
    pub prefetched: AtomicU64,
    pub prefetch_skipped: AtomicU64,
    /// Async prefetches dropped because newer ones arrived first.
    pub prefetch_dropped: AtomicU64,
    /// Demand PREFETCH batches running now; background guesses pause while this is above 0.
    pub demand_in_flight: AtomicU32,
    /// Bytes a demand read found arriving from a guess and waited for instead of rereading.
    pub prefetch_joined: AtomicU64,
}

/// One block held in RAM. `len` is short only at the end of the model.
pub struct Chunk {
    buf: AlignedBuf,
    len: usize,
}

/// Blocks in RAM, least recently used dropped first. The kernel asks for 128 KiB at a
/// time; this turns those into one 1 MiB drive read per block, and readers of a block that
/// is already on its way wait for that read instead of issuing their own.
struct ChunkCache {
    cap: usize,
    inner: Mutex<CacheInner>,
    loaded: Condvar,
}

#[derive(Default)]
struct CacheInner {
    ready: HashMap<u64, (Arc<Chunk>, u64)>,
    by_age: BTreeMap<u64, u64>,
    loading: HashSet<u64>,
    clock: u64,
}

impl Engine {
    pub fn new(path: PathBuf, primary_drive: &str, primary_prior: DriveModel, copies: Vec<(Arc<CopyStore>, DriveModel)>, cache_chunks: usize) -> io::Result<Engine> {
        let size = std::fs::metadata(&path)?.len();
        let (primary, primary_direct) = match OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(&path) {
            Ok(f) => (f, true),
            Err(_) => (File::open(&path)?, false),
        };
        let chunks = size.div_ceil(CHUNK);
        let mut sources = vec![Source::new(primary_drive, None, primary_prior)];
        for (c, prior) in copies {
            let drive = c.drive.clone();
            sources.push(Source::new(&drive, Some(c), prior));
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "model".into());
        let state = if sources.len() > 1 { CopierState::Waiting } else { CopierState::None };
        Ok(Engine {
            name,
            path,
            size,
            chunks,
            primary,
            primary_direct,
            sources,
            heat: (0..chunks).map(|_| AtomicU32::new(0)).collect(),
            last_read_ms: AtomicU64::new(0),
            t0: Instant::now(),
            stop: AtomicBool::new(false),
            copied_bytes: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            copier: AtomicU8::new(state as u8),
            cache: ChunkCache { cap: cache_chunks.max(4), inner: Mutex::new(CacheInner::default()), loaded: Condvar::new() },
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            prefetched: AtomicU64::new(0),
            prefetch_skipped: AtomicU64::new(0),
            prefetch_dropped: AtomicU64::new(0),
            demand_in_flight: AtomicU32::new(0),
            prefetch_joined: AtomicU64::new(0),
        })
    }

    fn now_ms(&self) -> f64 {
        self.t0.elapsed().as_secs_f64() * 1e3
    }

    pub fn copier_state(&self) -> CopierState {
        match self.copier.load(Ordering::Relaxed) {
            0 => CopierState::Waiting,
            1 => CopierState::Copying,
            2 => CopierState::Done,
            _ => CopierState::None,
        }
    }

    /// Measures a source with short random reads (read-only) and keeps the result.
    pub fn measure(&self, idx: usize) -> io::Result<()> {
        let s = &self.sources[idx];
        let (fd, bytes) = match &s.copy {
            None => (self.primary.as_raw_fd(), self.size / CHUNK * CHUNK),
            Some(c) => c.first_pack().ok_or_else(|| io::Error::other("copy is empty"))?,
        };
        if bytes < 4 * CHUNK {
            return Err(io::Error::other("too little data to measure"));
        }
        let big = bench::read_loop(fd, bytes, SOLO[3], Duration::from_millis(1000), 0x1234)?;
        let one = bench::read_loop(fd, bytes, SOLO[2], Duration::from_millis(1000), 0x5678)?;
        let transfer_ms = (1u64 << 20) as f64 / (big.mb_s.max(0.1) * 1e6) * 1e3;
        let mut m = s.model.lock().map_err(|_| io::Error::other("lock"))?;
        m.mb_s = big.mb_s;
        m.per_read_ms = (one.p50_us as f64 / 1e3 - transfer_ms).max(0.0);
        s.measured.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Reads `size` bytes at `offset` (short at end of file).
    pub fn read(&self, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        if offset >= self.size {
            return Ok(Vec::new());
        }
        let end = (offset + size as u64).min(self.size);
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.last_read_ms.store(self.now_ms() as u64, Ordering::Relaxed);
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            let block = pos / BLOCK;
            let piece_end = ((block + 1) * BLOCK).min(end);
            self.heat[(pos / CHUNK) as usize].fetch_add(1, Ordering::Relaxed);
            let data = self.block(block)?;
            let from = (pos - block * BLOCK) as usize;
            let to = ((piece_end - block * BLOCK) as usize).min(data.len);
            out.extend_from_slice(&data.buf.as_slice()[from.min(to)..to]);
            pos = piece_end;
        }
        Ok(out)
    }

    /// A block from RAM, or read once from the drive that will finish it soonest.
    pub(crate) fn block(&self, chunk: u64) -> io::Result<Arc<Chunk>> {
        let lock_err = || io::Error::other("chunk cache lock poisoned");
        {
            let mut c = self.cache.inner.lock().map_err(|_| lock_err())?;
            loop {
                if let Some((data, age)) = c.ready.get(&chunk).map(|(d, a)| (d.clone(), *a)) {
                    c.clock += 1;
                    let now = c.clock;
                    c.by_age.remove(&age);
                    c.by_age.insert(now, chunk);
                    if let Some(e) = c.ready.get_mut(&chunk) {
                        e.1 = now;
                    }
                    self.cache_hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(data);
                }
                if c.loading.contains(&chunk) {
                    c = self.cache.loaded.wait(c).map_err(|_| lock_err())?;
                    continue;
                }
                c.loading.insert(chunk);
                break;
            }
        }
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
        let fetched = self.fetch(chunk, false);
        let mut c = self.cache.inner.lock().map_err(|_| lock_err())?;
        c.loading.remove(&chunk);
        let result = fetched.map(|data| {
            let data = Arc::new(data);
            c.clock += 1;
            let now = c.clock;
            c.ready.insert(chunk, (data.clone(), now));
            c.by_age.insert(now, chunk);
            while c.ready.len() > self.cache.cap {
                let Some((_, old)) = c.by_age.pop_first() else { break };
                c.ready.remove(&old);
            }
            data
        });
        self.cache.loaded.notify_all();
        result
    }

    /// Picks the source that will finish reading `span` bytes at model offset `start` first
    /// (a range inside one chunk) and books the time on its queue. Returns (source, fd,
    /// offset in that source's file).
    pub fn choose(&self, start: u64, span: u64, original_only: bool) -> Option<(usize, i32, u64)> {
        let chunk = start / CHUNK;
        let now = self.now_ms();
        let mut best: Option<(usize, i32, u64, f64)> = None;
        for (i, s) in self.sources.iter().enumerate() {
            let target = match &s.copy {
                None => Some((self.primary.as_raw_fd(), start)),
                // An expert-major copy is not laid out like the file: plan() reads it.
                Some(c) if c.layout.is_some() => None,
                Some(c) if !original_only && s.online.load(Ordering::Relaxed) => c.locate(chunk).map(|(fd, base, _)| (fd, base + (start - chunk * CHUNK))),
                Some(_) => None,
            };
            let Some((fd, off)) = target else { continue };
            let busy = self.busy(i);
            let cost = s.model.lock().map(|m| m.read_ms(span)).unwrap_or(1.0);
            let finish = busy.max(now) + cost;
            if best.is_none_or(|b| finish < b.3) {
                best = Some((i, fd, off, finish));
            }
        }
        let (idx, fd, off, finish) = best?;
        self.book(idx, finish);
        Some((idx, fd, off))
    }

    /// When a source's drive is free: sources on one drive (the original and a copy
    /// beside it) share its time.
    fn busy(&self, i: usize) -> f64 {
        let d = &self.sources[i].drive;
        self.sources.iter().filter(|s| &s.drive == d).map(|s| s.busy_until_ms.lock().map(|b| *b).unwrap_or(0.0)).fold(0.0, f64::max)
    }

    fn book(&self, i: usize, finish: f64) {
        let d = &self.sources[i].drive;
        for s in self.sources.iter().filter(|s| &s.drive == d) {
            if let Ok(mut b) = s.busy_until_ms.lock() {
                *b = b.max(finish);
            }
        }
    }

    /// Whether reads from source `i` bypass the page cache.
    fn direct(&self, i: usize) -> bool {
        match &self.sources[i].copy {
            None => self.primary_direct,
            Some(c) => c.direct(),
        }
    }

    /// Whether an expert-major copy can serve reads now.
    pub fn has_expert_copy(&self) -> bool {
        self.sources.iter().any(|s| s.online.load(Ordering::Relaxed) && s.copy.as_ref().is_some_and(|c| c.layout.is_some() && c.present() > 0))
    }

    /// Bytes worth reading through rather than seeking past, on source `i`.
    fn merge_gap(&self, i: usize) -> u64 {
        let m = self.sources[i].model.lock().map(|m| m.clone()).unwrap_or(DriveModel { name: String::new(), mb_s: 100.0, per_read_ms: 0.1 });
        ((m.per_read_ms * m.mb_s * 1e3) as u64).clamp(64 << 10, 4 << 20) / PAGE * PAGE
    }

    /// Plans the reads for model ranges [a, b) (page aligned, sorted or not): what an
    /// expert-major copy holds is mapped into the copy and read in as few pieces as
    /// possible (neighbours merged), when that finishes sooner than the plain sources;
    /// the rest goes in pieces of up to 1 MiB to whichever plain source finishes first.
    /// Time is booked on the chosen drives.
    pub fn plan(&self, runs: &[(u64, u64)], original_only: bool) -> Vec<Job> {
        self.plan_pieces(runs, original_only, PIECE)
    }

    /// Where each page of some model ranges should be read from, for engines that read
    /// straight into their own memory (hinted READ, the fork's expert pool): (source, fd,
    /// offset in that source's file, model offset, length), page aligned, sorted so each
    /// drive sees its reads in order. Same plan as prefetch, so an expert-major copy serves
    /// scattered experts with one seek each; plain pieces still end at chunk boundaries, as
    /// READ always cut them.
    pub fn place(&self, runs: &[(u64, u64)]) -> Vec<(usize, i32, u64, u64, u64)> {
        let mut out = Vec::new();
        for job in self.plan_pieces(runs, false, CHUNK) {
            for &(model_off, len, at) in &job.outs {
                let (a0, a1) = (at, at + len.div_ceil(PAGE) * PAGE);
                let mut pos = 0u64;
                for &(fd, off, rlen) in &job.reads {
                    let (a, b) = (a0.max(pos), a1.min(pos + rlen));
                    if a < b {
                        out.push((job.source, fd, off + (a - pos), model_off + (a - at), b - a));
                    }
                    pos += rlen;
                }
            }
        }
        out.sort_unstable_by_key(|p| (p.0, p.1, p.2));
        out
    }

    fn plan_pieces(&self, runs: &[(u64, u64)], original_only: bool, piece: u64) -> Vec<Job> {
        let mut jobs = Vec::new();
        let mut wanted: Vec<(u64, u64)> = runs.iter().copied().filter(|r| r.0 < r.1 && r.0 < self.size).collect();
        wanted.sort_unstable();
        let mut rest: Vec<(u64, u64)> = Vec::with_capacity(wanted.len());
        for r in wanted {
            match rest.last_mut() {
                Some(m) if r.0 <= m.1 => m.1 = m.1.max(r.1),
                _ => rest.push(r),
            }
        }
        if !original_only {
            for (i, s) in self.sources.iter().enumerate() {
                let Some(copy) = s.copy.as_ref() else { continue };
                let Some(lay) = copy.layout.as_ref() else { continue };
                if !s.online.load(Ordering::Relaxed) || copy.present() == 0 || rest.is_empty() {
                    continue;
                }
                let mut held: Vec<Seg> = Vec::new();
                let mut left = Vec::new();
                let mut segs = Vec::new();
                for &(a, b) in &rest {
                    segs.clear();
                    lay.cover(a, b, &mut segs);
                    if segs.iter().map(|x| x.len).sum::<u64>() == b - a && segs.iter().all(|x| copy.holds(x.copy, x.copy + x.len)) {
                        held.extend_from_slice(&segs);
                    } else {
                        left.push((a, b));
                    }
                }
                // One decision for the whole batch: the copy's reads against the same bytes
                // read as they lie in the file. Scattered experts favour the copy; a stretch
                // of the file (a blind 1 MiB block, a tensor scan) spans several experts' records
                // in the copy and is cheaper from the file. Deciding group by group missed
                // that and turned one read into two (measured on the WD, 2026-11-23).
                let model = s.model.lock().map(|m| m.clone()).ok();
                let mut planned = Vec::new();
                for (c0, c1, group) in group_reads(&mut held, self.merge_gap(i), MAX_JOB) {
                    match copy.spans(c0, c1) {
                        Some(spans) => planned.push((c0, c1, group, spans)),
                        None => left.extend(group.iter().map(|x| (x.orig, x.orig + x.len))),
                    }
                }
                if planned.is_empty() {
                    rest = left;
                    continue;
                }
                let now = self.now_ms();
                let finish = self.busy(i).max(now) + planned.iter().map(|p| model.as_ref().map_or(1.0, |m| m.read_ms(p.1 - p.0))).sum::<f64>();
                let mut plain: Vec<(u64, u64)> = planned.iter().flat_map(|p| p.2.iter().map(|x| (x.orig, x.orig + x.len))).collect();
                plain.sort_unstable();
                let mut runs_in_file: Vec<(u64, u64)> = Vec::new();
                for r in plain {
                    match runs_in_file.last_mut() {
                        Some(m) if r.0 <= m.1 => m.1 = m.1.max(r.1),
                        _ => runs_in_file.push(r),
                    }
                }
                let pieces: Vec<u64> = runs_in_file.iter().map(|r| r.1 - r.0).collect();
                let plain_finish = self
                    .sources
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.online.load(Ordering::Relaxed) && p.copy.as_ref().is_none_or(|c| c.layout.is_none() && c.present() > 0))
                    .map(|(j, p)| {
                        let m = p.model.lock().map(|m| m.clone()).ok();
                        self.busy(j).max(now) + pieces.iter().map(|&n| m.as_ref().map_or(1.0, |m| m.read_ms(n))).sum::<f64>()
                    })
                    .fold(f64::INFINITY, f64::min);
                if finish > plain_finish {
                    left.extend(runs_in_file);
                    rest = left;
                    continue;
                }
                self.book(i, finish);
                for (c0, _, group, spans) in planned {
                    let outs = group
                        .iter()
                        .filter(|x| x.orig < self.size)
                        .map(|x| (x.orig, x.len.min(self.size - x.orig), x.copy - c0))
                        .collect();
                    jobs.push(Job { source: i, reads: spans, outs });
                }
                rest = left;
            }
        }
        rest.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for r in rest {
            match merged.last_mut() {
                Some(m) if r.0 <= m.1 => m.1 = m.1.max(r.1),
                _ => merged.push(r),
            }
        }
        for (mut p, e) in merged {
            while p < e {
                let q = (p + piece).min((p / CHUNK + 1) * CHUNK).min(e);
                if let Some((src, fd, off)) = self.choose(p, q - p, original_only) {
                    jobs.push(Job { source: src, reads: vec![(fd, off, q - p)], outs: vec![(p, q.min(self.size) - p, 0)] });
                }
                p = q;
            }
        }
        jobs
    }

    /// Runs a planned job and hands each piece of model data to `sink` (model offset,
    /// bytes). A failed copy goes offline and its pieces are read from the original.
    /// Returns the bytes delivered.
    pub fn exec(&self, job: &Job, buf: &mut AlignedBuf, sink: &mut dyn FnMut(u64, &[u8]) -> io::Result<()>) -> io::Result<u64> {
        let span = job.span() as usize;
        if buf.as_slice().len() < span {
            *buf = AlignedBuf::new(span);
        }
        let need = job.outs.iter().map(|o| o.2 + o.1).max().unwrap_or(0) as usize;
        let mut filled = 0usize;
        let mut err: Option<io::Error> = None;
        for &(fd, off, len) in &job.reads {
            match pread_full(fd, &mut buf.as_mut_slice()[filled..filled + len as usize], off) {
                Ok(got) => {
                    filled += got;
                    if got < len as usize {
                        break;
                    }
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        if !self.direct(job.source) {
            for &(fd, off, len) in &job.reads {
                drop_cached(fd, off, len);
            }
        }
        if err.is_none() && filled >= need {
            let total: u64 = job.outs.iter().map(|o| o.1).sum();
            self.served(job.source, total);
            let data = buf.as_slice();
            for &(off, len, at) in &job.outs {
                sink(off, &data[at as usize..(at + len) as usize])?;
            }
            return Ok(total);
        }
        let why = err.as_ref().map_or_else(|| "short read".to_string(), |e| e.to_string());
        if job.source == 0 {
            self.sources[0].errors.fetch_add(1, Ordering::Relaxed);
            return Err(err.unwrap_or_else(|| io::Error::other("short read from the original")));
        }
        self.copy_failed(job.source, &why);
        let runs: Vec<(u64, u64)> = job.outs.iter().map(|o| (o.0, (o.0 + o.1).div_ceil(PAGE) * PAGE)).collect();
        let mut total = 0;
        for j in self.plan(&runs, true) {
            total += self.exec(&j, buf, sink)?;
        }
        Ok(total)
    }

    /// Records a read of model bytes [from, to) for idle detection and chunk heat.
    pub fn note_read(&self, from: u64, to: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.last_read_ms.store(self.now_ms() as u64, Ordering::Relaxed);
        let mut c = from / CHUNK;
        while c * CHUNK < to && (c as usize) < self.heat.len() {
            self.heat[c as usize].fetch_add(1, Ordering::Relaxed);
            c += 1;
        }
    }

    pub fn served(&self, idx: usize, bytes: u64) {
        let s = &self.sources[idx];
        s.bytes.fetch_add(bytes, Ordering::Relaxed);
        s.reads.fetch_add(1, Ordering::Relaxed);
    }

    /// A copy failed a read: it goes offline until restart.
    pub fn copy_failed(&self, idx: usize, why: &str) {
        let s = &self.sources[idx];
        s.errors.fetch_add(1, Ordering::Relaxed);
        if idx != 0 && s.online.swap(false, Ordering::Relaxed) {
            eprintln!("{}: reading the copy on {} failed ({why}); using the original until restart.", crate::brand::BIN, s.drive);
        }
    }

    /// Reads one block from the soonest source; falls back to the original if a copy fails.
    fn fetch(&self, block: u64, original_only: bool) -> io::Result<Chunk> {
        let start = block * BLOCK;
        let len = (self.size - start).min(BLOCK);
        let span = len.div_ceil(ALIGN) * ALIGN;
        if !original_only && self.has_expert_copy() {
            let mut out = AlignedBuf::new(span as usize);
            let mut tmp = AlignedBuf::new(BLOCK as usize);
            for job in self.plan(&[(start, start + span)], false) {
                self.exec(&job, &mut tmp, &mut |off, data| {
                    let at = (off - start) as usize;
                    out.as_mut_slice()[at..at + data.len()].copy_from_slice(data);
                    Ok(())
                })?;
            }
            return Ok(Chunk { buf: out, len: len as usize });
        }
        let (idx, fd, off) = self.choose(start, span, original_only).ok_or_else(|| io::Error::other("no source for chunk"))?;
        let mut buf = AlignedBuf::new(span as usize);
        let res = pread_full(fd, buf.as_mut_slice(), off);
        if !self.direct(idx) {
            drop_cached(fd, off, span);
        }
        match res {
            Ok(got) if got as u64 >= len => {
                let s = &self.sources[idx];
                s.bytes.fetch_add(len, Ordering::Relaxed);
                s.reads.fetch_add(1, Ordering::Relaxed);
                Ok(Chunk { buf, len: len as usize })
            }
            res if idx != 0 => {
                let s = &self.sources[idx];
                s.errors.fetch_add(1, Ordering::Relaxed);
                let why = match res {
                    Err(e) => e.to_string(),
                    Ok(_) => "short read".into(),
                };
                if s.online.swap(false, Ordering::Relaxed) {
                    eprintln!("{}: reading the copy on {} failed ({why}); using the original until restart.", crate::brand::BIN, s.drive);
                }
                self.fetch(block, true)
            }
            res => {
                self.sources[0].errors.fetch_add(1, Ordering::Relaxed);
                Err(res.err().unwrap_or_else(|| io::Error::other("short read from the original")))
            }
        }
    }

    /// Fills the copies, hottest chunks first, only while nothing is reading.
    pub fn run_copier(self: Arc<Self>) {
        if self.sources.len() < 2 {
            return;
        }
        let mut buf = AlignedBuf::new(CHUNK as usize);
        let mut next_copy = 1;
        while !self.stop.load(Ordering::Relaxed) {
            let open: Vec<usize> = (1..self.sources.len())
                .filter(|i| self.sources[*i].online.load(Ordering::Relaxed) && !self.sources[*i].copy.as_ref().is_some_and(|c| c.is_full()))
                .collect();
            if open.is_empty() {
                self.copier.store(CopierState::Done as u8, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            let idle_for = self.now_ms() as u64 - self.last_read_ms.load(Ordering::Relaxed).min(self.now_ms() as u64);
            if self.requests.load(Ordering::Relaxed) > 0 && idle_for < IDLE_MS {
                self.copier.store(CopierState::Waiting as u8, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            self.copier.store(CopierState::Copying as u8, Ordering::Relaxed);
            let i = *open.iter().find(|i| **i >= next_copy).unwrap_or(&open[0]);
            next_copy = i + 1;
            let Some(copy) = self.sources[i].copy.clone() else { continue };
            let res = if copy.layout.is_some() {
                self.copy_next_expert_chunk(&copy, &mut buf)
            } else {
                // Hottest chunk this copy lacks, else the first one it lacks.
                let chunk = (0..self.chunks)
                    .filter(|k| !copy.has(*k))
                    .max_by_key(|k| (self.heat[*k as usize].load(Ordering::Relaxed), std::cmp::Reverse(*k)));
                let Some(chunk) = chunk else { continue };
                let slice = buf.as_mut_slice();
                slice.fill(0);
                let res = pread_full(self.primary.as_raw_fd(), slice, chunk * CHUNK);
                if !self.primary_direct {
                    drop_cached(self.primary.as_raw_fd(), chunk * CHUNK, CHUNK);
                }
                res.and_then(|_| copy.append(chunk, buf.as_mut_slice()))
            };
            match res {
                Ok(()) => {
                    self.copied_bytes.fetch_add(CHUNK, Ordering::Relaxed);
                    if !self.sources[i].measured.load(Ordering::Relaxed) && copy.present() >= MEASURE_AT {
                        let _ = self.measure(i);
                    }
                }
                Err(e) => {
                    if self.sources[i].online.swap(false, Ordering::Relaxed) {
                        eprintln!("{}: copying to {} failed ({e}); that drive is offline until restart.", crate::brand::BIN, self.sources[i].drive);
                    }
                }
            }
        }
    }

    /// Copies the next chunk of an expert-major copy. These fill in their own order, so
    /// what is copied is one contiguous stretch and neighbouring experts stay neighbours on
    /// the drive.
    fn copy_next_expert_chunk(&self, copy: &CopyStore, buf: &mut AlignedBuf) -> io::Result<()> {
        let lay = copy.layout.as_ref().ok_or_else(|| io::Error::other("not an expert copy"))?;
        let chunk = copy.present();
        let (c0, c1) = (chunk * CHUNK, ((chunk + 1) * CHUNK).min(lay.space));
        if c0 >= c1 {
            return Err(io::Error::other("the copy is complete"));
        }
        let slice = buf.as_mut_slice();
        slice.fill(0);
        for (orig, at, len) in lay.sources_of(c0, c1) {
            let res = pread_full(self.primary.as_raw_fd(), &mut slice[at as usize..(at + len) as usize], orig);
            if !self.primary_direct {
                drop_cached(self.primary.as_raw_fd(), orig, len);
            }
            res?;
        }
        copy.append(chunk, buf.as_mut_slice())
    }

    pub fn status_json(&self) -> String {
        let js = crate::js;
        let sources: Vec<String> = self
            .sources
            .iter()
            .map(|s| {
                let m = s.model.lock().map(|m| m.clone()).unwrap_or(DriveModel { name: s.drive.clone(), mb_s: 0.0, per_read_ms: 0.0 });
                format!(
                    "{{\"drive\":{},\"role\":{},\"layout\":{},\"online\":{},\"measured\":{},\"mb_s\":{:.1},\"per_read_ms\":{:.2},\"chunks\":{},\"bytes_served\":{},\"reads\":{},\"errors\":{}}}",
                    js(&s.drive),
                    js(if s.copy.is_some() { "copy" } else { "original" }),
                    js(if s.copy.as_ref().is_some_and(|c| c.layout.is_some()) { "experts" } else { "file" }),
                    s.online.load(Ordering::Relaxed),
                    s.measured.load(Ordering::Relaxed),
                    m.mb_s,
                    m.per_read_ms,
                    s.chunks(self.chunks),
                    s.bytes.load(Ordering::Relaxed),
                    s.reads.load(Ordering::Relaxed),
                    s.errors.load(Ordering::Relaxed)
                )
            })
            .collect();
        let state = match self.copier_state() {
            CopierState::Waiting => "waiting for idle",
            CopierState::Copying => "copying",
            CopierState::Done => "done",
            CopierState::None => "no copies",
        };
        format!(
            "{{\"model\":{{\"name\":{},\"path\":{},\"bytes\":{},\"chunks\":{},\"chunk_bytes\":{}}},\"uptime_s\":{:.1},\"requests\":{},\"cache\":{{\"chunks\":{},\"capacity\":{},\"hits\":{},\"misses\":{}}},\"prefetch\":{{\"stored_bytes\":{},\"already_resident_bytes\":{},\"async_dropped\":{},\"joined_bytes\":{}}},\"copier\":{{\"state\":{},\"copied_bytes\":{}}},\"sources\":[{}]}}",
            js(&self.name),
            js(&self.path.to_string_lossy()),
            self.size,
            self.chunks,
            CHUNK,
            self.t0.elapsed().as_secs_f64(),
            self.requests.load(Ordering::Relaxed),
            self.cache.inner.lock().map(|c| c.ready.len()).unwrap_or(0),
            self.cache.cap,
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_misses.load(Ordering::Relaxed),
            self.prefetched.load(Ordering::Relaxed),
            self.prefetch_skipped.load(Ordering::Relaxed),
            self.prefetch_dropped.load(Ordering::Relaxed),
            self.prefetch_joined.load(Ordering::Relaxed),
            js(state),
            self.copied_bytes.load(Ordering::Relaxed),
            sources.join(",")
        )
    }
}

/// Drops a range of a file from the page cache (after a read that could not bypass it).
fn drop_cached(fd: i32, off: u64, len: u64) {
    unsafe { libc::posix_fadvise(fd, off as libc::off_t, len as libc::off_t, libc::POSIX_FADV_DONTNEED) };
}

/// pread until the buffer is full or end of file; returns bytes read.
pub(crate) fn pread_full(fd: i32, buf: &mut [u8], off: u64) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        let n = unsafe { libc::pread(fd, buf[got..].as_mut_ptr().cast(), buf.len() - got, (off + got as u64) as libc::off_t) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            break;
        }
        got += n as usize;
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{ExpertMap, ExpertTensor};
    use std::path::Path;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A 9 MB stand-in model: 3 layers × 3 tensors × 10 experts of 70,001 bytes, so slices
    /// start and end mid-page, and the file ends mid-page too.
    fn stand_in(tag: &str) -> (TempDir, PathBuf, ExpertMap, Vec<u8>) {
        let dir = std::env::temp_dir().join(format!("corsia-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut layers = Vec::new();
        let mut pos = 300_000u64;
        for _ in 0..3 {
            pos += 500_000;
            let mut ts = Vec::new();
            for _ in 0..3 {
                pos = pos.div_ceil(32) * 32;
                ts.push(ExpertTensor { offs: pos, stride: 70_001, n_expert: 10 });
                pos += 70_001 * 10;
            }
            layers.push(ts);
        }
        let size = pos + 1_234_567;
        let bytes: Vec<u8> = (0..size).map(|i| (i.wrapping_mul(0x9E37_79B9) >> 11) as u8).collect();
        let path = dir.join("model.gguf");
        std::fs::write(&path, &bytes).unwrap();
        (TempDir(dir), path, ExpertMap { file_size: size, layers }, bytes)
    }

    fn hdd() -> DriveModel {
        DriveModel { name: "hdd".into(), mb_s: 100.0, per_read_ms: 10.0 }
    }

    fn engine_with_copy(path: &Path, dir: &Path, map: &ExpertMap, max_chunks: u64) -> Engine {
        let orders = vec![(0..10).rev().collect(), vec![3, 7, 1, 0, 2, 9, 8, 4, 5, 6], (0..10).collect()];
        let (copy, kept) = CopyStore::open_experts("hdd", dir, "model.gguf", map, max_chunks, || orders).unwrap();
        assert!(!kept);
        // The copy sits on the original's own drive, as on a machine with one hard disk.
        let engine = Engine::new(path.to_path_buf(), "hdd", hdd(), vec![(Arc::new(copy), hdd())], 16).unwrap();
        let mut buf = AlignedBuf::new(CHUNK as usize);
        let copy = engine.sources[1].copy.clone().unwrap();
        while !copy.is_full() {
            engine.copy_next_expert_chunk(&copy, &mut buf).unwrap();
        }
        engine
    }

    fn check_reads(engine: &Engine, orig: &[u8]) {
        let mut rng = 0x1234_5678u64;
        for _ in 0..300 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let off = (rng >> 17) % orig.len() as u64;
            let len = 1 + ((rng >> 45) % 300_000) as u32;
            let got = engine.read(off, len).unwrap();
            let end = (off + len as u64).min(orig.len() as u64) as usize;
            assert!(got == orig[off as usize..end], "read at {off} len {len} differs");
        }
    }

    #[test]
    fn reads_through_an_expert_copy_match_the_original() {
        let (tmp, path, map, orig) = stand_in("full");
        let engine = engine_with_copy(&path, &tmp.0, &map, u64::MAX);
        check_reads(&engine, &orig);
        assert!(engine.sources[1].bytes.load(Ordering::Relaxed) > 0, "the copy served reads");

        // A prefetch of one expert: one read, from the copy, every byte right.
        let runs: Vec<(u64, u64)> = map.slices(1, 4).collect();
        let jobs = engine.plan(&runs, false);
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0].source, 1);
        // A stretch of the file (a blind 1 MiB block inside a tensor) spans many experts'
        // records in the copy, so it comes from the original as it lies.
        let t = &map.layers[1][0];
        let a = (t.offs + 4096) / PAGE * PAGE;
        let block = engine.plan(&[(a, a + (1 << 20))], false);
        assert!(block.len() <= 2 && block.iter().all(|j| j.source == 0), "{block:?}");
        // Neighbours in the copy's order (layer 1: ... 2, 9, 8 ...) are one read too.
        let runs: Vec<(u64, u64)> = [2, 9, 8].iter().flat_map(|&e| map.slices(1, e)).collect();
        let jobs = engine.plan(&runs, false);
        assert_eq!(jobs.len(), 1);
        let mut buf = AlignedBuf::new(4096);
        let mut seen = 0u64;
        for j in &jobs {
            engine
                .exec(j, &mut buf, &mut |off, data| {
                    assert_eq!(off % PAGE, 0);
                    assert!(data == &orig[off as usize..off as usize + data.len()]);
                    seen += data.len() as u64;
                    Ok(())
                })
                .unwrap();
        }
        let wanted: u64 = {
            let mut r = runs.clone();
            r.sort_unstable();
            let mut m: Vec<(u64, u64)> = Vec::new();
            for x in r {
                match m.last_mut() {
                    Some(l) if x.0 <= l.1 => l.1 = l.1.max(x.1),
                    _ => m.push(x),
                }
            }
            m.iter().map(|x| x.1 - x.0).sum()
        };
        assert_eq!(seen, wanted, "every wanted page delivered once");

        // Reopening keeps the layout the copy was written with, whatever is asked now.
        let (again, kept) = CopyStore::open_experts("hdd", &tmp.0, "model.gguf", &map, u64::MAX, || panic!("must not re-lay")).unwrap();
        assert!(kept);
        assert_eq!(again.present(), engine.sources[1].copy.as_ref().unwrap().present());
    }

    /// Reads what place() says, straight from the files, like hinted READ does.
    fn read_places(engine: &Engine, runs: &[(u64, u64)], orig: &[u8]) -> Vec<(usize, i32, u64, u64, u64)> {
        let places = engine.place(runs);
        for &(_, fd, off, m, len) in &places {
            let mut buf = AlignedBuf::new(len as usize);
            let got = pread_full(fd, buf.as_mut_slice(), off).unwrap();
            let valid = (orig.len() as u64).saturating_sub(m).min(len) as usize;
            assert!(got >= valid);
            assert!(buf.as_slice()[..valid] == orig[m as usize..m as usize + valid], "place at model {m} differs");
        }
        let total: u64 = places.iter().map(|p| p.4).sum();
        let mut merged: Vec<(u64, u64)> = runs.to_vec();
        merged.sort_unstable();
        let mut m: Vec<(u64, u64)> = Vec::new();
        for r in merged {
            match m.last_mut() {
                Some(l) if r.0 <= l.1 => l.1 = l.1.max(r.1),
                _ => m.push(r),
            }
        }
        assert_eq!(total, m.iter().map(|r| r.1 - r.0).sum::<u64>(), "every wanted page placed once");
        places
    }

    #[test]
    fn hinted_reads_use_the_expert_copy() {
        let (tmp, path, map, orig) = stand_in("place");
        let engine = engine_with_copy(&path, &tmp.0, &map, u64::MAX);
        // One expert's three slices: from the copy, back to back on the drive.
        let runs: Vec<(u64, u64)> = map.slices(2, 7).collect();
        let places = read_places(&engine, &runs, &orig);
        assert!(places.iter().all(|p| p.0 == 1), "{places:?}");
        assert!(places.windows(2).all(|w| w[0].1 == w[1].1 && w[0].2 + w[0].4 == w[1].2), "contiguous in the copy: {places:?}");
        // A stretch of the file: from the original.
        let t = &map.layers[0][1];
        let a = (t.offs + 8192) / PAGE * PAGE;
        let places = read_places(&engine, &[(a, a + (1 << 20))], &orig);
        assert!(places.iter().all(|p| p.0 == 0), "{places:?}");
        // The end of the file, past the last page boundary.
        let end = (orig.len() as u64).div_ceil(PAGE) * PAGE;
        read_places(&engine, &[(end - 3 * PAGE, end)], &orig);
    }

    #[test]
    fn a_partial_expert_copy_falls_back_to_the_original() {
        let (tmp, path, map, orig) = stand_in("partial");
        let engine = engine_with_copy(&path, &tmp.0, &map, 1);
        assert_eq!(engine.sources[1].copy.as_ref().unwrap().present(), 1);
        check_reads(&engine, &orig);
        assert!(engine.sources[0].bytes.load(Ordering::Relaxed) > 0, "the original served what the copy lacks");
    }
}
