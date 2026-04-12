//! virtio-fs MMIO transport worker.
//!
//! The guest talks FUSE-over-virtio-fs at the protocol layer; this
//! crate implements the host side of that transport as a blocking
//! worker loop. It's deliberately narrow — a single function
//! ([`serve`]) that services one virtqueue on one thread.
//!
//! # What this crate is
//!
//! The virtio-fs transport: descriptor-chain pop, Reader/Writer
//! construction over guest memory, dispatch into a FUSE `Server`,
//! interrupt injection on completion.
//!
//! # What this crate is not
//!
//! - **Not a filesystem.** It's generic over any `F: FileSystem`; the
//!   filesystem logic lives in a separate crate (e.g. `s3-rootfs`).
//! - **Not a VMM.** It doesn't know about KVM, ioeventfd registration,
//!   or MMIO register decoding. Those live in the VMM. This crate just
//!   receives a ready-to-run `FsSession` and drives the loop.
//!
//! # Architecture
//!
//! The worker is woken by an `EventFd` (the "kick" fd) that KVM signals
//! via `ioeventfd` whenever the guest writes to the QueueNotify MMIO
//! register. This is a zero-exit fast path: the vCPU never transfers
//! to userspace for FUSE data — KVM delivers the kick directly.
//!
//! Once woken, the worker drains **all** pending descriptors from the
//! request queue in a single pass, then fires a **single** interrupt
//! via irqfd for the whole batch. This coalesces both ends of the
//! transport: one kick → many requests processed → one completion
//! interrupt.
//!
//! # Ownership and thread safety
//!
//! No IPC, no serialization. Workers and the rest of the process
//! (VMM, future orchestrator) coordinate via shared memory:
//! `Arc<F>` for the filesystem, `Arc<GuestMemoryMmap>` for guest RAM,
//! `Arc<AtomicU32>` for the virtio interrupt-status word. `EventFd`
//! handles signalling. Everything runs in one address space.
//!
//! # Lifetime contract
//!
//! The worker thread holds its `FsSession` for its entire lifetime, so
//! `GuestMemoryMmap` and `Arc<F>` outlive every `Reader`/`Writer` built
//! from them — which is why the `'static` transmute in the hot loop
//! is sound.
//!
//! # TODO: tests
//!
//! This crate has zero unit tests today. Because it's generic over
//! `F: FileSystem`, we can test the transport with a mock filesystem
//! and an in-memory queue — no RustFS or VM required. Minimum
//! coverage to add before OSS release:
//!
//! 1. Round-trip: mock FS, synthetic descriptor chain, kick_fd write,
//!    verify the mock was invoked and irqfd was fired exactly once.
//! 2. Batching: push N chains before kicking, verify one irqfd fire
//!    for N completions (exercises the §3 "single irqfd per batch"
//!    optimization).
//! 3. Spurious wakeup: kick with no pending descriptors, verify zero
//!    irqfd writes (no interrupt on empty drain).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::{self, JoinHandle};

use fuse_backend_rs::api::filesystem::FileSystem;
use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::transport::{Reader, VirtioFsWriter, Writer};
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;

/// Everything a virtio-fs worker thread needs to run.
///
/// Built by the VMM and handed to [`serve`]. The worker takes ownership;
/// anything a longer-lived entity (VMM, orchestrator) needs to keep
/// access to (`fs`, `interrupt_status`) should be cloned — those are
/// `Arc`, so cloning is just a refcount bump.
pub struct FsSession<F: FileSystem + Send + Sync + 'static> {
    /// The filesystem. Generic so the transport can serve any FUSE
    /// backend — not tied to any particular storage implementation.
    /// `fuse-backend-rs` provides `impl<FS: FileSystem> FileSystem for Arc<FS>`,
    /// so `Server::new` accepts `Arc<F>` directly.
    pub fs: Arc<F>,

    /// Guest memory. Reader/Writer borrow from this; it must outlive
    /// every request, which is guaranteed because the worker thread
    /// holds this `Arc` for its whole lifetime.
    pub memory: Arc<GuestMemoryMmap>,

    /// The request queue. Owned exclusively by the worker after
    /// handoff — the VMM thread never touches it again.
    pub queue: Queue,

    /// Wakeup signal: KVM writes here via ioeventfd when the guest
    /// kicks the QueueNotify register. Blocking read parks the thread
    /// with zero CPU while idle.
    pub kick_fd: EventFd,

    /// Interrupt injection: writing to this fd causes KVM to deliver
    /// a virtio interrupt to the guest, entirely in-kernel. No vCPU
    /// exit, no syscall round-trip back to userspace.
    pub irqfd: EventFd,

    /// Shared with the MMIO transport so guest reads of the
    /// InterruptStatus register see the pending-interrupt bit we
    /// set here before firing irqfd.
    pub interrupt_status: Arc<AtomicU32>,

    /// Thread name for observability (top/htop/perf).
    pub thread_name: String,
}

/// Spawn the worker thread and return its join handle.
///
/// The returned handle is typically held by the device struct and
/// joined on shutdown. The thread runs until the kick fd is closed
/// or the process exits.
///
/// The filesystem `F` must be `Send + Sync + 'static` because the
/// `Arc<F>` moves into the worker thread and is shared with any
/// other holders.
pub fn serve<F>(session: FsSession<F>) -> JoinHandle<()>
where
    F: FileSystem + Send + Sync + 'static,
{
    let FsSession {
        fs,
        memory,
        mut queue,
        kick_fd,
        irqfd,
        interrupt_status,
        thread_name,
    } = session;

    // `Server::new` takes the filesystem by value. Since fuse-backend-rs
    // has `impl<FS: FileSystem> FileSystem for Arc<FS>`, we just pass
    // the Arc. This is O(1) refcount bump, not a deep clone. Runs once
    // per worker lifetime, not per request.
    let server = Server::new(fs);

    thread::Builder::new()
        .name(thread_name)
        .spawn(move || worker_loop(&server, &mut queue, &memory, &kick_fd, &irqfd, &interrupt_status))
        .expect("failed to spawn virtio-fs worker thread")
}

/// The hot loop.
///
/// Kept as a free function (not a method on `FsSession`) so the borrow
/// checker doesn't need to reason about a struct holding disjoint
/// fields — each parameter is a clean reference.
///
/// Performance notes (do not regress these without benchmarks):
/// - `kick_fd.read()` blocks with zero CPU while idle.
/// - `pop_descriptor_chain` in a `while let` drains the queue in one
///   pass; a single kick processes many requests.
/// - `irqfd.write(1)` fires once per batch, not once per request.
/// - `interrupt_status.fetch_or(…, Relaxed)` is the cheapest ordering
///   that still publishes the bit before the irqfd write — the irqfd
///   write itself is the synchronization point with the guest.
///
/// TODO(observability): `.ok()` silently discards errors from
/// `kick_fd.read`, `queue.add_used`, and `irqfd.write`. A persistent
/// failure on any of these means the guest is stuck (no interrupts
/// delivered, no completions recorded). Add rate-limited error
/// logging here — once per N seconds is enough to surface a broken
/// device in production without drowning normal logs.
fn worker_loop<F>(
    server: &Server<Arc<F>>,
    queue: &mut Queue,
    memory: &GuestMemoryMmap,
    kick_fd: &EventFd,
    irqfd: &EventFd,
    interrupt_status: &AtomicU32,
) where
    F: FileSystem + Send + Sync + 'static,
{
    loop {
        // Park until the guest kicks. EventFd::read is a syscall that
        // blocks the thread with no spinning.
        kick_fd.read().ok();

        // Drain the request queue fully. `processed` tracks whether we
        // did any work at all — on a spurious wakeup (rare) we skip
        // the interrupt.
        let mut processed = false;
        while let Some(desc_chain) = queue.pop_descriptor_chain(memory) {
            let head_index = desc_chain.head_index();

            // Build Reader/Writer from the descriptor chain. These
            // borrow from `memory` with a lifetime tied to `desc_chain`.
            // `continue` on error skips malformed descriptors rather
            // than aborting the whole drain.
            let reader = match Reader::from_descriptor_chain(memory, desc_chain.clone()) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let writer = match VirtioFsWriter::new(memory, desc_chain) {
                Ok(w) => Writer::VirtioFs(w),
                Err(_) => continue,
            };

            // SAFETY: `Server::handle_message` requires `Reader<'static>`
            // and `Writer<'static>`. Our reader/writer actually borrow
            // from `memory`, which is held for the whole thread lifetime
            // via the captured `Arc<GuestMemoryMmap>`. `handle_message`
            // completes synchronously before we continue the loop, so
            // the borrows never outlive the memory. Same pattern as
            // virtiofsd for the same API constraint.
            let reader: Reader<'static> = unsafe { std::mem::transmute(reader) };
            let writer: Writer<'static> = unsafe { std::mem::transmute(writer) };

            // Dispatch into the filesystem. Returns void — any IO
            // errors are marshalled back to the guest through the
            // writer, not Rust's Result.
            let _ = server.handle_message(reader, writer, None, None);

            // Mark the descriptor complete. `len=0` is the virtio-fs
            // convention: the response payload was written through
            // the Writer above, which updated the used-ring length
            // internally.
            queue.add_used(memory, head_index, 0).ok();
            processed = true;
        }

        if processed {
            // Set the pending-interrupt bit BEFORE firing irqfd so
            // the guest sees it when it reads InterruptStatus during
            // IRQ handling. `Relaxed` is sufficient: the irqfd write
            // is the synchronization point (KVM's irqfd handling
            // establishes a happens-before with the guest's IRQ entry).
            interrupt_status.fetch_or(0x01, Ordering::Relaxed);
            irqfd.write(1).ok();
        }
    }
}
