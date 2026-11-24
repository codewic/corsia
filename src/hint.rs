//! Hinted mode: an engine reads byte ranges of the model through a private local socket,
//! straight into memory it shares with Corsia. No FUSE, no copies: the drives write into
//! the shared memory directly (O_DIRECT) and the reply says where each range starts.
//!
//! Protocol, version 1. Unix SOCK_SEQPACKET, one message per packet, little endian.
//! Every message starts with an 8-byte header: magic u32 "CORS", version u16, type u16.
//!
//! ```text
//! HELLO     client  type 0x00  arena_bytes u64          + the arena as a memfd (SCM_RIGHTS)
//! HELLO_OK  corsia  type 0x80  model_bytes u64, align u32, max_ranges u32, chunk_bytes u64
//! READ      client  type 0x01  batch u64, n u32, 0 u32, then n x (offset u64, len u32, 0 u32, arena_off u64)
//! READ_OK   corsia  type 0x81  batch u64, status i32 (0 or errno), n u32, then n x (data_off u64, got u32, source u8, 0 x3)
//! PREFETCH  client  type 0x02  batch u64, n u32, flags u32, then n x (offset u64, len u32, 0 u32)
//!                              flags bit 0: async, run in the background and send no reply
//! PREFETCH_OK corsia type 0x82 batch u64, status i32, pieces u32, bytes stored u64
//! ERROR     corsia  type 0xFF  code i32, then a UTF-8 message
//! ```
//!
//! `arena_off` must be a multiple of 4096 and leave room for the range rounded out to
//! 4096 on both ends (len + 8192 is always enough). The data for a range starts at
//! `data_off` inside the arena. One batch is one decision: Corsia spreads all its pieces
//! over the drives at once, so a batch should be what the engine needs next (a layer's
//! experts, say).
//!
//! PREFETCH is for engines that memory-map the model from Corsia's mount: Corsia reads
//! the pages of the ranges that are not already cached (spread over the drives) and
//! stores them straight into the kernel's page cache for the mounted file, then replies. The engine's next touch
//! of those pages is a plain memory access.

use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use io_uring::{opcode, types, IoUring};

use crate::engine::Engine;
use crate::store::CHUNK;

pub const MAGIC: u32 = u32::from_le_bytes(*b"CORS");
pub const VERSION: u16 = 1;
pub const T_HELLO: u16 = 0x00;
pub const T_READ: u16 = 0x01;
pub const T_PREFETCH: u16 = 0x02;
pub const T_PREFETCH_OK: u16 = 0x82;
pub const PRE_RANGE: usize = 16;
pub const T_HELLO_OK: u16 = 0x80;
pub const T_READ_OK: u16 = 0x81;
pub const T_ERROR: u16 = 0xFF;
pub const ALIGN: u64 = 4096;
pub const MAX_RANGES: usize = 2048;
pub const HEAD: usize = 8;
pub const REQ_RANGE: usize = 24;
pub const REP_RANGE: usize = 16;
/// Largest message either side sends.
pub const MAX_MSG: usize = HEAD + 16 + MAX_RANGES * REQ_RANGE;
const IN_FLIGHT: usize = 64;

/// `$XDG_RUNTIME_DIR/corsia/<model>.sock`, private to this user.
pub fn default_socket(model_name: &str) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(format!("/tmp/corsia-{}", unsafe { libc::getuid() })));
    base.join("corsia").join(format!("{model_name}.sock"))
}

pub fn header(t: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(64);
    v.extend_from_slice(&MAGIC.to_le_bytes());
    v.extend_from_slice(&VERSION.to_le_bytes());
    v.extend_from_slice(&t.to_le_bytes());
    v
}

pub fn parse_header(b: &[u8]) -> io::Result<u16> {
    if b.len() < HEAD || u32::from_le_bytes(b[0..4].try_into().unwrap()) != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a Corsia message"));
    }
    if u16::from_le_bytes(b[4..6].try_into().unwrap()) != VERSION {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported protocol version"));
    }
    Ok(u16::from_le_bytes(b[6..8].try_into().unwrap()))
}

pub fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
pub fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// Starts listening; one thread per connected engine.
/// The daemon's view of its own mounted file, opened on first PREFETCH (the mount must be
/// serving first) and released before unmounting, since a live mapping keeps the mount busy.
static RESIDENT: std::sync::Mutex<(Option<PathBuf>, Option<Resident>, bool)> = std::sync::Mutex::new((None, None, false));

/// Drops the daemon's own mapping of the mount; call before unmounting.
pub fn release_resident() {
    if let Ok(mut r) = RESIDENT.lock() {
        r.1 = None;
        r.2 = true;
    }
}

/// True when something accepts connections on the socket at `path` (a live Corsia).
fn socket_is_live(path: &Path) -> bool {
    let Ok((addr, len)) = sockaddr(path) else { return false };
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return false;
    }
    let live = unsafe { libc::connect(fd, (&addr as *const libc::sockaddr_un).cast(), len) } == 0;
    unsafe { libc::close(fd) };
    live
}

pub fn start(engine: Arc<Engine>, path: &Path, notifier: Option<fuser::Notifier>, mounted_file: Option<PathBuf>) -> io::Result<()> {
    if let Ok(mut r) = RESIDENT.lock() {
        r.0 = mounted_file;
    }
    let dir = path.parent().ok_or_else(|| io::Error::other("socket path has no folder"))?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_socket() {
            // Only a stale socket may go: if another Corsia still answers on it, taking it
            // over would cut that engine off without a word.
            if socket_is_live(path) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another Corsia is already serving on {}; stop it, or give this one its own socket with --socket", path.display()),
                ));
            }
            std::fs::remove_file(path)?;
        } else {
            return Err(io::Error::other(format!("{} exists and is not a socket", path.display())));
        }
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let (addr, len) = sockaddr(path)?;
    if unsafe { libc::bind(fd, (&addr as *const libc::sockaddr_un).cast(), len) } != 0 || unsafe { libc::listen(fd, 16) } != 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::spawn(move || {
        loop {
            let c = unsafe { libc::accept4(fd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
            if c < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if !same_user(c) {
                unsafe { libc::close(c) };
                continue;
            }
            let engine = engine.clone();
            let notifier = notifier.clone();
            std::thread::spawn(move || {
                if let Err(e) = serve_client(c, &engine, notifier.as_ref()) {
                    let mut m = header(T_ERROR);
                    m.extend_from_slice(&(e.raw_os_error().unwrap_or(libc::EIO)).to_le_bytes());
                    m.extend_from_slice(e.to_string().as_bytes());
                    let _ = send_msg(c, &m, None);
                }
                unsafe { libc::close(c) };
            });
        }
    });
    Ok(())
}

pub fn sockaddr(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::other("socket path is too long"));
    }
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    Ok((addr, (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t))
}

fn same_user(fd: RawFd) -> bool {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ok = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) } == 0;
    ok && cred.uid == unsafe { libc::getuid() }
}

/// The client's arena, mapped into Corsia for the life of the connection.
struct Arena {
    ptr: *mut u8,
    len: u64,
}

impl Drop for Arena {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), self.len as usize) };
    }
}

fn serve_client(fd: RawFd, engine: &Engine, notifier: Option<&fuser::Notifier>) -> io::Result<()> {
    let mut buf = vec![0u8; MAX_MSG];
    let (n, arena_fd) = recv_msg(fd, &mut buf)?;
    if parse_header(&buf[..n])? != T_HELLO || n < HEAD + 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected HELLO"));
    }
    let arena_fd = arena_fd.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "HELLO came without the shared memory"))?;
    let arena_len = u64_at(&buf, HEAD);
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(arena_fd, &mut st) } != 0 || (st.st_size as u64) < arena_len || arena_len == 0 {
        unsafe { libc::close(arena_fd) };
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the shared memory is smaller than announced"));
    }
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), arena_len as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, arena_fd, 0) };
    unsafe { libc::close(arena_fd) };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let arena = Arena { ptr: ptr.cast(), len: arena_len };

    let mut ok = header(T_HELLO_OK);
    ok.extend_from_slice(&engine.size.to_le_bytes());
    ok.extend_from_slice(&(ALIGN as u32).to_le_bytes());
    ok.extend_from_slice(&(MAX_RANGES as u32).to_le_bytes());
    ok.extend_from_slice(&CHUNK.to_le_bytes());
    send_msg(fd, &ok, None)?;

    let mut ring = IoUring::new(IN_FLIGHT as u32)?;
    // Async prefetches (flag bit 0) run on this connection's worker and get no reply, so
    // the engine never waits for a guess. Only the newest few are kept: a prediction that
    // is still queued when a newer one arrives is already stale.
    let queue = AsyncQueue::default();
    std::thread::scope(|sc| {
        sc.spawn(|| queue.work(engine, notifier));
        let result = (|| loop {
            let (n, stray) = recv_msg(fd, &mut buf)?;
            if let Some(s) = stray {
                unsafe { libc::close(s) };
            }
            if n == 0 {
                return Ok(()); // the engine hung up
            }
            match parse_header(&buf[..n])? {
                T_READ => {
                    let reply = read_batch(engine, &arena, &mut ring, &buf[..n]);
                    send_msg(fd, &reply, None)?;
                }
                T_PREFETCH if n >= HEAD + 16 && u32_at(&buf, HEAD + 12) & PREFETCH_ASYNC != 0 => {
                    queue.push(buf[..n].to_vec(), engine);
                }
                T_PREFETCH => {
                    let reply = prefetch_batch(engine, notifier, &buf[..n], false);
                    send_msg(fd, &reply, None)?;
                }
                t => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown message type {t:#x}"))),
            }
        })();
        queue.close();
        result
    })
}

/// PREFETCH flag: run in the background, send no reply.
pub const PREFETCH_ASYNC: u32 = 1;
/// Async prefetches kept waiting per connection; older ones are dropped.
const ASYNC_KEEP: usize = 2;

#[derive(Default)]
struct AsyncQueue {
    q: std::sync::Mutex<(std::collections::VecDeque<Vec<u8>>, bool)>,
    cv: std::sync::Condvar,
}

impl AsyncQueue {
    fn push(&self, msg: Vec<u8>, engine: &Engine) {
        let mut g = self.q.lock().unwrap_or_else(|e| e.into_inner());
        g.0.push_back(msg);
        while g.0.len() > ASYNC_KEEP {
            g.0.pop_front();
            engine.prefetch_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.cv.notify_one();
    }

    fn close(&self) {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.cv.notify_all();
    }

    fn work(&self, engine: &Engine, notifier: Option<&fuser::Notifier>) {
        loop {
            let msg = {
                let mut g = self.q.lock().unwrap_or_else(|e| e.into_inner());
                while g.0.is_empty() && !g.1 {
                    g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                match g.0.pop_front() {
                    Some(m) => m,
                    None => return, // closed and drained
                }
            };
            let _ = prefetch_batch(engine, notifier, &msg, true);
        }
    }
}

struct Piece {
    range: usize,
    source: usize,
    fd: RawFd,
    src_off: u64,
    model_off: u64,
    dst: u64,
    span: u64,
    need: u64,
}

fn read_batch(engine: &Engine, arena: &Arena, ring: &mut IoUring, msg: &[u8]) -> Vec<u8> {
    let batch = if msg.len() >= HEAD + 16 { u64_at(msg, HEAD) } else { 0 };
    let fail = |code: i32| {
        let mut r = header(T_READ_OK);
        r.extend_from_slice(&batch.to_le_bytes());
        r.extend_from_slice(&code.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
        r
    };
    if msg.len() < HEAD + 16 {
        return fail(libc::EINVAL);
    }
    let n = u32_at(msg, HEAD + 8) as usize;
    if n == 0 || n > MAX_RANGES || msg.len() < HEAD + 16 + n * REQ_RANGE {
        return fail(libc::EINVAL);
    }
    let mut ranges = Vec::with_capacity(n);
    for i in 0..n {
        let at = HEAD + 16 + i * REQ_RANGE;
        let (offset, len, arena_off) = (u64_at(msg, at), u32_at(msg, at + 8) as u64, u64_at(msg, at + 16));
        if len == 0 || offset >= engine.size || arena_off % ALIGN != 0 {
            return fail(libc::EINVAL);
        }
        let end = (offset + len).min(engine.size);
        let a_start = offset / ALIGN * ALIGN;
        let a_end = end.div_ceil(ALIGN) * ALIGN;
        if arena_off.checked_add(a_end - a_start).is_none_or(|e| e > arena.len) {
            return fail(libc::EINVAL);
        }
        engine.note_read(offset, end);
        ranges.push((offset, end, a_start, a_end, arena_off));
    }
    // One plan for the batch (as prefetch does): an expert-major copy serves scattered
    // experts with one seek each; without one, each piece goes to the soonest drive.
    let runs: Vec<(u64, u64)> = ranges.iter().map(|r| (r.2, r.3)).collect();
    let places = engine.place(&runs);
    let mut pieces = Vec::new();
    let mut answers = Vec::with_capacity(n);
    for (i, &(offset, end, a_start, a_end, arena_off)) in ranges.iter().enumerate() {
        let mut first_source = 0u8;
        let mut covered = 0u64;
        for &(source, fd, src_off, m, len) in &places {
            let (p, q) = (m.max(a_start), (m + len).min(a_end));
            if p >= q {
                continue;
            }
            if p == a_start {
                first_source = source as u8;
            }
            covered += q - p;
            pieces.push(Piece { range: i, source, fd, src_off: src_off + (p - m), model_off: p, dst: arena_off + (p - a_start), span: q - p, need: q.min(end).saturating_sub(p) });
        }
        if covered != a_end - a_start {
            return fail(libc::EIO);
        }
        answers.push((arena_off + (offset - a_start), (end - offset) as u32, first_source));
    }
    // Each drive gets its reads in the order they lie on it.
    pieces.sort_by_key(|pc| (pc.source, pc.fd, pc.src_off));

    // Every piece in flight at once (up to IN_FLIGHT); a failed copy read is retried from
    // the original.
    let mut queue: Vec<usize> = (0..pieces.len()).rev().collect();
    let mut in_flight = 0usize;
    let mut status = 0i32;
    while !queue.is_empty() || in_flight > 0 {
        while in_flight < IN_FLIGHT {
            let Some(i) = queue.pop() else { break };
            let pc = &pieces[i];
            let entry = opcode::Read::new(types::Fd(pc.fd), unsafe { arena.ptr.add(pc.dst as usize) }, pc.span as u32).offset(pc.src_off).build().user_data(i as u64);
            // SAFETY: each piece writes its own region of the arena, which stays mapped
            // until this function has reaped every read.
            if unsafe { ring.submission().push(&entry) }.is_err() {
                queue.push(i);
                break;
            }
            in_flight += 1;
        }
        if let Err(e) = ring.submit_and_wait(1) {
            status = e.raw_os_error().unwrap_or(libc::EIO);
            if in_flight == 0 {
                break;
            }
            continue;
        }
        let done: Vec<(u64, i32)> = ring.completion().map(|c| (c.user_data(), c.result())).collect();
        for (ud, res) in done {
            in_flight -= 1;
            let i = ud as usize;
            let good = res >= 0 && res as u64 >= pieces[i].need;
            if good {
                engine.served(pieces[i].source, pieces[i].need);
            } else if pieces[i].source != 0 && status == 0 {
                let why = if res < 0 { io::Error::from_raw_os_error(-res).to_string() } else { "short read".into() };
                engine.copy_failed(pieces[i].source, &why);
                match engine.choose(pieces[i].model_off, pieces[i].span, true) {
                    Some((source, fd, src_off)) => {
                        let pc = &mut pieces[i];
                        pc.source = source;
                        pc.fd = fd;
                        pc.src_off = src_off;
                        answers[pc.range].2 = 0;
                        queue.push(i);
                    }
                    None => status = libc::EIO,
                }
            } else if status == 0 {
                status = if res < 0 { -res } else { libc::EIO };
            }
        }
    }
    if status != 0 {
        return fail(status);
    }
    let mut r = header(T_READ_OK);
    r.extend_from_slice(&batch.to_le_bytes());
    r.extend_from_slice(&0i32.to_le_bytes());
    r.extend_from_slice(&(n as u32).to_le_bytes());
    for (data_off, got, source) in answers {
        r.extend_from_slice(&data_off.to_le_bytes());
        r.extend_from_slice(&got.to_le_bytes());
        r.push(source);
        r.extend_from_slice(&[0, 0, 0]);
    }
    r
}

/// The daemon's own read-only view of its mounted file, used only to ask the kernel which
/// pages are already in the page cache (mincore reads nothing and never calls into FUSE).
pub struct Resident {
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: a read-only mapping that is only passed to mincore.
unsafe impl Send for Resident {}
unsafe impl Sync for Resident {}

impl Resident {
    pub fn open(path: &Path, len: u64) -> io::Result<Resident> {
        let file = std::fs::File::open(path)?;
        use std::os::fd::AsRawFd;
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len as usize, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Resident { ptr, len: len as usize })
    }

    /// The page-aligned runs of [start, end) that are not in the page cache.
    fn missing(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let end = end.min((self.len as u64).div_ceil(PAGE) * PAGE);
        if start >= end {
            return Vec::new();
        }
        let pages = (end - start).div_ceil(PAGE) as usize;
        let mut vec = vec![0u8; pages];
        let ok = unsafe { libc::mincore(self.ptr.add(start as usize), (end - start) as usize, vec.as_mut_ptr()) } == 0;
        if !ok {
            return vec![(start, end)];
        }
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for (i, v) in vec.iter().enumerate() {
            if v & 1 == 0 {
                let p = start + i as u64 * PAGE;
                match runs.last_mut() {
                    Some(r) if r.1 == p => r.1 = (p + PAGE).min(end),
                    _ => runs.push((p, (p + PAGE).min(end))),
                }
            }
        }
        runs
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

const PAGE: u64 = 4096;

/// Page ranges background guesses are reading right now. A demand read that overlaps one
/// waits for it to land instead of reading the same bytes a second time.
static GUESSING: std::sync::Mutex<Vec<(u64, u64)>> = std::sync::Mutex::new(Vec::new());
static IN_FLIGHT_DONE: std::sync::Condvar = std::sync::Condvar::new();
/// Guesses sleep on this while a demand read runs; the last demand read to finish wakes them.
static DEMAND_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static DEMAND_DONE: std::sync::Condvar = std::sync::Condvar::new();
/// Largest single store into the page cache.
const PIECE: u64 = 1 << 20;

/// Puts exactly the requested ranges into the kernel's page cache for the mounted file:
/// pages already there are skipped (mincore), the rest are read in pieces of up to 1 MiB
/// that never cross a copy chunk, each from the soonest drive, 8 at a time, and stored
/// without keeping a second copy in Corsia's own cache.
/// `background`: a guess (async PREFETCH). It pauses whenever reads the engine is waiting
/// for are in flight, and uses fewer threads, so a guess never delays real work.
fn prefetch_batch(engine: &Engine, notifier: Option<&fuser::Notifier>, msg: &[u8], background: bool) -> Vec<u8> {
    // Demand reads announce themselves for as long as they run; guesses wait for them.
    struct Demand<'a>(&'a Engine);
    impl Drop for Demand<'_> {
        fn drop(&mut self) {
            if self.0.demand_in_flight.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                let _g = DEMAND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                DEMAND_DONE.notify_all();
            }
        }
    }
    let _demand = (!background).then(|| {
        engine.demand_in_flight.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Demand(engine)
    });
    let batch = if msg.len() >= HEAD + 16 { u64_at(msg, HEAD) } else { 0 };
    let reply = |status: i32, pieces: u32, bytes: u64| {
        let mut r = header(T_PREFETCH_OK);
        r.extend_from_slice(&batch.to_le_bytes());
        r.extend_from_slice(&status.to_le_bytes());
        r.extend_from_slice(&pieces.to_le_bytes());
        r.extend_from_slice(&bytes.to_le_bytes());
        r
    };
    let Some(notifier) = notifier else { return reply(libc::ENOTSUP, 0, 0) };
    if msg.len() < HEAD + 16 {
        return reply(libc::EINVAL, 0, 0);
    }
    let n = u32_at(msg, HEAD + 8) as usize;
    if n == 0 || n > MAX_RANGES || msg.len() < HEAD + 16 + n * PRE_RANGE {
        return reply(libc::EINVAL, 0, 0);
    }
    // Wanted page runs, merged.
    let mut want: Vec<(u64, u64)> = Vec::with_capacity(n);
    for i in 0..n {
        let at = HEAD + 16 + i * PRE_RANGE;
        let (offset, len) = (u64_at(msg, at), u32_at(msg, at + 8) as u64);
        if len == 0 || offset >= engine.size {
            continue;
        }
        let end = (offset + len).min(engine.size);
        engine.note_read(offset, end);
        want.push((offset / PAGE * PAGE, end.div_ceil(PAGE) * PAGE));
    }
    want.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for r in want {
        match merged.last_mut() {
            Some(m) if r.0 <= m.1 => m.1 = m.1.max(r.1),
            _ => merged.push(r),
        }
    }
    // Drop what is already resident, then cut into pieces.
    let mut pieces: Vec<(u64, u64)> = Vec::new();
    let mut skipped = 0u64;
    let overlaps = |list: &[(u64, u64)], a: u64, b: u64| list.iter().any(|&(c, d)| a < d && c < b);
    let mut guard = RESIDENT.lock().unwrap_or_else(|e| e.into_inner());
    if guard.1.is_none() && !guard.2 {
        guard.1 = guard.0.as_deref().and_then(|f| Resident::open(f, engine.size).ok());
    }
    for (s0, e0) in merged {
        let runs = guard.1.as_ref().map_or_else(|| vec![(s0, e0)], |r| r.missing(s0, e0));
        let missing: u64 = runs.iter().map(|(a, b)| b - a).sum();
        skipped += (e0 - s0).saturating_sub(missing);
        for (mut p, e) in runs {
            while p < e {
                let q = (p + PIECE).min((p / CHUNK + 1) * CHUNK).min(e);
                pieces.push((p, q));
                p = q;
            }
        }
    }
    drop(guard);
    if !background && !pieces.is_empty() {
        // Wait for guesses already fetching these pages, then keep only what is still missing.
        let mut waited = false;
        let mut g = GUESSING.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while pieces.iter().any(|&(a, b)| overlaps(&g, a, b)) && std::time::Instant::now() < deadline {
            waited = true;
            g = IN_FLIGHT_DONE.wait_timeout(g, std::time::Duration::from_millis(50)).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
        }
        drop(g);
        if waited {
            let r = RESIDENT.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(res) = r.1.as_ref() {
                let before: u64 = pieces.iter().map(|(a, b)| b - a).sum();
                pieces.retain(|&(a, b)| !res.missing(a, b).is_empty());
                let after: u64 = pieces.iter().map(|(a, b)| b - a).sum();
                skipped += before - after;
                engine.prefetch_joined.fetch_add(before - after, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    engine.prefetch_skipped.fetch_add(skipped, std::sync::atomic::Ordering::Relaxed);

    use std::sync::atomic::Ordering::Relaxed;
    // One plan for the batch: an expert-major copy turns neighbouring experts into one read.
    let jobs = engine.plan(&pieces, false);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let stored = std::sync::atomic::AtomicU64::new(0);
    let failure = std::sync::atomic::AtomicI32::new(0);
    let workers = jobs.len().clamp(1, if background { 2 } else { 8 });
    std::thread::scope(|sc| {
        for _ in 0..workers {
            sc.spawn(|| {
                let mut buf = crate::bench::AlignedBuf::new(PIECE as usize);
                loop {
                    if background && engine.demand_in_flight.load(std::sync::atomic::Ordering::Acquire) > 0 {
                        // Sleep until demand reads finish (at most half a second), no wake-ups meanwhile.
                        let g = DEMAND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = DEMAND_DONE
                            .wait_timeout_while(g, std::time::Duration::from_millis(500), |_| {
                                engine.demand_in_flight.load(std::sync::atomic::Ordering::Acquire) > 0
                            })
                            .unwrap_or_else(|e| e.into_inner());
                    }
                    let i = next.fetch_add(1, Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    let ranges: Vec<(u64, u64)> = job.outs.iter().map(|o| (o.0, o.0 + o.1)).collect();
                    if background {
                        GUESSING.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&ranges);
                    }
                    let res = engine.exec(job, &mut buf, &mut |off, data| notifier.store(crate::fusefs::FILE, off, data));
                    if background {
                        let mut g = GUESSING.lock().unwrap_or_else(|e| e.into_inner());
                        for r in &ranges {
                            if let Some(at) = g.iter().position(|x| x == r) {
                                g.swap_remove(at);
                            }
                        }
                        drop(g);
                        IN_FLIGHT_DONE.notify_all();
                    }
                    match res {
                        Ok(valid) => {
                            stored.fetch_add(valid, Relaxed);
                        }
                        Err(e) => {
                            let _ = failure.compare_exchange(0, e.raw_os_error().unwrap_or(libc::EIO), Relaxed, Relaxed);
                        }
                    }
                }
            });
        }
    });
    engine.prefetched.fetch_add(stored.load(Relaxed), Relaxed);
    reply(failure.load(Relaxed), jobs.len() as u32, stored.load(Relaxed))
}

/// Receives one packet and, if one came with it, a file descriptor.
pub fn recv_msg(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, Option<RawFd>)> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut control = [0u64; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    let n = loop {
        let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break n;
        }
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut passed = None;
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                passed = Some(std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const RawFd));
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if msg.msg_flags & libc::MSG_TRUNC != 0 {
        if let Some(p) = passed {
            unsafe { libc::close(p) };
        }
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    Ok((n as usize, passed))
}

/// Sends one packet, optionally passing a file descriptor with it.
pub fn send_msg(fd: RawFd, data: &[u8], pass: Option<RawFd>) -> io::Result<()> {
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
    let mut control = [0u64; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(p) = pass {
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, p);
        }
    }
    loop {
        let n = unsafe { libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}
