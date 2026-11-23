// SPDX-License-Identifier: GPL-2.0

//! The host's index of the exported tree, read in place from shared memory.
//!
//! The host is the only writer. Every entry carries a seqlock: odd while the host updates it,
//! even and unchanged across a copy when the copy is consistent. Readers copy an entry out and
//! retry on a torn read, so nothing here takes a lock or sleeps.

use core::ptr::NonNull;

use kernel::{
    bindings,
    prelude::*,
    processor::cpu_relax,
    sync::barrier::smp_rmb,
};

/// "SHAREDFS" as a little-endian u64, as the host writes it.
pub(crate) const MAGIC: u64 = u64::from_le_bytes(*b"SHAREDFS");
pub(crate) const FORMAT_VERSION: u32 = 1;
const HEADER_SIZE: usize = 4096;
const ENTRY_SIZE: usize = 384;

/// Slot 0 is always the root directory.
pub(crate) const ROOT_SLOT: u32 = 0;
/// Nil link in the child list, hash chain and table.
pub(crate) const NO_SLOT: u32 = u32::MAX;
const ENTRY_LIVE: u32 = 1;

/// How many times to spin on an entry the host is in the middle of writing before giving up.
/// A write takes nanoseconds; a count this high only trips if the host died mid-update.
const SEQ_RETRIES: u32 = 1 << 20;

/// `MEMREMAP_WB`: cached mapping. The region is ordinary host RAM shared read-only with the
/// guest; metadata reads want the cache.
const MEMREMAP_WB: usize = 1;

/// First page of the region.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Header {
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

/// One file, directory or symlink.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Entry {
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
    pub reserved: [u8; 32],
}

const _: () = assert!(core::mem::size_of::<Entry>() == ENTRY_SIZE);
const _: () = assert!(core::mem::size_of::<Header>() <= HEADER_SIZE);

impl Entry {
    pub(crate) fn name(&self) -> &[u8] {
        let len = usize::from(self.name_len).min(self.name.len());
        &self.name[..len]
    }

    pub(crate) fn is_live(&self) -> bool {
        self.flags & ENTRY_LIVE != 0
    }
}

/// FNV-1a over the parent slot and the name. The host computes the same.
fn name_hash(parent: u32, name: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in Iterator::chain(parent.to_le_bytes().iter(), name.iter()) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The mapped index region.
///
/// # Invariants
///
/// `base` maps `len` bytes of the device's shared memory region, whose header passed validation
/// in [`Index::map`]: entries and table lie inside the region.
pub(crate) struct Index {
    base: NonNull<u8>,
    len: usize,
    entry_cap: u32,
    table_cap: u32,
    entries_off: usize,
    table_off: usize,
}

// SAFETY: The mapping is read-only shared memory; concurrent reads from any thread are fine and
// nothing in `Index` is thread-affine.
unsafe impl Send for Index {}
// SAFETY: See above.
unsafe impl Sync for Index {}

impl Index {
    /// Maps `len` bytes of the guest-physical range at `addr` and validates the header.
    ///
    /// # Safety
    ///
    /// The range must be the device's shared memory region, which the host keeps mapped for as
    /// long as the device exists.
    pub(crate) unsafe fn map(addr: u64, len: u64) -> Result<Self> {
        let len = usize::try_from(len)?;
        if len < HEADER_SIZE {
            return Err(EINVAL);
        }
        // SAFETY: The caller guarantees the range is device memory; `memremap` validates the
        // request and returns null on failure.
        let p = unsafe { bindings::memremap(addr as _, len, MEMREMAP_WB as _) };
        let base = NonNull::new(p.cast::<u8>()).ok_or(ENOMEM)?;
        let mut index = Self {
            base,
            len,
            entry_cap: 0,
            table_cap: 0,
            entries_off: 0,
            table_off: 0,
        };

        let h = index.header();
        if h.magic != MAGIC {
            pr_err!("bad index magic {:#x} at {:#x}\n", h.magic, addr);
            return Err(EINVAL);
        }
        if h.version != FORMAT_VERSION {
            pr_err!("index format {} not supported\n", h.version);
            return Err(EINVAL);
        }
        let entries_off = usize::try_from(h.entries_off)?;
        let table_off = usize::try_from(h.table_off)?;
        let entries_end = (h.entry_cap as usize)
            .checked_mul(ENTRY_SIZE)
            .and_then(|n| n.checked_add(entries_off))
            .ok_or(EINVAL)?;
        let table_end = (h.table_cap as usize)
            .checked_mul(4)
            .and_then(|n| n.checked_add(table_off))
            .ok_or(EINVAL)?;
        if entries_end > len || table_end > len || !h.table_cap.is_power_of_two() {
            pr_err!("index layout does not fit its region\n");
            return Err(EINVAL);
        }

        // INVARIANT: The layout was checked against `len` above.
        index.entry_cap = h.entry_cap;
        index.table_cap = h.table_cap;
        index.entries_off = entries_off;
        index.table_off = table_off;
        Ok(index)
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn header(&self) -> Header {
        // SAFETY: The region is at least one page and starts with a `Header`; the host may update
        // fields at any time, so this is a volatile read of a plain-data struct.
        unsafe { core::ptr::read_volatile(self.base.as_ptr().cast::<Header>()) }
    }

    fn entry_ptr(&self, slot: u32) -> *const Entry {
        // SAFETY: Callers check `slot < entry_cap`, and by the type invariants every such entry
        // lies inside the mapping.
        unsafe {
            self.base
                .as_ptr()
                .add(self.entries_off + slot as usize * ENTRY_SIZE)
                .cast::<Entry>()
        }
    }

    fn table(&self, bucket: u32) -> u32 {
        // SAFETY: `bucket` is masked by `table_cap`, and by the type invariants the table lies
        // inside the mapping. The host updates buckets with release stores.
        unsafe {
            core::ptr::read_volatile(
                self.base
                    .as_ptr()
                    .add(self.table_off + bucket as usize * 4)
                    .cast::<u32>(),
            )
        }
    }

    /// Copies an entry out consistently, or returns `None` if the slot is out of range or the
    /// host never finished writing it.
    pub(crate) fn read(&self, slot: u32) -> Option<Entry> {
        if slot >= self.entry_cap {
            return None;
        }
        let p = self.entry_ptr(slot);
        let seq_ptr = p.cast::<u32>();
        let mut retries = SEQ_RETRIES;
        loop {
            // SAFETY: `p` is a valid entry pointer and `seq` is its first field.
            let s1 = unsafe { core::ptr::read_volatile(seq_ptr) };
            if s1 & 1 != 0 {
                retries = retries.checked_sub(1)?;
                cpu_relax();
                continue;
            }
            smp_rmb();
            // SAFETY: `p` is a valid entry pointer; the host may write concurrently, which the
            // sequence check below detects.
            let copy = unsafe { core::ptr::read_volatile(p) };
            smp_rmb();
            // SAFETY: As above.
            if unsafe { core::ptr::read_volatile(seq_ptr) } == s1 {
                return Some(copy);
            }
            retries = retries.checked_sub(1)?;
        }
    }

    /// Reads a live entry.
    pub(crate) fn live(&self, slot: u32) -> Option<Entry> {
        self.read(slot).filter(Entry::is_live)
    }

    /// Finds the child of `parent` called `name`.
    pub(crate) fn lookup(&self, parent: u32, name: &[u8]) -> Option<(u32, Entry)> {
        let bucket = (name_hash(parent, name) & u64::from(self.table_cap - 1)) as u32;
        let mut slot = self.table(bucket);
        // A chain can't legitimately be longer than the table; the bound guards against a
        // cycle read from an index the host is rewriting.
        let mut steps = self.entry_cap;
        while slot != NO_SLOT && steps > 0 {
            let e = self.read(slot)?;
            if e.is_live() && e.parent == parent && e.name() == name {
                return Some((slot, e));
            }
            slot = e.hash_next;
            steps -= 1;
        }
        None
    }
}
