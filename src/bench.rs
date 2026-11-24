//! The lane bench: measures each drive alone, then every drive at once.
//!
//! One thread per drive, each with its own io_uring queue, the same shape the adapter's
//! scheduler will use. Reads bypass the page cache (O_DIRECT) so the numbers are the
//! drive's, not RAM's. Test files hold random bytes so compressing filesystems can't cheat.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use io_uring::{opcode, types, IoUring};

pub static STOP: AtomicBool = AtomicBool::new(false);

const ALIGN: usize = 4096;
const WRITE_CHUNK: usize = 4 << 20;

#[derive(Clone, Copy, Debug)]
pub struct Pattern {
    pub name: &'static str,
    pub block: usize,
    pub depth: usize,
    pub random: bool,
}

/// Solo tests, in the order shown.
pub const SOLO: [Pattern; 4] = [
    Pattern { name: "Sequential 1 MiB, 8 deep", block: 1 << 20, depth: 8, random: false },
    Pattern { name: "Random 64 KiB, 8 deep", block: 64 << 10, depth: 8, random: true },
    Pattern { name: "Random 1 MiB, 1 deep", block: 1 << 20, depth: 1, random: true },
    Pattern { name: "Random 4 MiB, 4 deep", block: 4 << 20, depth: 4, random: true },
];

/// The expert-sized read the adapter will issue most: used for the all-at-once test.
pub const TOGETHER: Pattern = SOLO[3];

#[derive(Clone, Debug, Default)]
pub struct Measure {
    pub mb_s: f64,
    pub p50_us: u32,
    pub p99_us: u32,
    pub reads: u64,
}

pub struct Target {
    pub drive: String,
    pub dir: PathBuf,
    pub file: PathBuf,
    pub file_bytes: u64,
    /// False when the filesystem refused O_DIRECT; the page cache is dropped before each test instead.
    pub direct: bool,
    created_dir: bool,
}

impl Target {
    /// Writes the test file (or reuses one of the right size) and checks O_DIRECT support.
    pub fn prepare(drive: &str, dir: PathBuf, mib: u64) -> io::Result<Target> {
        let created_dir = !dir.exists();
        fs::create_dir_all(&dir)?;
        let file = dir.join(format!("corsia-test-{mib}m.bin"));
        let file_bytes = mib << 20;
        let reuse = fs::metadata(&file).map(|m| m.len() == file_bytes).unwrap_or(false);
        if !reuse {
            write_random(&file, file_bytes)?;
        }
        let direct = OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(&file).is_ok();
        Ok(Target { drive: drive.to_string(), dir, file, file_bytes, direct, created_dir })
    }

    pub fn remove(&self) {
        let _ = fs::remove_file(&self.file);
        if self.created_dir {
            let _ = fs::remove_dir(&self.dir);
        }
    }

    pub(crate) fn open(&self) -> io::Result<File> {
        let mut o = OpenOptions::new();
        o.read(true);
        if self.direct {
            o.custom_flags(libc::O_DIRECT);
        }
        let f = o.open(&self.file)?;
        // Without O_DIRECT, at least start cold.
        unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        Ok(f)
    }
}

fn write_random(path: &PathBuf, bytes: u64) -> io::Result<()> {
    let direct = OpenOptions::new().write(true).create(true).truncate(true).custom_flags(libc::O_DIRECT).open(path);
    let mut f = match direct {
        Ok(f) => f,
        Err(_) => OpenOptions::new().write(true).create(true).truncate(true).open(path)?,
    };
    let mut buf = AlignedBuf::new(WRITE_CHUNK);
    let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ bytes;
    let mut left = bytes;
    while left > 0 {
        if STOP.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
        }
        for word in buf.as_mut_slice().chunks_exact_mut(8) {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            word.copy_from_slice(&rng.to_le_bytes());
        }
        let n = left.min(WRITE_CHUNK as u64) as usize;
        f.write_all(&buf.as_mut_slice()[..n])?;
        left -= n as u64;
    }
    f.sync_all()?;
    unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    Ok(())
}

/// Runs one pattern on one target for `secs` seconds.
pub fn run(target: &Target, p: Pattern, secs: f64, seed: u64) -> io::Result<Measure> {
    let f = target.open()?;
    read_loop(f.as_raw_fd(), target.file_bytes, p, Duration::from_secs_f64(secs), seed)
}

/// Runs the same pattern on every target at once, starting together.
pub fn run_together(targets: &[Target], p: Pattern, secs: f64) -> Vec<io::Result<Measure>> {
    let barrier = Arc::new(Barrier::new(targets.len()));
    std::thread::scope(|s| {
        let handles: Vec<_> = targets
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let barrier = barrier.clone();
                s.spawn(move || {
                    let f = t.open();
                    // Every thread reaches the barrier, even one whose open failed.
                    barrier.wait();
                    let f = f?;
                    read_loop(f.as_raw_fd(), t.file_bytes, p, Duration::from_secs_f64(secs), 0xC0FFEE + i as u64)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err(io::Error::other("bench thread panicked"))))
            .collect()
    })
}

pub(crate) fn read_loop(fd: i32, file_bytes: u64, p: Pattern, dur: Duration, seed: u64) -> io::Result<Measure> {
    let entries = (p.depth as u32).next_power_of_two().max(4);
    let mut ring = IoUring::new(entries)?;
    let mut bufs: Vec<AlignedBuf> = (0..p.depth).map(|_| AlignedBuf::new(p.block)).collect();
    let blocks = (file_bytes / p.block as u64).max(1);
    let mut offsets = Offsets { random: p.random, blocks, block: p.block as u64, next: 0, rng: seed | 1 };
    let mut started = vec![Instant::now(); p.depth];
    let mut lat_us: Vec<u32> = Vec::new();
    let mut bytes = 0u64;
    let mut error: Option<io::Error> = None;

    let t0 = Instant::now();
    let deadline = t0 + dur;
    for (slot, buf) in bufs.iter_mut().enumerate() {
        started[slot] = Instant::now();
        push_read(&mut ring, fd, buf, offsets.next(), slot)?;
    }
    let mut in_flight = p.depth;
    let mut last = t0;
    while in_flight > 0 {
        ring.submit_and_wait(1)?;
        let done: Vec<(u64, i32)> = ring.completion().map(|c| (c.user_data(), c.result())).collect();
        let now = Instant::now();
        for (slot, res) in done {
            let slot = slot as usize;
            in_flight -= 1;
            if res < 0 {
                error.get_or_insert(io::Error::from_raw_os_error(-res));
                continue;
            }
            if now <= deadline {
                bytes += res as u64;
                lat_us.push(now.duration_since(started[slot]).as_micros().min(u32::MAX as u128) as u32);
                last = now;
            }
            // Keep draining in-flight reads after an error or stop: the kernel still owns their buffers.
            if now < deadline && error.is_none() && !STOP.load(Ordering::Relaxed) {
                started[slot] = Instant::now();
                push_read(&mut ring, fd, &mut bufs[slot], offsets.next(), slot)?;
                in_flight += 1;
            }
        }
    }
    if let Some(e) = error {
        return Err(e);
    }
    let secs = last.duration_since(t0).as_secs_f64().max(1e-6);
    lat_us.sort_unstable();
    let pct = |q: f64| lat_us.get(((lat_us.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0);
    Ok(Measure { mb_s: bytes as f64 / secs / 1e6, p50_us: pct(0.5), p99_us: pct(0.99), reads: lat_us.len() as u64 })
}

fn push_read(ring: &mut IoUring, fd: i32, buf: &mut AlignedBuf, offset: u64, slot: usize) -> io::Result<()> {
    let entry = opcode::Read::new(types::Fd(fd), buf.ptr, buf.len as u32).offset(offset).build().user_data(slot as u64);
    // SAFETY: the buffer outlives the read; read_loop drains every in-flight read before returning.
    unsafe { ring.submission().push(&entry) }.map_err(|_| io::Error::other("submission queue full"))
}

struct Offsets {
    random: bool,
    blocks: u64,
    block: u64,
    next: u64,
    rng: u64,
}

impl Offsets {
    fn next(&mut self) -> u64 {
        let i = if self.random {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            self.rng % self.blocks
        } else {
            let i = self.next;
            self.next = (self.next + 1) % self.blocks;
            i
        };
        i * self.block
    }
}

/// A page-aligned heap buffer, as O_DIRECT requires.
pub(crate) struct AlignedBuf {
    pub(crate) ptr: *mut u8,
    pub(crate) len: usize,
    layout: std::alloc::Layout,
}

impl AlignedBuf {
    pub(crate) fn new(len: usize) -> AlignedBuf {
        let layout = std::alloc::Layout::from_size_align(len.max(ALIGN), ALIGN).expect("buffer layout");
        // SAFETY: layout has a non-zero size.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        AlignedBuf { ptr, len, layout }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr is valid for len bytes and uniquely borrowed here.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr is valid for len bytes; shared borrows only read.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

// SAFETY: the buffer owns its allocation outright; mutation needs &mut, so sharing it
// between threads (the daemon's chunk cache) only ever reads.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in new() with this layout.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}
