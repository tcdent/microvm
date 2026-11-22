//! virtio queue worker.
//!
//! Two queues. On the request queue each descriptor chain carries a
//! request in its readable descriptors and receives the response in its
//! writable ones. On the notification queue the guest posts empty
//! writable buffers and the daemon fills one per notification. The
//! worker parks on the kick eventfd and the host-side change watcher,
//! drains in one pass, and fires one interrupt per batch. The MMIO
//! register side lives in the VMM; this module owns everything from the
//! virtqueue inward.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

use crate::proto::{Notification, Request, Response};
use crate::server::Server;

/// virtio device ID. Not assigned by the spec; the guest driver matches it.
pub const DEVICE_ID: u32 = 0x5346;
/// The export's tag, NUL-padded at the start of the device configuration
/// space as virtio-fs lays it out. A guest mount names the tag as its
/// source. One device per export; a VM may carry several.
pub const TAG_LEN: usize = 36;
pub const QUEUE_SIZE: u16 = 128;
pub const NUM_QUEUES: usize = 2;
pub const REQUEST_QUEUE: usize = 0;
pub const NOTIFY_QUEUE: usize = 1;
/// Suggested guest-physical base for the first export's index region:
/// 24 TiB, above the data windows. Shared memory region 0. Further exports
/// follow at index-length strides.
pub const SHM_BASE: u64 = 0x1800_0000_0000;
/// Default index size: room for about 2.7 million entries at 384 bytes
/// each, enough for a tree like a Servo checkout plus its build output.
/// Only pages that hold entries use memory, so an index mostly empty
/// costs little more than its hash table (32 MiB).
pub const INDEX_LEN: usize = 1 << 30;
/// Guest-physical base of the first export's data window: 16 TiB, far
/// above any RAM and well inside 48-bit physical addressing. Shared
/// memory region 1. Further exports follow at window-length strides.
pub const WINDOW_BASE: u64 = 0x1000_0000_0000;
/// Default window size. The guest keeps a `struct page` per window page,
/// 64 bytes per 4 KiB, so 8 GiB of window costs the guest 128 MiB.
pub const WINDOW_LEN: usize = 8 << 30;

/// Everything the worker thread owns.
pub struct Session {
    pub server: Server,
    pub memory: Arc<GuestMemoryMmap>,
    /// Shared with the VMM's device, which reconfigures or resets the rings when the driver
    /// does; the worker locks them per pass.
    pub queue: Arc<Mutex<Queue>>,
    pub notify_queue: Arc<Mutex<Queue>>,
    /// Signalled by KVM through ioeventfd on QueueNotify.
    pub kick_fd: EventFd,
    pub irqfd: EventFd,
    pub interrupt_status: Arc<AtomicU32>,
    /// Set by the VMM when the driver resets the device. A guest that resumes from a
    /// hibernation image resets the device before it reposts its buffers, and the kernel that
    /// booted to load the image already took the reconcile notifications: they go out again.
    pub replay: Arc<AtomicBool>,
    pub thread_name: String,
}

pub fn serve(session: Session) -> JoinHandle<()> {
    let Session { mut server, memory, queue, notify_queue, kick_fd, irqfd, interrupt_status, replay, thread_name } =
        session;
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let mut outbox = Outbox::default();
            // What changed on the host while no daemon watched, found when the index was
            // adopted. A guest resuming from hibernation needs these; a fresh one ignores them.
            outbox.push(server.take_pending());
            loop {
                // SAFETY: `kick_fd` owns its descriptor for the life of this loop.
                let kick = unsafe { BorrowedFd::borrow_raw(kick_fd.as_raw_fd()) };
                let mut fds = [
                    PollFd::new(kick, PollFlags::POLLIN),
                    PollFd::new(server.watch_fd(), PollFlags::POLLIN),
                ];
                if poll(&mut fds, PollTimeout::NONE).is_err() {
                    continue;
                }
                let kicked = fds[0].any().unwrap_or(false);
                let changed = fds[1].any().unwrap_or(false);
                let mut used = 0;
                if changed {
                    outbox.push(server.pump_watch());
                }
                if replay.swap(false, Ordering::AcqRel) {
                    outbox = Outbox::default();
                    outbox.push(server.take_pending());
                }
                if kicked {
                    kick_fd.read().ok();
                    used += drain(&mut server, &mut queue.lock().unwrap(), &memory);
                }
                // A kick on either queue, or a change, may have made room or work.
                used += outbox.flush(&mut notify_queue.lock().unwrap(), &memory);
                if used > 0 {
                    interrupt_status.fetch_or(0x01, Ordering::Relaxed);
                    irqfd.write(1).ok();
                }
            }
        })
        .expect("spawn sharedfs worker")
}

/// Service every pending chain once. Returns how many completed.
/// Notifications waiting for a guest buffer.
///
/// The guest reposts buffers as it consumes them, so a short wait is normal and the backlog is
/// kept. Past [`Outbox::BACKLOG`] it is dropped and replaced by one OVERFLOW, which tells the
/// guest to forget what it cached; the index is still right.
#[derive(Default)]
pub struct Outbox {
    pending: VecDeque<Vec<u8>>,
    overflowed: bool,
}

impl Outbox {
    /// Most notifications held while the guest has no buffer posted.
    pub const BACKLOG: usize = 1024;

    pub fn push(&mut self, notifications: Vec<Notification>) {
        if self.overflowed {
            return;
        }
        for n in notifications {
            self.pending.push_back(n.encode());
        }
        if self.pending.len() > Self::BACKLOG {
            self.pending.clear();
            self.overflowed = true;
        }
    }

    /// Hands as many pending notifications to the guest as it has buffers for. Returns the
    /// number of buffers used.
    pub fn flush(&mut self, queue: &mut Queue, mem: &GuestMemoryMmap) -> usize {
        let mut used = 0;
        loop {
            let next = if self.overflowed {
                Notification::Overflow.encode()
            } else if let Some(n) = self.pending.front() {
                n.clone()
            } else {
                return used;
            };
            if !queue.is_valid(mem) {
                return used;
            }
            let Some(chain) = queue.pop_descriptor_chain(mem) else {
                return used;
            };
            let head = chain.head_index();
            let mut written = 0usize;
            for d in chain {
                if !d.is_write_only() || written >= next.len() {
                    continue;
                }
                let n = (d.len() as usize).min(next.len() - written);
                if mem.write_slice(&next[written..written + n], d.addr()).is_err() {
                    break;
                }
                written += n;
            }
            queue.add_used(mem, head, written as u32).ok();
            used += 1;
            if self.overflowed {
                self.overflowed = false;
            } else {
                self.pending.pop_front();
            }
        }
    }
}

pub fn drain(server: &mut Server, queue: &mut Queue, mem: &GuestMemoryMmap) -> usize {
    let mut n = 0;
    while let Some(chain) = queue.pop_descriptor_chain(mem) {
        server.forget_pending();
        let head = chain.head_index();
        let written = handle_chain(server, mem, chain);
        queue.add_used(mem, head, written).ok();
        n += 1;
    }
    n
}

/// Gather the request, run it, scatter the response. Returns bytes written.
fn handle_chain(server: &mut Server, mem: &GuestMemoryMmap, chain: DescriptorChain<&GuestMemoryMmap>) -> u32 {
    let mut req = Vec::new();
    let mut out: Vec<(GuestAddress, usize)> = Vec::new();
    for d in chain {
        if d.is_write_only() {
            out.push((d.addr(), d.len() as usize));
        } else {
            let start = req.len();
            req.resize(start + d.len() as usize, 0);
            if mem.read_slice(&mut req[start..], d.addr()).is_err() {
                return 0;
            }
        }
    }
    let result = match Request::decode(&req) {
        Ok((unique, r)) => {
            // Data-path requests never fail in normal operation; a failure here is what the
            // guest turns into an I/O error or a SIGBUS, so leave a trace of it.
            let data_path = matches!(r, Request::Map { .. } | Request::Extend { .. } | Request::OpenRead { .. } | Request::OpenWrite { .. } | Request::Fsync { .. });
            let shown = if data_path { Some(format!("{r:?}")) } else { None };
            let res = server.handle(r);
            if let (Some(req), Err(e)) = (&shown, &res) {
                eprintln!("sharedfs: {req} failed: errno {e}");
            }
            (unique, res)
        }
        Err(_) => (unique_hint(&req), Err(libc::EINVAL)),
    };
    let bytes = Response::encode(result.0, &result.1);
    let mut off = 0;
    for (addr, len) in out {
        if off >= bytes.len() {
            break;
        }
        let n = len.min(bytes.len() - off);
        if mem.write_slice(&bytes[off..off + n], addr).is_err() {
            break;
        }
        off += n;
    }
    off as u32
}

/// The `unique` field of a request whose body did not parse, so the
/// guest can still match the error to its waiter.
fn unique_hint(req: &[u8]) -> u64 {
    req.get(8..16).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
}
