//! The data window: a reserved range of host virtual addresses, registered
//! with KVM as one guest-physical region, into which the daemon maps ranges
//! of host files on the guest's request. What the guest sees at those
//! addresses is the host page cache; nothing is copied.

use std::io;
use std::num::NonZeroUsize;
use std::os::fd::BorrowedFd;
use std::ptr::NonNull;

use nix::sys::mman::{MapFlags, ProtFlags, mmap, mmap_anonymous, munmap};

use crate::server::Errno;

pub const PAGE: u64 = 4096;

/// A reservation of `len` bytes of address space, `PROT_NONE` until mapped.
pub struct Window {
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: The window is only touched through `&self` methods that are each a single `mmap`
// call on disjoint or deliberately replaced ranges; the kernel serialises those.
unsafe impl Send for Window {}
unsafe impl Sync for Window {}

impl Window {
    /// Reserves `len` bytes. Nothing is backed until [`Window::map`] places a file there.
    pub fn reserve(len: usize) -> io::Result<Window> {
        let len_nz = NonZeroUsize::new(len).ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: Anonymous reservation; no file, no fixed address.
        let p = unsafe {
            mmap_anonymous(
                None,
                len_nz,
                ProtFlags::PROT_NONE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_NORESERVE,
            )
        }?;
        Ok(Window { base: p.cast(), len })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Validates a window range the guest named: page aligned and inside the reservation.
    pub fn check(&self, woff: u64, len: u64) -> Result<(), Errno> {
        if len == 0 || woff % PAGE != 0 || len % PAGE != 0 {
            return Err(libc::EINVAL);
        }
        match woff.checked_add(len) {
            Some(end) if end <= self.len as u64 => Ok(()),
            _ => Err(libc::ERANGE),
        }
    }

    /// Places `len` bytes of `fd` starting at `foff` at window offset `woff`, replacing
    /// whatever was there. The guest's page tables are refreshed by the kernel's MMU notifier.
    pub fn map(&self, woff: u64, len: u64, fd: BorrowedFd<'_>, foff: u64, writable: bool) -> nix::Result<()> {
        self.check(woff, len).map_err(nix::errno::Errno::from_raw)?;
        let prot = if writable {
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE
        } else {
            ProtFlags::PROT_READ
        };
        let addr = NonZeroUsize::new(self.base.as_ptr() as usize + woff as usize).unwrap();
        // SAFETY: The range lies inside our own reservation and `MAP_FIXED` replaces it atomically.
        unsafe {
            mmap(
                Some(addr),
                NonZeroUsize::new(len as usize).unwrap(),
                prot,
                MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
                fd,
                foff as libc::off_t,
            )
        }?;
        Ok(())
    }

    /// Returns a range to the `PROT_NONE` reservation.
    pub fn unmap(&self, woff: u64, len: u64) -> nix::Result<()> {
        self.check(woff, len).map_err(nix::errno::Errno::from_raw)?;
        let addr = NonZeroUsize::new(self.base.as_ptr() as usize + woff as usize).unwrap();
        // SAFETY: As in `map`; an anonymous `PROT_NONE` mapping restores the reservation.
        unsafe {
            mmap_anonymous(
                Some(addr),
                NonZeroUsize::new(len as usize).unwrap(),
                ProtFlags::PROT_NONE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED | MapFlags::MAP_NORESERVE,
            )
        }?;
        Ok(())
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: `base`/`len` are exactly what `reserve` mapped.
        let _ = unsafe { munmap(self.base.cast(), self.len) };
    }
}
