// SPDX-License-Identifier: GPL-2.0

//! The two virtqueues. On the request queue everything that is not a metadata read goes to the
//! host, one request per descriptor chain, and the caller sleeps until the host answers. On the
//! notification queue the driver keeps empty buffers posted and the host fills one per change
//! the guest should hear about.
//!
//! The vrings are only ever touched under `Session::queue`, in process context. The virtqueue
//! callbacks run in interrupt context and do nothing but schedule the drain work item, so no
//! interrupt-safe lock is needed around the rings.

use core::ptr::NonNull;

use kernel::{
    alloc::{KBox, KVec},
    bindings,
    prelude::*,
    sync::atomic::{ordering::Relaxed, Atomic},
    rbtree::RBTree,
    sync::{Arc, Completion, Mutex},
    virtio::virtqueue::{Virtqueue, Virtqueues},
    workqueue::{self, impl_has_work, new_work, Work, WorkItem},
};

use crate::data::{Dax, Maps, OpenFile};
use crate::index::Index;
use crate::Mount;

/// Request opcodes, shared with the host.
pub(crate) mod op {
    pub(crate) const FSYNC: u32 = 5;
    pub(crate) const CREATE: u32 = 6;
    pub(crate) const MKDIR: u32 = 7;
    pub(crate) const SYMLINK: u32 = 8;
    pub(crate) const UNLINK: u32 = 9;
    pub(crate) const RMDIR: u32 = 10;
    pub(crate) const RENAME: u32 = 11;
    pub(crate) const SETATTR: u32 = 12;
    pub(crate) const READLINK: u32 = 13;
    pub(crate) const STATFS: u32 = 14;
    pub(crate) const MAP: u32 = 15;
    pub(crate) const UNMAP: u32 = 16;
    pub(crate) const EXTEND: u32 = 17;
    pub(crate) const OPEN_WRITE: u32 = 18;
    pub(crate) const RELEASE_WRITE: u32 = 19;
    pub(crate) const OPEN_READ: u32 = 20;
    pub(crate) const RELEASE_READ: u32 = 21;
    pub(crate) const LINK: u32 = 22;
}

/// MAP flag bits, shared with the host.
pub(crate) mod map {
    pub(crate) const WRITE: u32 = 1 << 0;
}

/// SETATTR `valid` bits, shared with the host.
pub(crate) mod setattr {
    pub(crate) const MODE: u32 = 1 << 0;
    pub(crate) const UID: u32 = 1 << 1;
    pub(crate) const GID: u32 = 1 << 2;
    pub(crate) const SIZE: u32 = 1 << 3;
    pub(crate) const ATIME: u32 = 1 << 4;
    pub(crate) const MTIME: u32 = 1 << 5;
}

/// The longest request: a rename with two maximal names.
const REQ_MAX: usize = HEADER_LEN + 8 + 2 + 255 + 8 + 2 + 255 + 4;

/// Bytes in a request or response header: `len: u32`, `opcode`/`error: u32`, `unique: u64`.
pub(crate) const HEADER_LEN: usize = 16;

const REQUEST_VQ: usize = 0;
const NOTIFY_VQ: usize = 1;
/// Size of each posted notification buffer, matching the host.
const NOTIFY_BUF: usize = 320;
/// How many notification buffers stay posted. The host holds a backlog when none is free, so
/// this sets burst capacity, not correctness.
const NOTIFY_BUFS: usize = 64;
/// Byte offset of the name in an EVENT notification: the 8-byte header, mask, slot, parent and
/// cookie as `u32`, the `u64` generation, and the `u16` name length.
const NAME_OFF: usize = 8 + 4 * 4 + 8 + 2;

/// Notification kinds, shared with the host.
mod notify {
    pub(crate) const EVENT: u32 = 1;
    pub(crate) const OVERFLOW: u32 = 2;
}

/// A decoded notification. An event's name is left in the buffer and described by length.
enum Notification {
    Event { mask: u32, slot: u32, parent: u32, cookie: u32, generation: u64, name_len: usize },
    Overflow,
}

impl Notification {
    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Resp(b);
        let len = r.u32()? as usize;
        if len > b.len() || len < 8 {
            return Err(EIO);
        }
        match r.u32()? {
            notify::OVERFLOW => Ok(Self::Overflow),
            notify::EVENT => {
                let (mask, slot, parent, cookie) = (r.u32()?, r.u32()?, r.u32()?, r.u32()?);
                let generation = r.u64()?;
                let name_len = usize::from(r.u16()?);
                if NAME_OFF + name_len != len {
                    return Err(EIO);
                }
                Ok(Self::Event { mask, slot, parent, cookie, generation, name_len })
            }
            _ => Err(EIO),
        }
    }
}

/// The device's virtqueues. The pointers inside are owned by the virtio core for the lifetime
/// of the bound device; `Session::queue` serialises the driver's use of them.
struct Vqs(Virtqueues);

// SAFETY: See `Vqs`; the virtio core keeps the queues alive and the mutex keeps uses apart.
unsafe impl Send for Vqs {}
// SAFETY: See above.
unsafe impl Sync for Vqs {}

struct Queue {
    /// `None` between freeze and restore, when the device has no queues.
    vqs: Option<Vqs>,
    next_unique: u64,
    /// Buffers posted on the notification queue; a buffer's address is its token.
    notify_bufs: KVec<KBox<[u8; NOTIFY_BUF]>>,
}

impl Queue {
    fn vq(&self) -> Result<&Virtqueue> {
        let vqs = self.vqs.as_ref().ok_or(ENODEV)?;
        // SAFETY: `find_vqs` returned two valid queues, owned by `vqs`.
        Ok(unsafe { vqs.0[REQUEST_VQ].as_ref() })
    }

    fn notify_vq(&self) -> Result<&Virtqueue> {
        let vqs = self.vqs.as_ref().ok_or(ENODEV)?;
        // SAFETY: As above.
        Ok(unsafe { vqs.0[NOTIFY_VQ].as_ref() })
    }

    /// Hands notification buffer `i` to the device.
    fn post_notify_buf(&mut self, i: usize) -> Result {
        let buf: *mut [u8; NOTIFY_BUF] = &mut *self.notify_bufs[i];
        let mut sg: bindings::scatterlist = pin_init::zeroed();
        // SAFETY: The buffer is a live kmalloc allocation of `NOTIFY_BUF` bytes.
        unsafe { bindings::sg_init_one(&mut sg, buf.cast(), NOTIFY_BUF as u32) };
        let mut sgs = [&raw mut sg];
        // SAFETY: The buffer lives as long as the queue, nothing reads it until the device
        // returns it, and the token is its own address. The caller holds the queue mutex.
        unsafe { self.notify_vq()?.add_sgs_raw(&mut sgs, 0, buf.cast(), GFP_KERNEL) }
    }

    fn notify_buf_index(&self, token: NonNull<u8>) -> Option<usize> {
        self.notify_bufs.iter().position(|b| core::ptr::eq(b.as_ptr(), token.as_ptr()))
    }
}

/// One bound device: the index and the request queue. Shared between the virtio driver and
/// every mount of the file system.
#[pin_data]
pub(crate) struct Session {
    /// The export's tag, from the device's configuration space; a mount names it as its source.
    pub(crate) tag: KVec<u8>,
    pub(crate) index: Index,
    /// The data window and its DAX device, when the device offers a window.
    pub(crate) dax: Option<Dax>,
    /// Which chunks of which files are mapped where in the window.
    #[pin]
    pub(crate) maps: Mutex<Maps>,
    #[pin]
    queue: Mutex<Queue>,
    #[pin]
    drain: Work<Session>,
    /// Delivers host notifications. Separate from `drain`, which only completes requests and
    /// takes no lock a request sender may hold, so that a request made while delivering (an
    /// inode's last reference dropped by an event) or while holding the mount lock (reclaim)
    /// is still answered.
    #[pin]
    notify: Work<Session, 2>,
    /// Frees window space for page faults, which cannot reclaim themselves.
    #[pin]
    reclaim: Work<Session, 1>,
    /// The mount using this session, while there is one. Notifications are delivered to it
    /// under this lock, and the mount clears it under the same lock as it goes away.
    #[pin]
    pub(crate) mount: Mutex<Option<Mount>>,
    /// Files open in this guest, by inode number: the daemon holds handles for each.
    #[pin]
    pub(crate) open_files: Mutex<RBTree<u64, OpenFile>>,
}

impl_has_work! {
    impl HasWork<Self> for Session { self.drain }
    impl HasWork<Self, 1> for Session { self.reclaim }
    impl HasWork<Self, 2> for Session { self.notify }
}

/// What the waiter sleeps on, pinned on its stack for the life of one request.
#[pin_data]
struct Token {
    /// Bytes the device wrote into the writable buffers.
    len: Atomic<u32>,
    #[pin]
    done: Completion,
}

impl Session {
    pub(crate) fn new(tag: KVec<u8>, index: Index, dax: Option<Dax>, vqs: Virtqueues) -> Result<Arc<Self>> {
        if vqs.len() != 2 {
            return Err(EINVAL);
        }
        let mut notify_bufs = KVec::with_capacity(NOTIFY_BUFS, GFP_KERNEL)?;
        for _ in 0..NOTIFY_BUFS {
            notify_bufs.push(KBox::new([0u8; NOTIFY_BUF], GFP_KERNEL)?, GFP_KERNEL)?;
        }
        let window_len = dax.as_ref().map_or(0, |d| d.window().len());
        Arc::pin_init(
            try_pin_init!(Self {
                tag,
                index,
                dax,
                maps <- kernel::new_mutex!(Maps::new(window_len)),
                queue <- kernel::new_mutex!(Queue { vqs: Some(Vqs(vqs)), next_unique: 1, notify_bufs }),
                drain <- new_work!("sharedfs::drain"),
                reclaim <- new_work!("sharedfs::reclaim"),
                notify <- new_work!("sharedfs::notify"),
                mount <- kernel::new_mutex!(None),
                open_files <- kernel::new_mutex!(RBTree::new()),
            }),
            GFP_KERNEL,
        )
    }

    /// Posts every notification buffer. Called once the device is ready.
    pub(crate) fn start_notifications(&self) -> Result {
        let mut q = self.queue.lock();
        for i in 0..NOTIFY_BUFS {
            q.post_notify_buf(i)?;
        }
        q.notify_vq()?.kick();
        Ok(())
    }

    /// Gives the device's queues up ahead of a suspend or hibernation. Requests fail with
    /// `ENODEV` until [`Session::restore_queues`]; none are in flight, as every task is frozen
    /// before devices are.
    pub(crate) fn freeze_queues(&self) {
        self.queue.lock().vqs = None;
    }

    /// Takes freshly found queues after a resume and posts the notification buffers on them. The
    /// device on the other side is a new daemon with the same index, so request numbering
    /// simply continues.
    pub(crate) fn restore_queues(&self, vqs: Virtqueues) -> Result {
        if vqs.len() != 2 {
            return Err(EINVAL);
        }
        let mut q = self.queue.lock();
        q.vqs = Some(Vqs(vqs));
        for i in 0..NOTIFY_BUFS {
            q.post_notify_buf(i)?;
        }
        q.notify_vq()?.kick();
        Ok(())
    }

    fn deliver(&self, buf: &[u8]) {
        let Ok(n) = Notification::decode(buf) else {
            pr_warn!("dropping a malformed notification\n");
            return;
        };
        let mount = self.mount.lock();
        let Some(mount) = mount.as_ref() else { return };
        match n {
            Notification::Overflow => mount.on_overflow(),
            Notification::Event { mask, slot, parent, cookie, generation, name_len } => {
                let name = &buf[NAME_OFF..NAME_OFF + name_len];
                mount.on_event(&self.index, mask, slot, parent, cookie, generation, name);
            }
        }
    }

    /// Called from the virtqueue callback, in interrupt context: hand completions to the drain
    /// work item. Enqueueing while already queued is a no-op, which is the right coalescing.
    pub(crate) fn kick_drain(self: &Arc<Self>) {
        let _ = workqueue::system_highpri().enqueue::<Arc<Self>, 0>(self.clone());
        let _ = workqueue::system_highpri().enqueue::<Arc<Self>, 2>(self.clone());
    }

    /// Asks the reclaim worker for window space; repeated asks while it is queued coalesce.
    pub(crate) fn kick_reclaim(self: &Arc<Self>) {
        let _ = workqueue::system().enqueue::<Arc<Self>, 1>(self.clone());
    }

    /// Sends one request and waits for the response.
    ///
    /// `req` is the complete request with a zeroed `unique` field, which is filled in here, and
    /// `resp_len` bytes are provided for the response.
    ///
    /// Returns the response buffer and the number of bytes the host wrote.
    pub(crate) fn request(&self, req: &[u8], resp_len: usize) -> Result<(KVec<u8>, usize)> {
        if req.len() < HEADER_LEN || resp_len < HEADER_LEN {
            return Err(EINVAL);
        }
        // Buffers the device reads and writes must be kmalloc memory: the stack may be vmalloc.
        let mut reqbuf = KVec::with_capacity(req.len(), GFP_KERNEL)?;
        reqbuf.extend_from_slice(req, GFP_KERNEL)?;
        let mut respbuf = KVec::from_elem(0u8, resp_len, GFP_KERNEL)?;

        ::pin_init::stack_pin_init!(let token = pin_init!(Token {
            len: Atomic::new(0),
            done <- Completion::new(),
        }));

        let mut sg_req: bindings::scatterlist = pin_init::zeroed();
        let mut sg_resp: bindings::scatterlist = pin_init::zeroed();
        // SAFETY: The buffers are valid kmalloc allocations of the stated lengths.
        unsafe {
            bindings::sg_init_one(&mut sg_req, reqbuf.as_ptr().cast(), req.len() as u32);
            bindings::sg_init_one(&mut sg_resp, respbuf.as_mut_ptr().cast(), resp_len as u32);
        }
        let mut sgs = [&raw mut sg_req, &raw mut sg_resp];

        {
            let mut q = self.queue.lock();
            let unique = q.next_unique;
            q.next_unique += 1;
            reqbuf[8..16].copy_from_slice(&unique.to_le_bytes());

            let token_ptr: *const Token = &*token;
            // SAFETY: Each scatterlist is terminated, its memory is owned by this frame or by the
            // caller for the duration of the request, and `token` is pinned on this stack frame
            // until the device returns it. The mutex serialises vring access.
            unsafe {
                q.vq()?.add_sgs_raw(
                    &mut sgs,
                    1,
                    token_ptr.cast_mut().cast(),
                    GFP_KERNEL,
                )?
            };
            q.vq()?.kick();
        }

        token.done.wait_for_completion();
        let written = token.len.load(Relaxed) as usize;

        if written < HEADER_LEN {
            return Err(EIO);
        }
        let total = u32::from_le_bytes(respbuf[0..4].try_into().unwrap()) as usize;
        let error = i32::from_le_bytes(respbuf[4..8].try_into().unwrap());
        if total != written {
            return Err(EIO);
        }
        if error != 0 {
            return Err(Error::from_errno(-error));
        }
        Ok((respbuf, written))
    }
}

impl WorkItem<1> for Session {
    type Pointer = Arc<Session>;

    fn run(this: Arc<Session>) {
        this.reclaim_for_faults();
    }
}

impl WorkItem for Session {
    type Pointer = Arc<Session>;

    /// Completes finished requests and wakes their waiters. Runs in process context and takes
    /// only the queue lock, which no one holds while waiting.
    fn run(this: Arc<Session>) {
        let q = this.queue.lock();
        let Ok(vq) = q.vq() else { return };
        while let Some((token, len)) = vq.get_buf() {
            let token: NonNull<Token> = token.cast();
            // SAFETY: The pointer is one we passed to `add_sgs_raw`, and its owner is still
            // waiting on `done`, so the token is alive.
            let token = unsafe { token.as_ref() };
            token.len.store(len, Relaxed);
            token.done.complete_all();
        }
    }
}

impl WorkItem<2> for Session {
    type Pointer = Arc<Session>;

    /// Delivers the notifications the host filled in.
    fn run(this: Arc<Session>) {
        // Each notification is copied out and its buffer reposted before the mount acts on it,
        // so a slow fsnotify consumer never starves the device of buffers.
        loop {
            let mut copy = [0u8; NOTIFY_BUF];
            let len;
            {
                let mut q = this.queue.lock();
                let Some((token, n)) = q.notify_vq().ok().and_then(|vq| vq.get_buf()) else { break };
                let Some(i) = q.notify_buf_index(token) else { continue };
                len = (n as usize).min(NOTIFY_BUF);
                copy[..len].copy_from_slice(&q.notify_bufs[i][..len]);
                if q.post_notify_buf(i).is_ok() {
                    let _ = q.notify_vq().map(|vq| vq.kick());
                }
            }
            this.deliver(&copy[..len]);
        }
    }
}

/// Builds request bodies. Names are at most `NAME_MAX` and symlink targets at most `PATH_MAX`
/// by the time they reach the file system, so the longest request is bounded and the buffer
/// lives on the stack.
pub(crate) struct Req {
    buf: [u8; REQ_MAX],
    len: usize,
}

impl Req {
    pub(crate) fn new(opcode: u32) -> Self {
        let mut r = Self {
            buf: [0; REQ_MAX],
            len: HEADER_LEN,
        };
        r.buf[4..8].copy_from_slice(&opcode.to_le_bytes());
        r
    }

    fn put(mut self, v: &[u8]) -> Self {
        // Over-long input is a caller bug: the VFS bounds names before any operation.
        let end = self.len + v.len();
        self.buf[self.len..end].copy_from_slice(v);
        self.len = end;
        self
    }

    pub(crate) fn u16(self, v: u16) -> Self {
        self.put(&v.to_le_bytes())
    }

    pub(crate) fn u32(self, v: u32) -> Self {
        self.put(&v.to_le_bytes())
    }

    pub(crate) fn u64(self, v: u64) -> Self {
        self.put(&v.to_le_bytes())
    }

    pub(crate) fn i64(self, v: i64) -> Self {
        self.put(&v.to_le_bytes())
    }

    /// A `u16`-length-prefixed name.
    pub(crate) fn bytes(self, v: &[u8]) -> Self {
        self.u16(v.len() as u16).put(v)
    }

    /// A `u32`-length-prefixed blob.
    pub(crate) fn blob(self, v: &[u8]) -> Self {
        self.u32(v.len() as u32).put(v)
    }

    pub(crate) fn opcode(&self) -> u32 {
        u32::from_le_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]])
    }

    pub(crate) fn as_bytes(&mut self) -> &[u8] {
        let len = self.len as u32;
        self.buf[0..4].copy_from_slice(&len.to_le_bytes());
        &self.buf[..self.len]
    }
}

/// Reads little-endian fields out of a response body.
pub(crate) struct Resp<'a>(pub(crate) &'a [u8]);

impl Resp<'_> {
    pub(crate) fn u16(&mut self) -> Result<u16> {
        let (head, tail) = self.0.split_at_checked(2).ok_or(EIO)?;
        self.0 = tail;
        Ok(u16::from_le_bytes(head.try_into().unwrap()))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        let (head, tail) = self.0.split_at_checked(4).ok_or(EIO)?;
        self.0 = tail;
        Ok(u32::from_le_bytes(head.try_into().unwrap()))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        let (head, tail) = self.0.split_at_checked(8).ok_or(EIO)?;
        self.0 = tail;
        Ok(u64::from_le_bytes(head.try_into().unwrap()))
    }

    /// A `u32`-length-prefixed blob.
    pub(crate) fn blob(&mut self) -> Result<&[u8]> {
        let n = self.u32()? as usize;
        let (head, tail) = self.0.split_at_checked(n).ok_or(EIO)?;
        self.0 = tail;
        Ok(head)
    }
}
