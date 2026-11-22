//! Shared memory layout and wire protocol.
//!
//! Everything here is mirrored by the guest driver, so it is kept to
//! `repr(C)` structs of fixed-width little-endian integers and byte
//! arrays, with no std types in the layouts.

use core::mem::size_of;

/// "SHAREDFS" as a little-endian u64.
pub const MAGIC: u64 = u64::from_le_bytes(*b"SHAREDFS");
pub const FORMAT_VERSION: u32 = 1;

/// Linux NAME_MAX. Names are stored inline, so no arena is needed.
pub const NAME_MAX: usize = 255;
pub const HEADER_SIZE: usize = 4096;
pub const ENTRY_SIZE: usize = 384;

/// Slot 0 is always the root directory.
pub const ROOT_SLOT: u32 = 0;
/// Inode number of slot `s` is `s + 1`, so the root is inode 1.
/// The root's inode number: slot 0 at generation 1, which it keeps for the life of the index.
pub const ROOT_INO: u64 = ino_of(ROOT_SLOT, 1);
/// Nil link in the child list, hash chain and table.
pub const NO_SLOT: u32 = u32::MAX;

pub const ENTRY_LIVE: u32 = 1;

/// Mapping granularity: the guest maps and unmaps whole chunks of a file.
pub const CHUNK: u64 = 2 << 20;

#[inline]
/// An inode number names an index slot and the low half of the generation the entry had when
/// the guest met it. A slot reused for another file therefore has a new number: the guest's
/// cached inode stays distinct from the new one, and a request carrying the old number is
/// refused with `ESTALE` instead of reaching the wrong file.
pub const fn ino_of(slot: u32, generation: u64) -> u64 {
    ((generation as u32 as u64) << 32) | (slot as u64 + 1)
}

#[inline]
pub fn slot_of(ino: u64) -> Option<u32> {
    (ino as u32).checked_sub(1)
}

#[inline]
pub fn generation_of(ino: u64) -> u32 {
    (ino >> 32) as u32
}

/// First page of the region.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub magic: u64,
    pub version: u32,
    pub flags: u32,
    pub entry_cap: u32,
    /// Power of two.
    pub table_cap: u32,
    pub entries_off: u64,
    pub table_off: u64,
    pub region_len: u64,
    /// Live entries, informational.
    pub live: u32,
    pub _pad: u32,
}

/// One file, directory or symlink. 384 bytes.
///
/// `seq` is a seqlock: odd while the daemon writes, even when stable.
/// Readers copy the entry, re-read `seq`, and retry on change or odd.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Entry {
    pub seq: u32,
    pub flags: u32,
    /// Incremented each time this slot is reused.
    pub generation: u64,
    pub parent: u32,
    pub hash_next: u32,
    pub first_child: u32,
    pub next_sibling: u32,
    pub prev_sibling: u32,
    pub nlink: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub name_len: u16,
    pub _pad: u16,
    pub size: u64,
    pub rdev: u64,
    /// Nanoseconds since the epoch.
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub name: [u8; 256],
    /// Which host file the entry was made for: device and inode, zero when not recorded (an
    /// index written before these fields existed). The daemon's alone; the guest's copy of
    /// this layout keeps the bytes reserved.
    pub host_dev: u64,
    pub host_ino: u64,
    pub reserved: [u8; 16],
}

const _: () = assert!(size_of::<Entry>() == ENTRY_SIZE);
const _: () = assert!(size_of::<Header>() <= HEADER_SIZE);

impl Entry {
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
    pub fn is_live(&self) -> bool {
        self.flags & ENTRY_LIVE != 0
    }
}

/// FNV-1a over the parent slot and the name. The guest computes the same.
pub fn name_hash(parent: u32, name: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in parent.to_le_bytes().iter().chain(name) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------
// Queue protocol
// ---------------------------------------------------------------------

pub mod op {
    pub const FSYNC: u32 = 5;
    pub const CREATE: u32 = 6;
    pub const MKDIR: u32 = 7;
    pub const SYMLINK: u32 = 8;
    pub const UNLINK: u32 = 9;
    pub const RMDIR: u32 = 10;
    pub const RENAME: u32 = 11;
    pub const SETATTR: u32 = 12;
    pub const READLINK: u32 = 13;
    pub const STATFS: u32 = 14;
    pub const MAP: u32 = 15;
    pub const UNMAP: u32 = 16;
    pub const EXTEND: u32 = 17;
    pub const OPEN_WRITE: u32 = 18;
    pub const RELEASE_WRITE: u32 = 19;
    pub const OPEN_READ: u32 = 20;
    pub const RELEASE_READ: u32 = 21;
    pub const LINK: u32 = 22;
}

/// MAP `flags` bits.
pub mod map {
    /// Map for writing as well as reading.
    pub const WRITE: u32 = 1 << 0;
}

/// SETATTR `valid` bits.
pub mod setattr {
    pub const MODE: u32 = 1 << 0;
    pub const UID: u32 = 1 << 1;
    pub const GID: u32 = 1 << 2;
    pub const SIZE: u32 = 1 << 3;
    pub const ATIME: u32 = 1 << 4;
    pub const MTIME: u32 = 1 << 5;
}

// ---------------------------------------------------------------------
// Notification queue: daemon to guest
// ---------------------------------------------------------------------

/// Notification kinds.
pub mod notify {
    /// Something happened to an entry; `mask` says what.
    pub const EVENT: u32 = 1;
    /// Notifications were lost. The guest drops what it cached and re-reads the index.
    pub const OVERFLOW: u32 = 2;
}

/// Event mask bits, numerically equal to Linux `IN_*` and `FS_*` so the
/// guest can hand them to fsnotify as they are.
pub mod mask {
    pub const MODIFY: u32 = 0x0000_0002;
    pub const ATTRIB: u32 = 0x0000_0004;
    pub const CLOSE_WRITE: u32 = 0x0000_0008;
    pub const MOVED_FROM: u32 = 0x0000_0040;
    pub const MOVED_TO: u32 = 0x0000_0080;
    pub const CREATE: u32 = 0x0000_0100;
    pub const DELETE: u32 = 0x0000_0200;
    pub const ISDIR: u32 = 0x4000_0000;
}

/// Size of each buffer the guest posts on the notification queue. Fits any
/// notification: 32 bytes of fields plus a `NAME_MAX` name.
pub const NOTIFY_BUF: usize = 320;

/// One notification. On the wire: `len: u32`, `kind: u32`, then for EVENT
/// `mask: u32`, `slot: u32`, `parent: u32`, `cookie: u32`, `generation: u64`,
/// `name: u16 len + bytes`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notification {
    Event { mask: u32, slot: u32, parent: u32, cookie: u32, generation: u64, name: Vec<u8> },
    Overflow,
}

impl Notification {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Out(Vec::with_capacity(64));
        o.u32(0);
        match self {
            Notification::Overflow => o.u32(notify::OVERFLOW),
            Notification::Event { mask, slot, parent, cookie, generation, name } => {
                o.u32(notify::EVENT);
                o.u32(*mask);
                o.u32(*slot);
                o.u32(*parent);
                o.u32(*cookie);
                o.u64(*generation);
                o.bytes(name);
            }
        }
        o.finish()
    }

    pub fn decode(b: &[u8]) -> Result<Notification, Malformed> {
        let mut c = Cursor { b, at: 0 };
        let len = c.u32()? as usize;
        if len > b.len() || len < 8 {
            return Err(Malformed);
        }
        let mut c = Cursor { b: &b[..len], at: 4 };
        let n = match c.u32()? {
            notify::OVERFLOW => Notification::Overflow,
            notify::EVENT => Notification::Event {
                mask: c.u32()?,
                slot: c.u32()?,
                parent: c.u32()?,
                cookie: c.u32()?,
                generation: c.u64()?,
                name: c.bytes()?,
            },
            _ => return Err(Malformed),
        };
        if c.at != len {
            return Err(Malformed);
        }
        Ok(n)
    }
}

/// Request header on the wire: `len` covers header and body.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ReqHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
}

/// Response header on the wire. `error` is a positive errno or 0.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RespHeader {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Place `len` bytes of the file at `foff` at window offset `woff`.
    Map { ino: u64, generation: u64, foff: u64, len: u64, woff: u64, flags: u32 },
    /// Give a window range back.
    Unmap { woff: u64, len: u64 },
    /// Grow the file to `size` so the guest can write up to there through the window.
    Extend { ino: u64, size: u64 },
    Fsync { ino: u64 },
    /// The guest opened the file for writing and nobody had it open for writing before. The
    /// daemon opens it once and keeps the handle, so writes keep the access granted at open.
    OpenWrite { ino: u64 },
    /// The last guest writer closed the file; the handle goes.
    ReleaseWrite { ino: u64 },
    /// The guest opened the file for reading (an open file includes one still mapped) and
    /// nobody had it open for reading before. The handle keeps the file reachable after an
    /// unlink, as an open descriptor does on any file system.
    OpenRead { ino: u64 },
    /// The last guest reader closed the file.
    ReleaseRead { ino: u64 },
    Create { parent: u64, mode: u32, flags: u32, name: Vec<u8> },
    Mkdir { parent: u64, mode: u32, name: Vec<u8> },
    Symlink { parent: u64, name: Vec<u8>, target: Vec<u8> },
    Unlink { parent: u64, name: Vec<u8> },
    /// A further name for the file `ino`: `name` in `parent`. The new name is an entry of its
    /// own in the index, as a hard link made on the host is.
    Link { ino: u64, parent: u64, name: Vec<u8> },
    Rmdir { parent: u64, name: Vec<u8> },
    Rename { parent: u64, name: Vec<u8>, new_parent: u64, new_name: Vec<u8>, flags: u32 },
    Setattr { ino: u64, valid: u32, mode: u32, uid: u32, gid: u32, size: u64, atime: i64, mtime: i64 },
    Readlink { ino: u64 },
    Statfs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Empty,
    Node { ino: u64, generation: u64 },
    Readlink { target: Vec<u8> },
    Statfs { bsize: u64, blocks: u64, bfree: u64, bavail: u64, files: u64, ffree: u64, namelen: u32 },
}

/// Decode error: malformed bytes. Maps to EINVAL at the transport.
#[derive(Debug)]
pub struct Malformed;

struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        let s = self.b.get(self.at..self.at + n).ok_or(Malformed)?;
        self.at += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, Malformed> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, Malformed> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, Malformed> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<Vec<u8>, Malformed> {
        let n = self.u16()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn blob(&mut self) -> Result<Vec<u8>, Malformed> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
}

struct Out(Vec<u8>);

impl Out {
    fn u16(&mut self, v: u16) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn i64(&mut self, v: i64) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn bytes(&mut self, v: &[u8]) { self.u16(v.len() as u16); self.0.extend_from_slice(v); }
    fn blob(&mut self, v: &[u8]) { self.u32(v.len() as u32); self.0.extend_from_slice(v); }
    fn finish(mut self) -> Vec<u8> {
        let len = self.0.len() as u32;
        self.0[..4].copy_from_slice(&len.to_le_bytes());
        self.0
    }
}

impl Request {
    pub fn opcode(&self) -> u32 {
        use Request::*;
        match self {
            Map { .. } => op::MAP,
            Unmap { .. } => op::UNMAP,
            Extend { .. } => op::EXTEND,
            Fsync { .. } => op::FSYNC,
            OpenWrite { .. } => op::OPEN_WRITE,
            ReleaseWrite { .. } => op::RELEASE_WRITE,
            OpenRead { .. } => op::OPEN_READ,
            ReleaseRead { .. } => op::RELEASE_READ,
            Create { .. } => op::CREATE,
            Mkdir { .. } => op::MKDIR,
            Symlink { .. } => op::SYMLINK,
            Unlink { .. } => op::UNLINK,
            Link { .. } => op::LINK,
            Rmdir { .. } => op::RMDIR,
            Rename { .. } => op::RENAME,
            Setattr { .. } => op::SETATTR,
            Readlink { .. } => op::READLINK,
            Statfs => op::STATFS,
        }
    }

    /// Wire form: [`ReqHeader`] then the body.
    pub fn encode(&self, unique: u64) -> Vec<u8> {
        use Request::*;
        let mut o = Out(Vec::with_capacity(64));
        o.u32(0);
        o.u32(self.opcode());
        o.u64(unique);
        match self {
            Map { ino, generation, foff, len, woff, flags } => {
                o.u64(*ino); o.u64(*generation); o.u64(*foff); o.u64(*len); o.u64(*woff); o.u32(*flags);
            }
            Unmap { woff, len } => { o.u64(*woff); o.u64(*len); }
            Extend { ino, size } => { o.u64(*ino); o.u64(*size); }
            Fsync { ino } | OpenWrite { ino } | ReleaseWrite { ino } | OpenRead { ino } | ReleaseRead { ino } => o.u64(*ino),
            Create { parent, mode, flags, name } => { o.u64(*parent); o.u32(*mode); o.u32(*flags); o.bytes(name); }
            Mkdir { parent, mode, name } => { o.u64(*parent); o.u32(*mode); o.bytes(name); }
            Symlink { parent, name, target } => { o.u64(*parent); o.bytes(name); o.blob(target); }
            Unlink { parent, name } | Rmdir { parent, name } => { o.u64(*parent); o.bytes(name); }
            Link { ino, parent, name } => { o.u64(*ino); o.u64(*parent); o.bytes(name); }
            Rename { parent, name, new_parent, new_name, flags } => {
                o.u64(*parent); o.bytes(name); o.u64(*new_parent); o.bytes(new_name); o.u32(*flags);
            }
            Setattr { ino, valid, mode, uid, gid, size, atime, mtime } => {
                o.u64(*ino); o.u32(*valid); o.u32(*mode); o.u32(*uid); o.u32(*gid); o.u64(*size); o.i64(*atime); o.i64(*mtime);
            }
            Readlink { ino } => o.u64(*ino),
            Statfs => {}
        }
        o.finish()
    }

    /// Parse one request. Returns the `unique` tag with it.
    pub fn decode(b: &[u8]) -> Result<(u64, Request), Malformed> {
        use Request::*;
        let mut c = Cursor { b, at: 0 };
        let len = c.u32()? as usize;
        if len != b.len() { return Err(Malformed); }
        let opcode = c.u32()?;
        let unique = c.u64()?;
        let r = match opcode {
            op::MAP => Map { ino: c.u64()?, generation: c.u64()?, foff: c.u64()?, len: c.u64()?, woff: c.u64()?, flags: c.u32()? },
            op::UNMAP => Unmap { woff: c.u64()?, len: c.u64()? },
            op::EXTEND => Extend { ino: c.u64()?, size: c.u64()? },
            op::FSYNC => Fsync { ino: c.u64()? },
            op::OPEN_WRITE => OpenWrite { ino: c.u64()? },
            op::RELEASE_WRITE => ReleaseWrite { ino: c.u64()? },
            op::OPEN_READ => OpenRead { ino: c.u64()? },
            op::RELEASE_READ => ReleaseRead { ino: c.u64()? },
            op::CREATE => Create { parent: c.u64()?, mode: c.u32()?, flags: c.u32()?, name: c.bytes()? },
            op::MKDIR => Mkdir { parent: c.u64()?, mode: c.u32()?, name: c.bytes()? },
            op::SYMLINK => Symlink { parent: c.u64()?, name: c.bytes()?, target: c.blob()? },
            op::UNLINK => Unlink { parent: c.u64()?, name: c.bytes()? },
            op::LINK => Link { ino: c.u64()?, parent: c.u64()?, name: c.bytes()? },
            op::RMDIR => Rmdir { parent: c.u64()?, name: c.bytes()? },
            op::RENAME => Rename { parent: c.u64()?, name: c.bytes()?, new_parent: c.u64()?, new_name: c.bytes()?, flags: c.u32()? },
            op::SETATTR => Setattr {
                ino: c.u64()?, valid: c.u32()?, mode: c.u32()?, uid: c.u32()?, gid: c.u32()?, size: c.u64()?, atime: c.i64()?, mtime: c.i64()?,
            },
            op::READLINK => Readlink { ino: c.u64()? },
            op::STATFS => Statfs,
            _ => return Err(Malformed),
        };
        if c.at != b.len() { return Err(Malformed); }
        Ok((unique, r))
    }
}

impl Response {
    /// Wire form: [`RespHeader`] then the body. `Err(errno)` carries no body.
    pub fn encode(unique: u64, r: &Result<Response, i32>) -> Vec<u8> {
        use Response::*;
        let mut o = Out(Vec::with_capacity(32));
        o.u32(0);
        match r {
            Err(e) => { o.u32(*e as u32); o.u64(unique); }
            Ok(resp) => {
                o.u32(0);
                o.u64(unique);
                match resp {
                    Empty => {}
                    Node { ino, generation } => { o.u64(*ino); o.u64(*generation); }
                    Readlink { target } => o.blob(target),
                    Statfs { bsize, blocks, bfree, bavail, files, ffree, namelen } => {
                        o.u64(*bsize); o.u64(*blocks); o.u64(*bfree); o.u64(*bavail); o.u64(*files); o.u64(*ffree); o.u32(*namelen);
                    }
                }
            }
        }
        o.finish()
    }

    /// Parse a response for a known request opcode.
    pub fn decode(opcode: u32, b: &[u8]) -> Result<(u64, Result<Response, i32>), Malformed> {
        use Response::*;
        let mut c = Cursor { b, at: 0 };
        let len = c.u32()? as usize;
        if len != b.len() { return Err(Malformed); }
        let error = c.u32()? as i32;
        let unique = c.u64()?;
        if error != 0 {
            return Ok((unique, Err(error)));
        }
        let r = match opcode {
            op::MAP | op::UNMAP | op::EXTEND | op::FSYNC | op::UNLINK | op::RMDIR | op::RENAME | op::SETATTR
            | op::OPEN_WRITE | op::RELEASE_WRITE | op::OPEN_READ | op::RELEASE_READ => Empty,
            op::CREATE | op::MKDIR | op::SYMLINK | op::LINK => Node { ino: c.u64()?, generation: c.u64()? },
            op::READLINK => Readlink { target: c.blob()? },
            op::STATFS => Statfs {
                bsize: c.u64()?, blocks: c.u64()?, bfree: c.u64()?, bavail: c.u64()?, files: c.u64()?, ffree: c.u64()?, namelen: c.u32()?,
            },
            _ => return Err(Malformed),
        };
        if c.at != b.len() { return Err(Malformed); }
        Ok((unique, Ok(r)))
    }
}
