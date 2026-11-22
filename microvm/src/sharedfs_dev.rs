// sharedfs device — MMIO ceremony around `sharedfs::device`.
//
// Mirrors virtio_fs.rs: register plumbing, ioeventfd registration,
// ownership of the queue and kick fd until the worker starts. The
// index region is published through the virtio-mmio shared-memory
// registers and mapped into the guest read-only by the launcher.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use kvm_ioctls::{IoEventAddress, NoDatamatch, VmFd};
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;

use sharedfs::Server;
use sharedfs::device::{DEVICE_ID, NOTIFY_QUEUE, NUM_QUEUES, QUEUE_SIZE, REQUEST_QUEUE, Session, TAG_LEN, serve};

use crate::virtio_mmio::VirtioMmioDevice;

const VENDOR_ID: u32 = 0x554D_4551;

pub struct SharedfsDev {
    pub mmio: VirtioMmioDevice,
    /// Shared with the worker once it runs: the driver's resets and reconfigurations reach
    /// the rings the worker is using.
    queues: Vec<Arc<Mutex<Queue>>>,
    kick_fd: EventFd,
    ioevent_registered: bool,
    /// Tells the worker the driver reset the device; see `Session::replay`.
    replay: Arc<AtomicBool>,
    mmio_base: u64,
    tag: String,
    server: Option<Server>,
    /// Guest-physical base, host address and length of the data window, kept after the server
    /// moves to its thread.
    window: Option<(u64, usize, usize)>,
    _worker: Option<thread::JoinHandle<()>>,
}

impl SharedfsDev {
    pub fn new(server: Server, tag: &str, irq: u32, mmio_base: u64, shm_base: u64, window_base: u64) -> Self {
        let mut mmio = VirtioMmioDevice::new(DEVICE_ID, VENDOR_ID, 0, irq, NUM_QUEUES, QUEUE_SIZE);
        // Shared memory region 0 is the index, region 1 the data window.
        mmio.shm = vec![(shm_base, server.index().len() as u64)];
        if let Some(w) = server.window() {
            mmio.shm.push((window_base, w.len() as u64));
        }
        let window = server.window().map(|w| (window_base, w.as_ptr() as usize, w.len()));
        Self {
            mmio,
            queues: (0..NUM_QUEUES).map(|_| Arc::new(Mutex::new(Queue::new(QUEUE_SIZE).unwrap()))).collect(),
            kick_fd: EventFd::new(0).expect("eventfd"),
            ioevent_registered: false,
            replay: Arc::new(AtomicBool::new(false)),
            mmio_base,
            tag: tag.to_owned(),
            server: Some(server),
            window,
            _worker: None,
        }
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// The guest touched a window page KVM could not back: the host file was truncated under a
    /// live mapping, or the guest read a range nobody mapped. Beyond a file's end there are no
    /// bytes, so a page of zeros is the right answer; it is replaced the next time the daemon
    /// maps something there. Returns false when the fault is not in the window.
    pub fn window_fault(&self, gpa: u64) -> bool {
        let Some((gpa_base, base, len)) = self.window else { return false };
        if gpa < gpa_base || gpa >= gpa_base + len as u64 {
            return false;
        }
        let page = base + ((gpa - gpa_base) as usize & !0xfff);
        // SAFETY: The page lies inside our own window reservation; MAP_FIXED replaces whatever
        // is there with anonymous zeros.
        let p = unsafe {
            libc::mmap(
                page as *mut libc::c_void,
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        p != libc::MAP_FAILED
    }

    /// Device configuration space: the tag, NUL-padded to `TAG_LEN`. The index header carries
    /// the geometry.
    pub fn config_read(&self, offset: u64) -> u32 {
        let off = offset as usize;
        if off >= TAG_LEN {
            return 0;
        }
        let tag = self.tag.as_bytes();
        let mut buf = [0u8; 4];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = tag.get(off + i).copied().unwrap_or(0);
        }
        u32::from_le_bytes(buf)
    }

    /// The driver reset the device, as it does around a hibernation: the rings start over. The
    /// worker keeps running on the shared queues and sees the fresh configuration next.
    pub fn reset(&mut self) {
        for q in &self.queues {
            q.lock().unwrap().reset();
        }
        if self._worker.is_some() {
            self.replay.store(true, Ordering::Release);
        }
    }

    pub fn activate_queue(&mut self, queue_idx: usize) {
        if queue_idx >= NUM_QUEUES {
            return;
        }
        if let (Some(q), Some(qc)) = (self.queues.get(queue_idx), self.mmio.queues.get(queue_idx)) {
            let mut q = q.lock().unwrap();
            q.set_size(qc.size);
            q.set_desc_table_address(Some((qc.desc_addr & 0xFFFF_FFFF) as u32), Some((qc.desc_addr >> 32) as u32));
            q.set_avail_ring_address(Some((qc.avail_addr & 0xFFFF_FFFF) as u32), Some((qc.avail_addr >> 32) as u32));
            q.set_used_ring_address(Some((qc.used_addr & 0xFFFF_FFFF) as u32), Some((qc.used_addr >> 32) as u32));
            q.set_ready(true);
        }
    }

    /// Starts the worker once every queue is ready; the driver readies them in order. Started
    /// once per VM: after a reset the same worker serves the reconfigured rings.
    pub fn start_worker(&mut self, vm: &VmFd, memory: Arc<GuestMemoryMmap>) {
        if self._worker.is_some() {
            return;
        }
        if !self.mmio.queues.iter().take(NUM_QUEUES).all(|q| q.ready) {
            return;
        }
        if !self.ioevent_registered {
            vm.register_ioevent(&self.kick_fd, &IoEventAddress::Mmio(self.mmio_base + 0x050), NoDatamatch)
                .expect("register ioeventfd for sharedfs");
            self.ioevent_registered = true;
        }
        let session = Session {
            server: self.server.take().expect("server already taken"),
            memory,
            queue: self.queues[REQUEST_QUEUE].clone(),
            notify_queue: self.queues[NOTIFY_QUEUE].clone(),
            kick_fd: self.kick_fd.try_clone().expect("clone kick_fd"),
            irqfd: self.mmio.irqfd.try_clone().expect("clone irqfd"),
            interrupt_status: self.mmio.interrupt_status.clone(),
            replay: self.replay.clone(),
            thread_name: format!("vm-sharedfs-{}", self.tag),
        };
        self._worker = Some(serve(session));
    }
}
