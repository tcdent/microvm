//! The shared metadata index and the daemon's single-writer operations.
//!
//! The region is a memfd so the VMM can map the same pages into the
//! guest read-only. All mutation goes through [`Index::write`], which
//! brackets the change in the entry's seqlock. Links are published only
//! after the entry they point at is complete, so a reader that reaches
//! an entry through the table or a child list always finds it whole.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::num::NonZeroUsize;
use std::os::unix::fs::OpenOptionsExt;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering, fence};

use nix::sys::memfd::{MFdFlags, memfd_create};
use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap};
use nix::unistd::ftruncate;

use crate::proto::*;

/// POSIX attributes as the index stores them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub rdev: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    /// The host file's device and inode. Recorded when an entry is made and kept for its
    /// lifetime, so a name that comes to hold a different file can be told from one that
    /// changed in place.
    pub dev: u64,
    pub ino: u64,
}

pub struct Index {
    fd: OwnedFd,
    base: NonNull<u8>,
    len: usize,
    entry_cap: u32,
    table_cap: u32,
    /// Slots freed by `remove`, reused before untouched ones.
    free: Vec<u32>,
    next_unused: u32,
    live: u32,
    /// Live slots by the host file they were made for, so a hard link's other names are found
    /// without a scan. Kept in memory only; rebuilt from the entries when an index is adopted.
    names: HashMap<(u64, u64), Vec<u32>>,
}

// The region is only ever written from the thread that owns the Index.
unsafe impl Send for Index {}

fn errno(e: nix::Error) -> i32 {
    e as i32
}

impl Index {
    /// Allocate an anonymous region of `len` bytes and install the root directory.
    pub fn create(len: usize, root: Attr) -> Result<Index, i32> {
        let fd = memfd_create(c"sharedfs-index", MFdFlags::MFD_CLOEXEC).map_err(errno)?;
        ftruncate(&fd, len as libc::off_t).map_err(errno)?;
        let mut ix = Self::map(fd, len)?;
        ix.init(root)?;
        Ok(ix)
    }

    /// Open the index kept in the file at `path`, or create it. Returns whether an earlier
    /// index was adopted: its entries keep their slots and generations, and the caller
    /// reconciles them with the directory. A file of another size, or one whose header or
    /// entries are not consistent, is started afresh.
    pub fn open_file(path: &Path, len: usize, root: Attr) -> Result<(Index, bool), i32> {
        let io = |e: std::io::Error| e.raw_os_error().unwrap_or(libc::EIO);
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path).map_err(io)?;
        let same_size = file.metadata().map_err(io)?.len() == len as u64;
        if !same_size {
            file.set_len(len as u64).map_err(io)?;
        }
        let mut ix = Self::map(OwnedFd::from(file), len)?;
        if same_size && ix.adopt().is_ok() {
            return Ok((ix, true));
        }
        // Start from zeros: `init` writes only the header and table, and leftovers of an
        // older layout in the entry area would read as torn writes on the next adopt.
        ftruncate(&ix.fd, 0).map_err(errno)?;
        ftruncate(&ix.fd, len as libc::off_t).map_err(errno)?;
        ix.init(root)?;
        Ok((ix, false))
    }

    fn map(fd: OwnedFd, len: usize) -> Result<Index, i32> {
        if !len.is_multiple_of(4096) {
            return Err(libc::EINVAL);
        }
        let (entry_cap, table_cap) = Self::layout(len).ok_or(libc::EINVAL)?;
        let base = unsafe {
            mmap(
                None,
                NonZeroUsize::new(len).ok_or(libc::EINVAL)?,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                &fd,
                0,
            )
        }
        .map_err(errno)?
        .cast::<u8>();
        Ok(Index { fd, base, len, entry_cap, table_cap, free: Vec::new(), next_unused: 0, live: 0, names: HashMap::new() })
    }

    /// Takes over a region an earlier daemon wrote: checks the header against this region's
    /// geometry and every entry for a write that was cut short, then rebuilds the allocation
    /// state from the entries. The region itself is not touched.
    fn adopt(&mut self) -> Result<(), i32> {
        let h = unsafe { self.header_ptr().read() };
        let entries_off = HEADER_SIZE as u64;
        let table_off = entries_off + self.entry_cap as u64 * ENTRY_SIZE as u64;
        let geometry_ok = h.magic == MAGIC
            && h.version == FORMAT_VERSION
            && h.entry_cap == self.entry_cap
            && h.table_cap == self.table_cap
            && h.entries_off == entries_off
            && h.table_off == table_off
            && h.region_len == self.len as u64;
        if !geometry_ok {
            return Err(libc::EINVAL);
        }
        let (mut live, mut next_unused, mut free) = (0, 0, Vec::new());
        let mut names: HashMap<(u64, u64), Vec<u32>> = HashMap::new();
        for slot in 0..self.entry_cap {
            let e = unsafe { self.entry_ptr(slot).read() };
            if e.seq & 1 != 0 {
                return Err(libc::EINVAL);
            }
            if e.generation == 0 {
                continue;
            }
            next_unused = slot + 1;
            if e.is_live() {
                live += 1;
                if e.host_ino != 0 {
                    names.entry((e.host_dev, e.host_ino)).or_default().push(slot);
                }
            } else {
                free.push(slot);
            }
        }
        if !unsafe { self.entry_ptr(ROOT_SLOT).read() }.is_live() {
            return Err(libc::EINVAL);
        }
        // `alloc` pops from the end: hand out the lowest freed slot first.
        free.reverse();
        (self.free, self.next_unused, self.live, self.names) = (free, next_unused, live, names);
        Ok(())
    }

    /// Writes the header and an empty table, then installs the root directory.
    fn init(&mut self, root: Attr) -> Result<(), i32> {
        let ix = self;
        let (entry_cap, table_cap, len) = (ix.entry_cap, ix.table_cap, ix.len);
        (ix.free, ix.next_unused, ix.live) = (Vec::new(), 0, 0);
        ix.names.clear();
        let entries_off = HEADER_SIZE as u64;
        let table_off = entries_off + entry_cap as u64 * ENTRY_SIZE as u64;
        unsafe {
            ix.header_ptr().write(Header {
                magic: MAGIC,
                version: FORMAT_VERSION,
                flags: 0,
                entry_cap,
                table_cap,
                entries_off,
                table_off,
                region_len: len as u64,
                live: 0,
                _pad: 0,
            });
            for i in 0..table_cap {
                ix.table(i).store(NO_SLOT, Ordering::Relaxed);
            }
        }
        // Root: slot 0, its own parent, never in the hash table.
        let slot = ix.alloc()?;
        debug_assert_eq!(slot, ROOT_SLOT);
        ix.write(slot, |e| {
            e.flags = ENTRY_LIVE;
            e.generation += 1;
            e.parent = ROOT_SLOT;
            e.hash_next = NO_SLOT;
            e.first_child = NO_SLOT;
            e.next_sibling = NO_SLOT;
            e.prev_sibling = NO_SLOT;
            e.name_len = 0;
            set_attr(e, &root);
        });
        ix.bump_live(1);
        Ok(())
    }

    /// Entry and table capacities that fit in `len`, table twice the entries.
    fn layout(len: usize) -> Option<(u32, u32)> {
        let usable = len.checked_sub(HEADER_SIZE)?;
        let mut entry_cap = usable / (ENTRY_SIZE + 8);
        loop {
            if entry_cap < 2 {
                return None;
            }
            let table_cap = (entry_cap * 2).next_power_of_two();
            if entry_cap * ENTRY_SIZE + table_cap * 4 <= usable {
                return Some((entry_cap as u32, table_cap as u32));
            }
            entry_cap -= 1;
        }
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
    /// Host address of the region, for registering it as a read-only
    /// guest memory slot. Page-aligned; `len()` is a page multiple.
    pub fn as_ptr(&self) -> *const u8 {
        self.base.as_ptr()
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        false
    }
    pub fn entry_cap(&self) -> u32 {
        self.entry_cap
    }
    pub fn live_count(&self) -> u32 {
        self.live
    }
    /// Whether one more `insert` can succeed. Checked before any host
    /// mutation so the directory and the index never disagree.
    pub fn has_room(&self) -> bool {
        !self.free.is_empty() || self.next_unused < self.entry_cap
    }

    // --- raw access -------------------------------------------------

    fn header_ptr(&self) -> *mut Header {
        self.base.as_ptr().cast()
    }

    fn entry_ptr(&self, slot: u32) -> *mut Entry {
        assert!(slot < self.entry_cap, "slot out of range");
        unsafe { self.base.as_ptr().add(HEADER_SIZE + slot as usize * ENTRY_SIZE).cast() }
    }

    fn table(&self, i: u32) -> &AtomicU32 {
        assert!(i < self.table_cap);
        let off = HEADER_SIZE + self.entry_cap as usize * ENTRY_SIZE + i as usize * 4;
        unsafe { AtomicU32::from_ptr(self.base.as_ptr().add(off).cast()) }
    }

    fn bucket(&self, parent: u32, name: &[u8]) -> u32 {
        (name_hash(parent, name) & (self.table_cap as u64 - 1)) as u32
    }

    /// The daemon's own view. Sound because this thread is the only writer.
    pub fn entry(&self, slot: u32) -> Option<&Entry> {
        if slot >= self.entry_cap {
            return None;
        }
        Some(unsafe { &*self.entry_ptr(slot) })
    }

    /// Live entries only.
    pub fn live(&self, slot: u32) -> Option<&Entry> {
        self.entry(slot).filter(|e| e.is_live())
    }

    /// Seqlock read, exactly as the guest performs it.
    pub fn read(&self, slot: u32) -> Option<Entry> {
        if slot >= self.entry_cap {
            return None;
        }
        let p = self.entry_ptr(slot);
        let seq = unsafe { AtomicU32::from_ptr(p.cast()) };
        loop {
            let s1 = seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let copy = unsafe { p.read_volatile() };
            fence(Ordering::Acquire);
            if seq.load(Ordering::Relaxed) == s1 {
                return Some(copy);
            }
        }
    }

    /// Mutate one entry inside its seqlock.
    fn write(&mut self, slot: u32, f: impl FnOnce(&mut Entry)) {
        let p = self.entry_ptr(slot);
        let seq = unsafe { AtomicU32::from_ptr(p.cast()) };
        let s = seq.load(Ordering::Relaxed);
        seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        f(unsafe { &mut *p });
        fence(Ordering::Release);
        seq.store(s.wrapping_add(2), Ordering::Release);
    }

    fn bump_live(&mut self, delta: i32) {
        self.live = (self.live as i32 + delta) as u32;
        unsafe { (*self.header_ptr()).live = self.live };
    }

    fn alloc(&mut self) -> Result<u32, i32> {
        if let Some(s) = self.free.pop() {
            return Ok(s);
        }
        if self.next_unused < self.entry_cap {
            self.next_unused += 1;
            return Ok(self.next_unused - 1);
        }
        Err(libc::ENOSPC)
    }

    // --- queries ----------------------------------------------------

    pub fn lookup(&self, parent: u32, name: &[u8]) -> Option<u32> {
        let mut slot = self.table(self.bucket(parent, name)).load(Ordering::Acquire);
        while slot != NO_SLOT {
            let e = self.entry(slot)?;
            if e.is_live() && e.parent == parent && e.name() == name {
                return Some(slot);
            }
            slot = e.hash_next;
        }
        None
    }

    /// Child slots of `parent`, most recently linked first.
    pub fn children(&self, parent: u32) -> impl Iterator<Item = u32> + '_ {
        let mut cur = self.live(parent).map_or(NO_SLOT, |e| e.first_child);
        std::iter::from_fn(move || {
            if cur == NO_SLOT {
                return None;
            }
            let s = cur;
            cur = self.entry(s).map_or(NO_SLOT, |e| e.next_sibling);
            Some(s)
        })
    }

    pub fn is_dir(&self, slot: u32) -> bool {
        self.live(slot).is_some_and(|e| e.mode & libc::S_IFMT == libc::S_IFDIR)
    }

    pub fn name(&self, slot: u32) -> Option<OsString> {
        self.live(slot).map(|e| OsStr::from_bytes(e.name()).to_owned())
    }

    /// Path relative to the root, `"."` for the root itself.
    pub fn path(&self, slot: u32) -> Option<PathBuf> {
        let mut parts = Vec::new();
        let mut cur = slot;
        while cur != ROOT_SLOT {
            let e = self.live(cur)?;
            parts.push(OsStr::from_bytes(e.name()).to_owned());
            cur = e.parent;
        }
        self.live(ROOT_SLOT)?;
        if parts.is_empty() {
            return Some(PathBuf::from("."));
        }
        Some(parts.iter().rev().collect())
    }

    // --- mutation ---------------------------------------------------

    /// Add `name` under `parent`. The caller has already created it on the host.
    pub fn insert(&mut self, parent: u32, name: &[u8], attr: Attr) -> Result<u32, i32> {
        check_name(name)?;
        if !self.is_dir(parent) {
            return Err(libc::ENOTDIR);
        }
        if self.lookup(parent, name).is_some() {
            return Err(libc::EEXIST);
        }
        let slot = self.alloc()?;
        self.write(slot, |e| {
            e.flags = ENTRY_LIVE;
            e.generation += 1;
            e.parent = parent;
            e.first_child = NO_SLOT;
            e.name_len = name.len() as u16;
            e.name[..name.len()].copy_from_slice(name);
            set_attr(e, &attr);
            (e.host_dev, e.host_ino) = (attr.dev, attr.ino);
        });
        self.note_name(slot, attr.dev, attr.ino);
        self.link(slot);
        self.bump_live(1);
        Ok(slot)
    }

    /// Drop a childless entry. The caller has already removed it on the host.
    pub fn remove(&mut self, slot: u32) -> Result<(), i32> {
        let e = *self.live(slot).ok_or(libc::ENOENT)?;
        if slot == ROOT_SLOT {
            return Err(libc::EBUSY);
        }
        if e.first_child != NO_SLOT {
            return Err(libc::ENOTEMPTY);
        }
        self.unlink(slot);
        self.write(slot, |e| e.flags = 0);
        if let Some(slots) = self.names.get_mut(&(e.host_dev, e.host_ino)) {
            slots.retain(|&s| s != slot);
            if slots.is_empty() {
                self.names.remove(&(e.host_dev, e.host_ino));
            }
        }
        self.free.push(slot);
        self.bump_live(-1);
        Ok(())
    }

    /// Move `slot` to `new_parent/new_name`. The target name must be free.
    pub fn rename(&mut self, slot: u32, new_parent: u32, new_name: &[u8]) -> Result<(), i32> {
        check_name(new_name)?;
        self.live(slot).ok_or(libc::ENOENT)?;
        if slot == ROOT_SLOT {
            return Err(libc::EBUSY);
        }
        if !self.is_dir(new_parent) {
            return Err(libc::ENOTDIR);
        }
        if self.lookup(new_parent, new_name).is_some() {
            return Err(libc::EEXIST);
        }
        self.unlink(slot);
        self.write(slot, |e| {
            e.parent = new_parent;
            e.name_len = new_name.len() as u16;
            e.name[..new_name.len()].copy_from_slice(new_name);
        });
        self.link(slot);
        Ok(())
    }

    /// Stores `attr` for `slot`. The host identity is left alone: it belongs to the file the
    /// entry was made for, and is only filled in here if it was never recorded.
    pub fn set_attr(&mut self, slot: u32, attr: Attr) -> Result<(), i32> {
        let unrecorded = self.live(slot).ok_or(libc::ENOENT)?.host_ino == 0;
        self.write(slot, |e| {
            set_attr(e, &attr);
            if unrecorded {
                (e.host_dev, e.host_ino) = (attr.dev, attr.ino);
            }
        });
        if unrecorded {
            self.note_name(slot, attr.dev, attr.ino);
        }
        Ok(())
    }

    fn note_name(&mut self, slot: u32, dev: u64, ino: u64) {
        if ino != 0 {
            self.names.entry((dev, ino)).or_default().push(slot);
        }
    }

    /// Every live slot made for the host file `id`: one per name, for a hard-linked file.
    pub fn names_of(&self, id: (u64, u64)) -> &[u32] {
        self.names.get(&id).map_or(&[], Vec::as_slice)
    }

    /// The host file `slot` was made for, if recorded.
    pub fn host_file(&self, slot: u32) -> Option<(u64, u64)> {
        self.live(slot).filter(|e| e.host_ino != 0).map(|e| (e.host_dev, e.host_ino))
    }

    /// Publish `slot` at the head of its parent's child list and hash chain.
    fn link(&mut self, slot: u32) {
        let e = *self.entry(slot).unwrap();
        let parent = e.parent;
        let head = self.entry(parent).unwrap().first_child;
        let b = self.bucket(parent, e.name());
        let chain = self.table(b).load(Ordering::Relaxed);
        self.write(slot, |e| {
            e.next_sibling = head;
            e.prev_sibling = NO_SLOT;
            e.hash_next = chain;
        });
        if head != NO_SLOT {
            self.write(head, |h| h.prev_sibling = slot);
        }
        self.write(parent, |p| p.first_child = slot);
        self.table(b).store(slot, Ordering::Release);
    }

    /// Detach `slot` from its parent's child list and hash chain.
    fn unlink(&mut self, slot: u32) {
        let e = *self.entry(slot).unwrap();
        let (parent, prev, next) = (e.parent, e.prev_sibling, e.next_sibling);
        if prev == NO_SLOT {
            self.write(parent, |p| p.first_child = next);
        } else {
            self.write(prev, |p| p.next_sibling = next);
        }
        if next != NO_SLOT {
            self.write(next, |n| n.prev_sibling = prev);
        }
        let b = self.bucket(parent, e.name());
        let tbl = self.table(b);
        if tbl.load(Ordering::Relaxed) == slot {
            tbl.store(e.hash_next, Ordering::Release);
        } else {
            let mut cur = tbl.load(Ordering::Relaxed);
            while cur != NO_SLOT {
                let c = self.entry(cur).unwrap();
                if c.hash_next == slot {
                    let after = e.hash_next;
                    self.write(cur, |c| c.hash_next = after);
                    break;
                }
                cur = c.hash_next;
            }
        }
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        unsafe {
            let _ = munmap(self.base.cast(), self.len);
        }
    }
}

fn set_attr(e: &mut Entry, a: &Attr) {
    e.mode = a.mode;
    e.nlink = a.nlink;
    e.uid = a.uid;
    e.gid = a.gid;
    e.size = a.size;
    e.rdev = a.rdev;
    e.atime = a.atime;
    e.mtime = a.mtime;
    e.ctime = a.ctime;
}

/// A single path component: non-empty, within NAME_MAX, no slash or NUL,
/// not `.` or `..`.
pub fn check_name(name: &[u8]) -> Result<(), i32> {
    if name.is_empty() || name.len() > NAME_MAX || name == b"." || name == b".." {
        return Err(libc::EINVAL);
    }
    if name.contains(&b'/') || name.contains(&0) {
        return Err(libc::EINVAL);
    }
    Ok(())
}
