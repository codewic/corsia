//! Speed copies of a model on an extra drive.
//!
//! Chunks are appended to 1 GiB pack files in the order they are copied, so a partial
//! copy (the hot chunks only) is natural and any filesystem works, FAT included. An index
//! file lists the chunk number of each slot. Data is synced before its index line, so a
//! drive pulled out mid-copy loses at most the chunk being written.
//!
//! ```text
//! DIR/.corsia/<model-name>-<bytes>/pack-000.bin   slots 0..255
//! DIR/.corsia/<model-name>-<bytes>/pack-001.bin   slots 256..511
//! DIR/.corsia/<model-name>-<bytes>/index          one chunk number per line, slot order
//! ```
//!
//! An expert-major copy (`--copy-layout experts`) lives in `<model-name>-<bytes>-experts`
//! with a `layout` file beside the index. Its chunks are chunks of the rearranged copy
//! (see layout.rs), filled in order, and the layout file says how to find an original
//! offset in it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::layout::{ExpertLayout, ExpertMap};

pub const CHUNK: u64 = 4 << 20;
const SLOTS_PER_PACK: u64 = 256;
const ABSENT: u32 = u32::MAX;

pub struct CopyStore {
    pub drive: String,
    pub dir: PathBuf,
    pub max_chunks: u64,
    /// Set for an expert-major copy: original offsets must go through it.
    pub layout: Option<Arc<ExpertLayout>>,
    inner: RwLock<Inner>,
}

struct Inner {
    /// chunk number -> slot, or ABSENT
    slot_of: Vec<u32>,
    /// slot -> chunk number
    order: Vec<u32>,
    /// Read handles per pack (O_DIRECT when the filesystem allows it).
    readers: Vec<File>,
    direct: bool,
}

impl CopyStore {
    pub fn open(drive: &str, base: &Path, model_name: &str, model_bytes: u64, max_chunks: u64) -> io::Result<CopyStore> {
        let dir = copy_dir(base, model_name, model_bytes, "");
        fs::create_dir_all(&dir)?;
        Self::open_in(drive, dir, model_bytes.div_ceil(CHUNK), max_chunks, None)
    }

    /// An expert-major copy. A copy already started keeps the layout it was written with
    /// (its `layout` file); a new one is laid out with `orders` (called only then).
    /// Returns the store and whether an existing layout was kept.
    pub fn open_experts(
        drive: &str,
        base: &Path,
        model_name: &str,
        map: &ExpertMap,
        max_chunks: u64,
        orders: impl FnOnce() -> Vec<Vec<u32>>,
    ) -> io::Result<(CopyStore, bool)> {
        let dir = copy_dir(base, model_name, map.file_size, "-experts");
        fs::create_dir_all(&dir)?;
        let file = dir.join("layout");
        let (layout, kept) = match fs::read_to_string(&file) {
            Ok(text) => {
                let lay = ExpertLayout::from_text(map, &text)
                    .map_err(|e| io::Error::other(format!("{} ({e}); delete that folder to start the copy again", dir.display())))?;
                (lay, true)
            }
            Err(_) => {
                let lay = ExpertLayout::new(map, orders()).map_err(io::Error::other)?;
                // No layout file means nothing in the packs can be trusted.
                let _ = fs::remove_file(dir.join("index"));
                let tmp = dir.join("layout.tmp");
                let mut f = File::create(&tmp)?;
                f.write_all(lay.to_text().as_bytes())?;
                f.sync_all()?;
                fs::rename(tmp, &file)?;
                (lay, false)
            }
        };
        let chunks = layout.space.div_ceil(CHUNK);
        Ok((Self::open_in(drive, dir, chunks, max_chunks, Some(Arc::new(layout)))?, kept))
    }

    fn open_in(drive: &str, dir: PathBuf, total_chunks: u64, max_chunks: u64, layout: Option<Arc<ExpertLayout>>) -> io::Result<CopyStore> {
        let chunks = total_chunks as usize;
        let mut slot_of = vec![ABSENT; chunks];
        let mut order = Vec::new();
        if let Ok(text) = fs::read_to_string(dir.join("index")) {
            for line in text.lines() {
                let Ok(c) = line.trim().parse::<u32>() else { break };
                let slot = order.len() as u64;
                // Trust a line only if its pack really holds the slot's bytes.
                let pack_len = fs::metadata(pack_path(&dir, slot / SLOTS_PER_PACK)).map(|m| m.len()).unwrap_or(0);
                if (c as usize) >= chunks || pack_len < (slot % SLOTS_PER_PACK + 1) * CHUNK || slot_of[c as usize] != ABSENT {
                    break;
                }
                slot_of[c as usize] = slot as u32;
                order.push(c);
            }
        }
        let mut store = CopyStore {
            drive: drive.to_string(),
            dir,
            max_chunks: max_chunks.min(chunks as u64),
            layout,
            inner: RwLock::new(Inner { slot_of, order, readers: Vec::new(), direct: true }),
        };
        store.rewrite_index()?;
        store.reopen_readers()?;
        Ok(store)
    }

    /// Drops index lines past what the packs hold (after a crash or a pulled drive).
    fn rewrite_index(&mut self) -> io::Result<()> {
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        let text: String = inner.order.iter().map(|c| format!("{c}\n")).collect();
        let tmp = self.dir.join("index.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        fs::rename(tmp, self.dir.join("index"))
    }

    fn reopen_readers(&mut self) -> io::Result<()> {
        let dir = self.dir.clone();
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        let packs = (inner.order.len() as u64).div_ceil(SLOTS_PER_PACK);
        inner.readers.clear();
        for p in 0..packs {
            let (f, direct) = open_reader(&pack_path(&dir, p))?;
            inner.direct &= direct;
            inner.readers.push(f);
        }
        Ok(())
    }

    pub fn present(&self) -> u64 {
        self.inner.read().map(|i| i.order.len() as u64).unwrap_or(0)
    }

    pub fn has(&self, chunk: u64) -> bool {
        self.inner.read().map(|i| i.slot_of.get(chunk as usize).is_some_and(|s| *s != ABSENT)).unwrap_or(false)
    }

    pub fn is_full(&self) -> bool {
        self.present() >= self.max_chunks
    }

    /// Where a chunk lives: (fd, byte offset of the chunk in that pack, direct reads ok).
    pub fn locate(&self, chunk: u64) -> Option<(i32, u64, bool)> {
        let inner = self.inner.read().ok()?;
        let slot = *inner.slot_of.get(chunk as usize)? as u64;
        if slot == ABSENT as u64 {
            return None;
        }
        let f = inner.readers.get((slot / SLOTS_PER_PACK) as usize)?;
        Some((f.as_raw_fd(), (slot % SLOTS_PER_PACK) * CHUNK, inner.direct))
    }

    /// Where copy bytes [c0, c1) live, as preads (fd, offset, length) that fill a buffer
    /// in order, joined where the chunks sit back to back in one pack. None if any chunk
    /// is not copied yet.
    pub fn spans(&self, c0: u64, c1: u64) -> Option<Vec<(i32, u64, u64)>> {
        let inner = self.inner.read().ok()?;
        let mut out: Vec<(i32, u64, u64)> = Vec::new();
        let mut p = c0;
        while p < c1 {
            let chunk = p / CHUNK;
            let slot = *inner.slot_of.get(chunk as usize)? as u64;
            if slot == ABSENT as u64 {
                return None;
            }
            let fd = inner.readers.get((slot / SLOTS_PER_PACK) as usize)?.as_raw_fd();
            let to = ((chunk + 1) * CHUNK).min(c1);
            let off = (slot % SLOTS_PER_PACK) * CHUNK + (p - chunk * CHUNK);
            match out.last_mut() {
                Some(last) if last.0 == fd && last.1 + last.2 == off => last.2 += to - p,
                _ => out.push((fd, off, to - p)),
            }
            p = to;
        }
        Some(out)
    }

    /// Whether every chunk of copy bytes [c0, c1) is copied.
    pub fn holds(&self, c0: u64, c1: u64) -> bool {
        let Ok(inner) = self.inner.read() else { return false };
        (c0 / CHUNK..c1.div_ceil(CHUNK)).all(|k| inner.slot_of.get(k as usize).is_some_and(|s| *s != ABSENT))
    }

    /// False when the copy's filesystem refused direct reads (ntfs-3g, for one).
    pub fn direct(&self) -> bool {
        self.inner.read().map(|i| i.direct).unwrap_or(false)
    }

    /// The first pack's read handle, for measuring the drive: (fd, bytes it holds).
    pub fn first_pack(&self) -> Option<(i32, u64)> {
        let inner = self.inner.read().ok()?;
        let f = inner.readers.first()?;
        Some((f.as_raw_fd(), (inner.order.len() as u64).min(SLOTS_PER_PACK) * CHUNK))
    }

    /// Appends one chunk (exactly CHUNK bytes, the last one zero-padded).
    pub fn append(&self, chunk: u64, data: &[u8]) -> io::Result<()> {
        debug_assert_eq!(data.len() as u64, CHUNK);
        let slot = self.present();
        let pack = slot / SLOTS_PER_PACK;
        let path = pack_path(&self.dir, pack);
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        let expected = (slot % SLOTS_PER_PACK) * CHUNK;
        if f.metadata()?.len() != expected {
            // A partial chunk from an interrupted copy: cut it off before appending.
            f.set_len(expected)?;
        }
        f.write_all(data)?;
        f.sync_data()?;
        unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        let mut idx = OpenOptions::new().create(true).append(true).open(self.dir.join("index"))?;
        idx.write_all(format!("{chunk}\n").as_bytes())?;
        idx.sync_data()?;

        let mut inner = self.inner.write().map_err(|_| io::Error::other("copy index lock poisoned"))?;
        if (pack as usize) >= inner.readers.len() {
            let (r, direct) = open_reader(&path)?;
            inner.direct &= direct;
            inner.readers.push(r);
        }
        inner.slot_of[chunk as usize] = slot as u32;
        inner.order.push(chunk as u32);
        Ok(())
    }
}

fn copy_dir(base: &Path, model_name: &str, model_bytes: u64, suffix: &str) -> PathBuf {
    let safe: String = model_name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect();
    base.join(".corsia").join(format!("{safe}-{model_bytes}{suffix}"))
}

fn pack_path(dir: &Path, pack: u64) -> PathBuf {
    dir.join(format!("pack-{pack:03}.bin"))
}

fn open_reader(path: &Path) -> io::Result<(File, bool)> {
    match OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(path) {
        Ok(f) => Ok((f, true)),
        Err(_) => Ok((File::open(path)?, false)),
    }
}
