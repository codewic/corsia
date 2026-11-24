//! A hinted-mode client: what an engine does to read the model through Corsia, and the
//! reference for the llama.cpp patch. Also `corsia hint-bench`.

use std::fs::File;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use crate::hint::{self, HEAD, MAX_MSG, REP_RANGE, T_HELLO, T_HELLO_OK, T_PREFETCH, T_PREFETCH_OK, T_READ, T_READ_OK};

pub struct Client {
    fd: RawFd,
    arena: *mut u8,
    arena_len: u64,
    pub model_bytes: u64,
    batch: u64,
    buf: Vec<u8>,
}

// SAFETY: the client owns its socket and mapping; one thread uses a client at a time.
unsafe impl Send for Client {}

/// Where one range landed in the arena.
#[derive(Clone, Copy, Debug)]
pub struct Landed {
    pub data_off: u64,
    pub got: u32,
    pub source: u8,
}

impl Client {
    pub fn connect(socket: &Path, arena_len: u64) -> io::Result<Client> {
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let close_err = |fd: RawFd| {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            e
        };
        let (addr, len) = hint::sockaddr(socket)?;
        if unsafe { libc::connect(fd, (&addr as *const libc::sockaddr_un).cast(), len) } != 0 {
            return Err(close_err(fd));
        }
        let mfd = unsafe { libc::memfd_create(c"corsia-arena".as_ptr(), libc::MFD_CLOEXEC) };
        if mfd < 0 {
            return Err(close_err(fd));
        }
        if unsafe { libc::ftruncate(mfd, arena_len as libc::off_t) } != 0 {
            unsafe { libc::close(mfd) };
            return Err(close_err(fd));
        }
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), arena_len as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, mfd, 0) };
        if ptr == libc::MAP_FAILED {
            unsafe { libc::close(mfd) };
            return Err(close_err(fd));
        }
        let mut hello = hint::header(T_HELLO);
        hello.extend_from_slice(&arena_len.to_le_bytes());
        let sent = hint::send_msg(fd, &hello, Some(mfd));
        unsafe { libc::close(mfd) };
        let mut c = Client { fd, arena: ptr.cast(), arena_len, model_bytes: 0, batch: 0, buf: vec![0; MAX_MSG] };
        sent?;
        let (n, _) = hint::recv_msg(fd, &mut c.buf)?;
        match hint::parse_header(&c.buf[..n])? {
            T_HELLO_OK if n >= HEAD + 8 => c.model_bytes = hint::u64_at(&c.buf, HEAD),
            _ => return Err(server_error(&c.buf[..n])),
        }
        Ok(c)
    }

    /// Reads a batch of (offset, len, arena_off) ranges; Corsia spreads them over the drives
    /// at once and returns when all have landed.
    pub fn read(&mut self, ranges: &[(u64, u32, u64)]) -> io::Result<Vec<Landed>> {
        self.batch += 1;
        let mut m = hint::header(T_READ);
        m.extend_from_slice(&self.batch.to_le_bytes());
        m.extend_from_slice(&(ranges.len() as u32).to_le_bytes());
        m.extend_from_slice(&0u32.to_le_bytes());
        for &(off, len, arena_off) in ranges {
            m.extend_from_slice(&off.to_le_bytes());
            m.extend_from_slice(&len.to_le_bytes());
            m.extend_from_slice(&0u32.to_le_bytes());
            m.extend_from_slice(&arena_off.to_le_bytes());
        }
        hint::send_msg(self.fd, &m, None)?;
        let (n, _) = hint::recv_msg(self.fd, &mut self.buf)?;
        let b = &self.buf[..n];
        if hint::parse_header(b)? != T_READ_OK || n < HEAD + 16 {
            return Err(server_error(b));
        }
        let status = i32::from_le_bytes(b[HEAD + 8..HEAD + 12].try_into().unwrap());
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        let count = hint::u32_at(b, HEAD + 12) as usize;
        if count != ranges.len() || n < HEAD + 16 + count * REP_RANGE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "reply does not match the request"));
        }
        Ok((0..count)
            .map(|i| {
                let at = HEAD + 16 + i * REP_RANGE;
                Landed { data_off: hint::u64_at(b, at), got: hint::u32_at(b, at + 8), source: b[at + 12] }
            })
            .collect())
    }

    /// Asks Corsia to put these (offset, len) ranges into the page cache of its mounted
    /// file; returns (blocks, bytes stored) once they are resident.
    pub fn prefetch(&mut self, ranges: &[(u64, u32)]) -> io::Result<(u32, u64)> {
        self.send_prefetch(ranges, 0)?;
        let (n, _) = hint::recv_msg(self.fd, &mut self.buf)?;
        let b = &self.buf[..n];
        if hint::parse_header(b)? != T_PREFETCH_OK || n < HEAD + 24 {
            return Err(server_error(b));
        }
        let status = i32::from_le_bytes(b[HEAD + 8..HEAD + 12].try_into().unwrap());
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        Ok((hint::u32_at(b, HEAD + 12), hint::u64_at(b, HEAD + 16)))
    }

    /// Like prefetch, but returns at once: Corsia works in the background and sends no reply.
    pub fn prefetch_async(&mut self, ranges: &[(u64, u32)]) -> io::Result<()> {
        self.send_prefetch(ranges, hint::PREFETCH_ASYNC)
    }

    fn send_prefetch(&mut self, ranges: &[(u64, u32)], flags: u32) -> io::Result<()> {
        self.batch += 1;
        let mut m = hint::header(T_PREFETCH);
        m.extend_from_slice(&self.batch.to_le_bytes());
        m.extend_from_slice(&(ranges.len() as u32).to_le_bytes());
        m.extend_from_slice(&flags.to_le_bytes());
        for &(off, len) in ranges {
            m.extend_from_slice(&off.to_le_bytes());
            m.extend_from_slice(&len.to_le_bytes());
            m.extend_from_slice(&0u32.to_le_bytes());
        }
        hint::send_msg(self.fd, &m, None)
    }

    pub fn slice(&self, l: Landed) -> &[u8] {
        assert!(l.data_off + l.got as u64 <= self.arena_len);
        // SAFETY: inside the arena, which lives as long as self.
        unsafe { std::slice::from_raw_parts(self.arena.add(l.data_off as usize), l.got as usize) }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.arena.cast(), self.arena_len as usize);
            libc::close(self.fd);
        }
    }
}

fn server_error(b: &[u8]) -> io::Error {
    if b.len() >= HEAD + 4 && hint::parse_header(b).ok() == Some(hint::T_ERROR) {
        let msg = String::from_utf8_lossy(&b[HEAD + 4..]).to_string();
        return io::Error::other(format!("Corsia: {msg}"));
    }
    io::Error::new(io::ErrorKind::InvalidData, "unexpected reply from Corsia")
}

pub struct BenchResult {
    pub mb_s: f64,
    pub bytes: u64,
    pub secs: f64,
    pub ranges: u64,
    pub mismatches: u64,
    /// Ranges whose first piece came from a copy rather than the original.
    pub from_copies: u64,
}

/// Reads every 4 MiB range once, in random order, `batch` ranges per request, with
/// `threads` clients. With `verify`, ranges get odd offsets and lengths and each one is
/// compared with the same bytes read straight from the original file.
pub fn bench(socket: &Path, threads: usize, batch: usize, verify: Option<&Path>) -> io::Result<BenchResult> {
    const BLOCK: u64 = 4 << 20;
    const SLOT: u64 = BLOCK + 8192;
    let probe = Client::connect(socket, 4096)?;
    let size = probe.model_bytes;
    drop(probe);
    let mut order: Vec<u64> = (0..size.div_ceil(BLOCK)).collect();
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    for i in (1..order.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        order.swap(i, (s % (i as u64 + 1)) as usize);
    }
    let queue = Mutex::new(order);
    let original = verify.map(File::open).transpose()?;
    let t0 = Instant::now();
    let results: Vec<io::Result<(u64, u64, u64, u64)>> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..threads.max(1))
            .map(|_| {
                let queue = &queue;
                let original = original.as_ref();
                sc.spawn(move || -> io::Result<(u64, u64, u64, u64)> {
                    let mut c = Client::connect(socket, SLOT * batch as u64)?;
                    let (mut bytes, mut ranges, mut bad, mut copies) = (0u64, 0u64, 0u64, 0u64);
                    let mut check = Vec::new();
                    loop {
                        let blocks: Vec<u64> = {
                            let mut q = queue.lock().map_err(|_| io::Error::other("queue lock"))?;
                            let take = q.len().min(batch);
                            let at = q.len() - take;
                            q.split_off(at)
                        };
                        if blocks.is_empty() {
                            break;
                        }
                        let req: Vec<(u64, u32, u64)> = blocks
                            .iter()
                            .enumerate()
                            .map(|(i, b)| {
                                let (mut off, mut len) = (b * BLOCK, BLOCK.min(size - b * BLOCK));
                                if original.is_some() {
                                    // Odd offsets and lengths exercise the alignment handling.
                                    let skew = (b * 7919) % 4096;
                                    off += skew.min(len - 1);
                                    len = (len - skew.min(len - 1)).saturating_sub((b * 104_729) % 8192).max(1);
                                }
                                (off, len as u32, i as u64 * SLOT)
                            })
                            .collect();
                        let landed = c.read(&req)?;
                        for (r, l) in req.iter().zip(&landed) {
                            bytes += l.got as u64;
                            ranges += 1;
                            copies += (l.source != 0) as u64;
                            if let Some(f) = original {
                                check.resize(r.1 as usize, 0);
                                let n = f.read_at(&mut check, r.0)?;
                                if n != l.got as usize || c.slice(*l) != &check[..n] {
                                    bad += 1;
                                }
                            }
                        }
                    }
                    Ok((bytes, ranges, bad, copies))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(io::Error::other("bench thread panicked")))).collect()
    });
    let secs = t0.elapsed().as_secs_f64();
    let (mut bytes, mut ranges, mut mismatches, mut from_copies) = (0, 0, 0, 0);
    for r in results {
        let (b, n, m, c) = r?;
        bytes += b;
        ranges += n;
        mismatches += m;
        from_copies += c;
    }
    Ok(BenchResult { mb_s: bytes as f64 / secs / 1e6, bytes, secs, ranges, mismatches, from_copies })
}
