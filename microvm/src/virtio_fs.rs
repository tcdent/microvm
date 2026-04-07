// Virtio-fs device — bridges virtio MMIO transport to fuse-backend-rs.
//
// FUSE processing runs on a dedicated worker thread. The worker signals
// completion via an eventfd. The vCPU thread polls this fd between exits
// to drain completions and inject IRQs.
//
// Flow:
//   vCPU exit (queue notify) → dispatch_requests() → channel → worker
//   worker → S3 call → write response → completion channel + eventfd
//   vCPU exit (any) → process_completions() → update used ring → IRQ

use std::os::fd::{AsRawFd, RawFd};
use std::sync::Mutex;
use std::thread;

use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::api::filesystem::FileSystem;
use fuse_backend_rs::transport::{Reader, Writer, VirtioFsWriter};
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;

use crate::virtio_mmio::VirtioMmioDevice;

const VIRTIO_DEV_FS: u32 = 26;
const VENDOR_ID: u32 = 0x554D_4551;
const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;

const HIPRIO_QUEUE: usize = 0;
const REQUEST_QUEUE: usize = 1;

struct FuseRequest {
    head_index: u16,
    reader: Reader<'static>,
    writer: Writer<'static>,
}

unsafe impl Send for FuseRequest {}

struct FuseCompletion {
    head_index: u16,
}

pub struct VirtioFs {
    pub mmio: VirtioMmioDevice,
    queue: Mutex<Queue>,
    tag: String,

    tx: std::sync::mpsc::Sender<FuseRequest>,
    rx_completions: std::sync::mpsc::Receiver<FuseCompletion>,

    /// Eventfd that the worker writes to on completion.
    /// Poll this fd to know when to drain completions.
    completion_eventfd: RawFd,

    _worker: thread::JoinHandle<()>,
}

impl VirtioFs {
    pub fn new<F: FileSystem + Sync + Send + 'static>(fs: F, tag: &str, irq: u32) -> Self {
        let mmio = VirtioMmioDevice::new(
            VIRTIO_DEV_FS,
            VENDOR_ID,
            0,
            irq,
            NUM_QUEUES,
            QUEUE_SIZE,
        );

        let queue = Mutex::new(Queue::new(QUEUE_SIZE).unwrap());

        let (tx, rx) = std::sync::mpsc::channel::<FuseRequest>();
        let (tx_completions, rx_completions) = std::sync::mpsc::channel::<FuseCompletion>();

        // Eventfd for signaling completions to the vCPU thread
        let efd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(efd >= 0, "Failed to create eventfd");

        let server = Server::new(fs);
        let worker_efd = efd;

        let worker = thread::spawn(move || {
            for req in rx {
                let _ = server.handle_message(req.reader, req.writer, None, None);
                tx_completions.send(FuseCompletion {
                    head_index: req.head_index,
                }).ok();
                // Signal the vCPU thread that a completion is ready
                let val: u64 = 1;
                unsafe {
                    libc::write(worker_efd, &val as *const u64 as *const libc::c_void, 8);
                }
            }
        });

        Self {
            mmio,
            queue,
            tag: tag.to_string(),
            tx,
            rx_completions,
            completion_eventfd: efd,
            _worker: worker,
        }
    }

    /// Fd that becomes readable when FUSE completions are available.
    /// Register with epoll or poll between vCPU exits.
    pub fn completion_fd(&self) -> RawFd {
        self.completion_eventfd
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

    pub fn activate_queue(&self, queue_idx: usize) {
        if queue_idx != REQUEST_QUEUE {
            return;
        }
        if let Some(qc) = self.mmio.queues.get(queue_idx) {
            let mut q = self.queue.lock().unwrap();
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

    /// Dispatch pending FUSE requests to the worker thread.
    /// Non-blocking — returns immediately.
    pub fn dispatch_requests(&self, memory: &GuestMemoryMmap) {
        let mut queue = self.queue.lock().unwrap();

        while let Some(desc_chain) = queue.pop_descriptor_chain(memory) {
            let head_index = desc_chain.head_index();

            let reader = match Reader::from_descriptor_chain(memory, desc_chain.clone()) {
                Ok(r) => r,
                Err(_) => continue,
            };

            let writer = match VirtioFsWriter::new(memory, desc_chain) {
                Ok(w) => Writer::VirtioFs(w),
                Err(_) => continue,
            };

            let request = unsafe {
                FuseRequest {
                    head_index,
                    reader: std::mem::transmute(reader),
                    writer: std::mem::transmute(writer),
                }
            };

            self.tx.send(request).ok();
        }
    }

    /// Block until the eventfd signals at least one completion.
    /// Uses a blocking read (the eventfd was created with EFD_NONBLOCK,
    /// so we temporarily switch to blocking mode).
    pub fn wait_for_completion(&self) {
        // Use poll with a timeout instead of switching fd flags
        let mut pollfd = libc::pollfd {
            fd: self.completion_eventfd,
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe {
            libc::poll(&mut pollfd, 1, 5000); // 5s timeout max
        }
    }

    /// Drain completions (non-blocking). Returns true if any were processed.
    /// Call this when completion_fd() is readable, or on every vCPU exit.
    pub fn process_completions(&self, memory: &GuestMemoryMmap) -> bool {
        // Drain the eventfd
        let mut val: u64 = 0;
        unsafe {
            libc::read(self.completion_eventfd, &mut val as *mut u64 as *mut libc::c_void, 8);
        }

        let mut queue = self.queue.lock().unwrap();
        let mut did_work = false;

        while let Ok(completion) = self.rx_completions.try_recv() {
            queue.add_used(memory, completion.head_index, 0).ok();
            did_work = true;
        }

        did_work
    }
}

impl Drop for VirtioFs {
    fn drop(&mut self) {
        unsafe { libc::close(self.completion_eventfd); }
    }
}
