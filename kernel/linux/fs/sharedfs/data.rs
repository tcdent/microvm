// SPDX-License-Identifier: GPL-2.0

//! File data through the window.
//!
//! The host maps 2 MiB chunks of files into the data window on request; the guest reads and
//! writes them in place through DAX. This module keeps the table of which chunk of which file
//! sits where, hands ranges to the DAX code through the iomap operations, and reclaims the
//! least recently used chunk when the window is full.

use kernel::{
    alloc::KVec,
    fs::{dax, inode::INode, sb, Offset},
    prelude::*,
    rbtree::RBTree,
    sync::MutexGuard,
};

use crate::queue::{map, op, Req, Session, HEADER_LEN};
use crate::SharedFs;

/// Mapping granularity, matching the host.
pub(crate) const CHUNK: u64 = 2 << 20;
const PAGE: u64 = 4096;

/// The DAX device over the data window.
pub(crate) type Dax = dax::Device;

/// One mapped chunk.
#[derive(Clone, Copy)]
struct Chunk {
    /// Offset in the window.
    woff: u64,
    writable: bool,
    /// Extents handed out by `begin` and not yet returned by `end`. A chunk with users is not
    /// reclaimed.
    users: u32,
    /// Last use, for choosing a reclaim victim.
    tick: u64,
}

/// Key: index slot and chunk index within the file.
/// A chunk of a file: the file's inode number and the chunk's index within it. The inode
/// number carries the entry's generation, so a file unlinked while still mapped keeps its
/// chunks apart from those of a new file that reuses its slot.
type Key = (u64, u32);

pub(crate) struct Maps {
    tree: RBTree<Key, Chunk>,
    /// Window offsets given back, ready to reuse.
    free: KVec<u64>,
    /// First never-used window offset.
    next: u64,
    limit: u64,
    tick: u64,
}

impl Maps {
    pub(crate) fn new(window_len: u64) -> Self {
        Self {
            tree: RBTree::new(),
            free: KVec::new(),
            next: 0,
            limit: window_len,
            tick: 0,
        }
    }

    /// Window space left to hand out without reclaiming.
    fn free_chunks(&self) -> u64 {
        self.free.len() as u64 + (self.limit - self.next) / CHUNK
    }

    fn alloc(&mut self) -> Option<u64> {
        if let Some(w) = self.free.pop() {
            return Some(w);
        }
        if self.next + CHUNK <= self.limit {
            let w = self.next;
            self.next += CHUNK;
            return Some(w);
        }
        None
    }

    fn release(&mut self, woff: u64) {
        // Failing to remember a free chunk only leaks window space until unmount.
        let _ = self.free.push(woff, GFP_KERNEL);
    }

    /// The least recently used chunk nobody is in the middle of using, taken out of the table.
    fn take_victim(&mut self) -> Option<(Key, Chunk)> {
        let key = *self
            .tree
            .iter()
            .filter(|(_, c)| c.users == 0)
            .min_by_key(|(_, c)| c.tick)?
            .0;
        let chunk = self.tree.remove(&key)?;
        Some((key, chunk))
    }
}


/// A file the guest has open, keyed by inode number. The daemon holds a read handle while it
/// has readers and a write handle while it has writers, which keep the access granted at open
/// and keep the file reachable after an unlink. A mapped file counts as open: the VFS only
/// releases a file when its last mapping goes.
pub(crate) struct OpenFile {
    readers: u32,
    writers: u32,
    /// Created by this guest and not yet opened: the daemon kept the write handle it created
    /// the file with, which the creator's open adopts (or gives back, if that open is
    /// read-only).
    created: bool,
}

impl Session {
    /// A file this guest just created: the daemon holds the creator's write handle.
    pub(crate) fn note_created(&self, ino: u64) {
        let file = OpenFile { readers: 0, writers: 0, created: true };
        let _ = self.open_files.lock().try_create_and_insert(ino, file, GFP_KERNEL);
    }

    /// An open of `ino`. The first reader and the first writer each ask the daemon for a
    /// handle of that kind.
    pub(crate) fn opened(&self, ino: u64, readable: bool, writable: bool) -> Result {
        let mut files = self.open_files.lock();
        if files.get(&ino).is_none() {
            files
                .try_create_and_insert(ino, OpenFile { readers: 0, writers: 0, created: false }, GFP_KERNEL)
                .map_err(|_| ENOMEM)?;
        }
        let f = files.get_mut(&ino).ok_or(ENOMEM)?;
        let created = core::mem::replace(&mut f.created, false);
        if created && !writable {
            // The creator opened read-only after all: give the creation handle back.
            let _ = self.write_request(op::RELEASE_WRITE, ino);
        }
        if writable {
            if f.writers == 0 && !created {
                self.write_request(op::OPEN_WRITE, ino)?;
            }
            f.writers += 1;
        }
        if readable {
            if f.readers == 0 {
                self.write_request(op::OPEN_READ, ino)?;
            }
            f.readers += 1;
        }
        Ok(())
    }

    /// The last reference to an open file of `ino` went away.
    pub(crate) fn released(&self, ino: u64, readable: bool, writable: bool) {
        let mut files = self.open_files.lock();
        let Some(f) = files.get_mut(&ino) else { return };
        if writable {
            f.writers = f.writers.saturating_sub(1);
            if f.writers == 0 {
                let _ = self.write_request(op::RELEASE_WRITE, ino);
            }
        }
        if readable {
            f.readers = f.readers.saturating_sub(1);
            if f.readers == 0 {
                let _ = self.write_request(op::RELEASE_READ, ino);
            }
        }
        if f.readers == 0 && f.writers == 0 && !f.created {
            files.remove(&ino);
        }
    }

    /// After a resume the daemon is a new one with no handles: ask again for every file still
    /// open. A file unlinked meanwhile, or no longer permitted, cannot be reopened; its users
    /// get errors from then on.
    pub(crate) fn reopen_files(&self) {
        let files = self.open_files.lock();
        for (ino, f) in files.iter() {
            let ok = (f.writers == 0 || self.write_request(op::OPEN_WRITE, *ino).is_ok())
                && (f.readers == 0 || self.write_request(op::OPEN_READ, *ino).is_ok());
            if !ok {
                pr_warn!("inode {:#x}: could not reopen after resume\n", ino);
            }
        }
    }

    fn write_request(&self, opcode: u32, ino: u64) -> Result {
        let mut req = Req::new(opcode).u64(ino);
        self.request(req.as_bytes(), HEADER_LEN).map(|_| ())
    }

    pub(crate) fn map(&self, ino: u64, generation: u64, foff: u64, woff: u64, writable: bool) -> Result {
        let flags = if writable { map::WRITE } else { 0 };
        let mut req = Req::new(op::MAP)
            .u64(ino)
            .u64(generation)
            .u64(foff)
            .u64(CHUNK)
            .u64(woff)
            .u32(flags);
        self.request(req.as_bytes(), HEADER_LEN).map(|_| ())
    }

    pub(crate) fn unmap(&self, woff: u64) -> Result {
        let mut req = Req::new(op::UNMAP).u64(woff).u64(CHUNK);
        self.request(req.as_bytes(), HEADER_LEN).map(|_| ())
    }

    pub(crate) fn extend(&self, ino: u64, size: u64) -> Result {
        let mut req = Req::new(op::EXTEND).u64(ino).u64(size);
        self.request(req.as_bytes(), HEADER_LEN).map(|_| ())
    }

    pub(crate) fn fsync(&self, ino: u64) -> Result {
        let mut req = Req::new(op::FSYNC).u64(ino);
        self.request(req.as_bytes(), HEADER_LEN).map(|_| ())
    }

    /// Returns the window offset of chunk `idx` of the inode, mapped at least for reading, or
    /// for writing when `write`, and counts the caller as a user of it.
    fn ensure_chunk(&self, inode: &INode<SharedFs>, idx: u32, write: bool, fault: bool) -> Result<u64> {
        let node = inode.data();
        let key = (inode.ino(), idx);
        let ino = inode.ino();
        let foff = u64::from(idx) * CHUNK;
        loop {
            let mut maps = self.maps.lock();
            if let Some(c) = maps.tree.get(&key).copied() {
                if write && !c.writable {
                    self.map(ino, node.generation, foff, c.woff, true)?;
                }
                let tick = maps.tick + 1;
                maps.tick = tick;
                let c = maps.tree.get_mut(&key).ok_or(EAGAIN)?;
                c.writable |= write;
                c.users += 1;
                c.tick = tick;
                return Ok(c.woff);
            }
            let Some(woff) = maps.alloc() else {
                if fault {
                    // A fault holds locks reclaim needs; the fault handler retries after a pause.
                    return Err(EAGAIN);
                }
                drop(maps);
                self.reclaim_one(inode.super_block(), inode.ino())?;
                continue;
            };
            if let Err(e) = self.map(ino, node.generation, foff, woff, write) {
                maps.release(woff);
                return Err(e);
            }
            let tick = maps.tick + 1;
            maps.tick = tick;
            let chunk = Chunk { woff, writable: write, users: 1, tick };
            if maps.tree.try_create_and_insert(key, chunk, GFP_KERNEL).is_err() {
                let _ = self.unmap(woff);
                maps.release(woff);
                return Err(ENOMEM);
            }
            return Ok(woff);
        }
    }

    /// Frees the least recently used idle chunk, preferring one that is not `mine`'s: the inode
    /// asking for space, whose lock the caller holds (0 when nobody does).
    fn reclaim_one(&self, sb: &sb::SuperBlock<SharedFs>, mine: u64) -> Result {
        let (key, chunk) = {
            let mut maps = self.maps.lock();
            // Prefer another file's chunk; fall back to one of our own that is idle.
            let pick = maps
                .tree
                .iter()
                .filter(|(k, c)| c.users == 0 && k.0 != mine)
                .min_by_key(|(_, c)| c.tick)
                .map(|(k, _)| *k);
            match pick {
                Some(k) => {
                    let c = maps.tree.remove(&k).ok_or(ENOSPC)?;
                    (k, c)
                }
                None => maps.take_victim().ok_or(ENOSPC)?,
            }
        };
        let start = u64::from(key.1) * CHUNK;
        if let Some(inode) = sb.cached_inode(key.0) {
            // Nobody may keep pages of the range mapped once the window behind it moves.
            dax::break_layout(&inode, start as Offset, (start + CHUNK - 1) as Offset)?;
        }
        let _ = self.unmap(chunk.woff);
        self.maps.lock().release(chunk.woff);
        Ok(())
    }

    /// Reclaim on behalf of page faults, which cannot reclaim themselves (a fault holds its
    /// file's invalidate lock, which reclaim of another file's chunk could need). Runs from a
    /// work item holding no locks, frees a batch of idle chunks, and lets the waiting faults
    /// retry.
    pub(crate) fn reclaim_for_faults(&self) {
        const BATCH: u64 = 16;
        let mount = self.mount.lock();
        let Some(m) = mount.as_ref() else { return };
        while self.maps.lock().free_chunks() < BATCH {
            if self.reclaim_one(m.sb(), 0).is_err() {
                break;
            }
        }
    }

    /// Gives back every chunk of a file. Called when its inode is destroyed, after the VFS has
    /// already dropped its pages.
    /// Forgets every window mapping ahead of a hibernation: the daemon that comes back will
    /// have mapped nothing, so processes must fault their pages in again. Each range is first
    /// unmapped from every process, as reclaim does, so no stale page is ever touched.
    pub(crate) fn drop_window(&self) {
        let mount = self.mount.lock();
        let mut maps = self.maps.lock();
        if let Some(m) = mount.as_ref() {
            let sb = m.sb();
            for (key, _) in maps.tree.iter() {
                if let Some(inode) = sb.cached_inode(key.0) {
                    let start = u64::from(key.1) * CHUNK;
                    let _ = dax::break_layout(&inode, start as Offset, (start + CHUNK - 1) as Offset);
                }
            }
        }
        let limit = maps.limit;
        *maps = Maps::new(limit);
    }

    pub(crate) fn release_file(&self, ino: u64) {
        let mut maps = self.maps.lock();
        let keys: KVec<Key> = {
            let mut v = KVec::new();
            for k in maps.tree.keys().filter(|k| k.0 == ino) {
                let _ = v.push(*k, GFP_KERNEL);
            }
            v
        };
        for k in keys {
            if let Some(c) = maps.tree.remove(&k) {
                let _ = self.unmap(c.woff);
                maps.release(c.woff);
            }
        }
    }

    /// Drops the use count `ensure_chunk` added.
    fn put_chunk(&self, key: Key) {
        let mut maps: MutexGuard<'_, Maps> = self.maps.lock();
        if let Some(c) = maps.tree.get_mut(&key) {
            c.users = c.users.saturating_sub(1);
        }
    }
}

#[vtable]
impl dax::Operations for SharedFs {
    type FileSystem = SharedFs;

    fn begin(inode: &INode<SharedFs>, pos: Offset, length: Offset, flags: u32) -> Result<dax::Extent<'_>> {
        let session = &inode.super_block().data().session;
        let dax = session.dax.as_ref().ok_or(ENODEV)?;
        let write = flags & dax::flags::WRITE != 0;
        let fault = flags & dax::flags::FAULT != 0;
        let i_size = inode.size();
        if !write && pos >= i_size {
            return Ok(dax::Extent::Hole { length });
        }
        let idx = u32::try_from(pos as u64 / CHUNK)?;
        let start = u64::from(idx) * CHUNK;
        let woff = match session.ensure_chunk(inode, idx, write, fault) {
            Ok(woff) => woff,
            Err(e) if e == EAGAIN && fault => {
                // The window is full and this fault cannot reclaim: have the worker do it.
                session.kick_reclaim();
                return Err(e);
            }
            Err(e) => {
                // EAGAIN is the fault path's normal back-off; anything else fails the access,
                // and on a fault that is a SIGBUS for the process.
                if e != EAGAIN {
                    pr_warn!(
                        "inode {:#x} chunk {}: cannot map ({}{}): {:?}\n",
                        inode.ino(),
                        idx,
                        if write { "write" } else { "read" },
                        if fault { ", fault" } else { "" },
                        e
                    );
                }
                return Err(e);
            }
        };

        let off = pos as u64 - start;
        let mut len = length.min((start + CHUNK - pos as u64) as Offset);
        if !write {
            len = len.min(i_size - pos);
        }
        if fault {
            len = (len + PAGE as Offset - 1) & !(PAGE as Offset - 1);
        }
        Ok(dax::Extent::Mapped {
            device: dax,
            window_offset: woff + off,
            length: len,
            private: idx as usize,
        })
    }

    fn end(inode: &INode<SharedFs>, _pos: Offset, _length: Offset, _written: isize, _flags: u32, private: usize) {
        inode.super_block().data().session.put_chunk((inode.ino(), private as u32));
    }
}
