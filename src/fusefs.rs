//! Blind mode: the model appears as an ordinary read-only file in a folder, so any engine
//! can open or mmap it. Every read goes through the engine.

use std::ffi::OsStr;
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, KernelConfig, LockOwner, OpenFlags, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request,
};

use crate::engine::Engine;

const TTL: Duration = Duration::from_secs(60);
/// The model file's inode: the one page-cache pushes target.
pub const FILE: INodeNo = INodeNo(2);

pub struct ModelFs {
    engine: Arc<Engine>,
    uid: u32,
    gid: u32,
    mtime: SystemTime,
}

impl ModelFs {
    pub fn new(engine: Arc<Engine>) -> ModelFs {
        let mtime = std::fs::metadata(&engine.path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        ModelFs { engine, uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() }, mtime }
    }

    fn attr(&self, ino: INodeNo) -> Option<FileAttr> {
        let (kind, perm, size, nlink) = match ino {
            INodeNo::ROOT => (FileType::Directory, 0o555, 0, 2),
            FILE => (FileType::RegularFile, 0o444, self.engine.size, 1),
            _ => return None,
        };
        Some(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: self.mtime,
            mtime: self.mtime,
            ctime: self.mtime,
            crtime: self.mtime,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
}

impl Filesystem for ModelFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // The kernel caps readahead at its own setting (usually 128 KiB); asking for more is
        // harmless. More requests in flight let readers of different chunks run in parallel.
        let _ = config.set_max_readahead(4 << 20);
        let _ = config.set_max_background(64);
        let _ = config.set_congestion_threshold(48);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if parent == INodeNo::ROOT && name == OsStr::new(&self.engine.name) {
            if let Some(a) = self.attr(FILE) {
                return reply.entry(&TTL, &a, Generation(0));
            }
        }
        reply.error(Errno::ENOENT);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attr(ino) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, _kill_suid_gid: bool, reply: ReplyOpen) {
        if ino == FILE {
            // The model never changes while mounted, so the kernel may keep what it cached.
            reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
        } else {
            reply.error(Errno::EISDIR);
        }
    }

    fn read(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock: Option<LockOwner>, reply: ReplyData) {
        if ino != FILE {
            return reply.error(Errno::EISDIR);
        }
        match self.engine.read(offset, size) {
            Ok(data) => reply.data(&data),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        if ino != INodeNo::ROOT {
            return reply.error(Errno::ENOTDIR);
        }
        let entries = [(INodeNo::ROOT, FileType::Directory, "."), (INodeNo::ROOT, FileType::Directory, ".."), (FILE, FileType::RegularFile, self.engine.name.as_str())];
        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*ino, (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }
}
