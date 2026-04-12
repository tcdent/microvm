// Virtio-fs device — MMIO transport wrapper around the worker in
// s3-rootfs::worker. Keeps only the setup/teardown ceremony here:
//
//   - virtio MMIO register plumbing (via VirtioMmioDevice)
//   - ioeventfd registration so QueueNotify writes skip the vCPU exit
//   - ownership of the Queue and kick EventFd until the worker starts
//
// Once the guest writes DRIVER_OK we hand all of that to the worker via
// `FsSession` and the worker takes over the FUSE data path entirely.
// See research/11-s3-rootfs-refactor-plan.md for the split.

use std::sync::Arc;
use std::thread;

use kvm_ioctls::{IoEventAddress, NoDatamatch, VmFd};
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;

use s3_rootfs::S3Fs;
use virtio_fs_transport::{FsSession, serve};

use crate::virtio_mmio::VirtioMmioDevice;

const VIRTIO_DEV_FS: u32 = 26;
const VENDOR_ID: u32 = 0x554D_4551;
const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;

const REQUEST_QUEUE: usize = 1;

/// Virtio-fs MMIO device.
///
/// Owns the `Arc<S3Fs>` until `start_worker` hands it to the worker
/// thread via `FsSession`. After handoff, the filesystem is shared
/// between the worker (which services FUSE requests) and any future
/// orchestrator reference (which can invalidate the cache, etc.).
pub struct VirtioFs {
    pub mmio: VirtioMmioDevice,
    queue: Option<Queue>,
    tag: String,
    kick_fd: EventFd,
    mmio_base: u64,
    /// Filesystem. Held in `Option` because `start_worker` consumes
    /// this slot — once the worker owns its Arc, the device struct
    /// no longer needs a reference.
    fs: Option<Arc<S3Fs>>,
    _worker: Option<thread::JoinHandle<()>>,
}

impl VirtioFs {
    pub fn new(fs: Arc<S3Fs>, tag: &str, irq: u32, mmio_base: u64) -> Self {
        let mmio = VirtioMmioDevice::new(
            VIRTIO_DEV_FS,
            VENDOR_ID,
            0,
            irq,
            NUM_QUEUES,
            QUEUE_SIZE,
        );

        // Blocking eventfd so the worker can park on `read()` with
        // zero CPU while idle. KVM signals via ioeventfd on every
        // QueueNotify write by the guest.
        let kick_fd = EventFd::new(0).expect("eventfd");

        Self {
            mmio,
            queue: Some(Queue::new(QUEUE_SIZE).unwrap()),
            tag: tag.to_string(),
            kick_fd,
            mmio_base,
            fs: Some(fs),
            _worker: None,
        }
    }

    /// Config space: returns the virtio-fs mount tag (device-specific).
    pub fn config_read(&self, offset: u64) -> u32 {
        let tag_bytes = self.tag.as_bytes();
        match offset {
            0..=35 => {
                let off = offset as usize;
                let mut buf = [0u8; 4];
                for i in 0..4 {
                    if off + i < tag_bytes.len() {
                        buf[i] = tag_bytes[off + i];
                    }
                }
                u32::from_le_bytes(buf)
            }
            36 => 1,
            _ => 0,
        }
    }

    pub fn activate_queue(&mut self, queue_idx: usize) {
        if queue_idx != REQUEST_QUEUE { return; }
        if let Some(ref mut q) = self.queue {
            if let Some(qc) = self.mmio.queues.get(queue_idx) {
                q.set_size(qc.size);
                q.set_desc_table_address(
                    Some((qc.desc_addr & 0xFFFF_FFFF) as u32),
                    Some((qc.desc_addr >> 32) as u32),
                );
                q.set_avail_ring_address(
                    Some((qc.avail_addr & 0xFFFF_FFFF) as u32),
                    Some((qc.avail_addr >> 32) as u32),
                );
                q.set_used_ring_address(
                    Some((qc.used_addr & 0xFFFF_FFFF) as u32),
                    Some((qc.used_addr >> 32) as u32),
                );
                q.set_ready(true);
            }
        }
    }

    /// Register ioeventfd with KVM and spawn the worker thread.
    ///
    /// After this call, the worker owns the queue, the filesystem
    /// reference, and the kick/irqfd clones. The vCPU thread should
    /// never touch the request queue or the FUSE server again —
    /// everything on the data path runs in the worker.
    pub fn start_worker(&mut self, vm: &VmFd, memory: Arc<GuestMemoryMmap>) {
        // Only start once.
        if self._worker.is_some() || self.queue.is_none() {
            return;
        }

        // Register ioeventfd: QueueNotify is at offset 0x050 in the
        // virtio MMIO spec. With this, guest writes to that register
        // never exit the vCPU — KVM signals kick_fd directly.
        let notify_addr = self.mmio_base + 0x050;
        vm.register_ioevent(
            &self.kick_fd,
            &IoEventAddress::Mmio(notify_addr),
            NoDatamatch,
        )
        .expect("register ioeventfd for virtio-fs");

        // Build the session. Ownership transfers from the device to
        // the worker thread: queue, kick_fd clone, irqfd clone, and
        // the fs Arc all move across. The `interrupt_status` Arc is
        // cloned (not moved) because the mmio device keeps one to
        // serve InterruptStatus reads from the vCPU.
        let session = FsSession {
            fs: self.fs.take().expect("fs already taken"),
            memory,
            queue: self.queue.take().expect("queue already taken"),
            kick_fd: self.kick_fd.try_clone().expect("clone kick_fd"),
            irqfd: self.mmio.irqfd.try_clone().expect("clone irqfd"),
            interrupt_status: self.mmio.interrupt_status.clone(),
            thread_name: "vm-fuse".into(),
        };

        self._worker = Some(serve(session));
    }
}
