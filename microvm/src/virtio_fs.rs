// Virtio-fs device — bridges virtio MMIO transport to fuse-backend-rs.
//
// Uses ioeventfd so the guest's QueueNotify writes go directly to the
// worker thread without a vCPU exit. The worker owns the entire data path:
//   ioeventfd kick → pop descriptors → S3 call → write response →
//   add_used → atomic interrupt_status → irqfd → KVM delivers interrupt
//
// The vCPU only handles MMIO for device setup. Zero exits on the data path.

use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::api::filesystem::FileSystem;
use fuse_backend_rs::transport::{Reader, Writer, VirtioFsWriter};
use kvm_ioctls::{IoEventAddress, NoDatamatch, VmFd};
use virtio_queue::{Queue, QueueT};
use vmm_sys_util::eventfd::EventFd;
use vm_memory::GuestMemoryMmap;

use crate::virtio_mmio::VirtioMmioDevice;

const VIRTIO_DEV_FS: u32 = 26;
const VENDOR_ID: u32 = 0x554D_4551;
const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;

const REQUEST_QUEUE: usize = 1;

/// Holds the filesystem server until the worker starts.
/// Box<dyn Any + Send> would work but we use a trait object to keep the type.
trait FuseServer: Send + 'static {
    fn handle(&self, reader: Reader<'static>, writer: Writer<'static>);
}

struct FuseServerImpl<F: FileSystem + Sync + Send + 'static> {
    server: Server<F>,
}

impl<F: FileSystem + Sync + Send + 'static> FuseServer for FuseServerImpl<F> {
    fn handle(&self, reader: Reader<'static>, writer: Writer<'static>) {
        let _ = self.server.handle_message(reader, writer, None, None);
    }
}

pub struct VirtioFs {
    pub mmio: VirtioMmioDevice,
    queue: Option<Queue>,
    tag: String,
    kick_fd: EventFd,
    mmio_base: u64,
    server: Option<Box<dyn FuseServer>>,
    _worker: Option<thread::JoinHandle<()>>,
}

impl VirtioFs {
    pub fn new<F: FileSystem + Sync + Send + 'static>(
        fs: F,
        tag: &str,
        irq: u32,
        mmio_base: u64,
    ) -> Self {
        let mmio = VirtioMmioDevice::new(
            VIRTIO_DEV_FS,
            VENDOR_ID,
            0,
            irq,
            NUM_QUEUES,
            QUEUE_SIZE,
        );

        let kick_fd = EventFd::new(0).expect("eventfd");

        Self {
            mmio,
            queue: Some(Queue::new(QUEUE_SIZE).unwrap()),
            tag: tag.to_string(),
            kick_fd,
            mmio_base,
            server: Some(Box::new(FuseServerImpl { server: Server::new(fs) })),
            _worker: None,
        }
    }

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
    /// After this call, the worker owns the queue and processes FUSE
    /// requests entirely independently of the vCPU thread.
    pub fn start_worker(&mut self, vm: &VmFd, memory: GuestMemoryMmap) {
        // Only start once
        if self._worker.is_some() || self.queue.is_none() { return; }

        // Register ioeventfd: QueueNotify at offset 0x050
        let notify_addr = self.mmio_base + 0x050;
        vm.register_ioevent(&self.kick_fd, &IoEventAddress::Mmio(notify_addr), NoDatamatch)
            .expect("register ioeventfd for virtio-fs");

        let mut queue = self.queue.take().unwrap();
        let server = self.server.take().expect("server already taken");
        let kick = self.kick_fd.try_clone().expect("clone kick_fd");
        let irqfd = self.mmio.irqfd.try_clone().expect("clone irqfd");
        let interrupt_status = &self.mmio.interrupt_status as *const AtomicU32 as usize;

        self._worker = Some(thread::spawn(move || {
            loop {
                // Block until guest writes to QueueNotify
                kick.read().ok();

                // Process all pending FUSE requests
                let mut processed = false;
                while let Some(desc_chain) = queue.pop_descriptor_chain(&memory) {
                    let head_index = desc_chain.head_index();

                    let reader = match Reader::from_descriptor_chain(&memory, desc_chain.clone()) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    let writer = match VirtioFsWriter::new(&memory, desc_chain) {
                        Ok(w) => Writer::VirtioFs(w),
                        Err(_) => continue,
                    };

                    // SAFETY: guest memory is Arc-wrapped, valid for VM lifetime
                    let reader: Reader<'static> = unsafe { std::mem::transmute(reader) };
                    let writer: Writer<'static> = unsafe { std::mem::transmute(writer) };

                    server.handle(reader, writer);
                    queue.add_used(&memory, head_index, 0).ok();
                    processed = true;
                }

                if processed {
                    unsafe {
                        let status = &*(interrupt_status as *const AtomicU32);
                        status.fetch_or(0x01, Ordering::Relaxed);
                    }
                    irqfd.write(1).ok();
                }
            }
        }));
    }
}
