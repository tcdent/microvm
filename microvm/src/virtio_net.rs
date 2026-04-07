// Virtio-net device — bridges guest networking to xdp-net NetBackend.
//
// Ownership model:
// - TX queue + backend fd: owned by vCPU thread, used synchronously
// - RX queue + backend fd: moved to worker thread after activation
// - No Arc, no Mutex in the hot path
// - Interrupt signaling via atomic interrupt_status + irqfd

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use virtio_queue::{Queue, QueueT};
use vm_memory::{GuestMemory, GuestMemoryMmap};
use xdp_net::{NetBackend, NetError};

use crate::virtio_mmio::{QueueConfig, VirtioMmioDevice};

const VIRTIO_DEV_NET: u32 = 1;
const VENDOR_ID: u32 = 0x554D_4551;
const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;

const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;

const VIRTIO_NET_F_MAC: u64 = 1 << 5;

pub struct VirtioNet<B: NetBackend + Send + 'static> {
    pub mmio: VirtioMmioDevice,
    tx_queue: Queue,
    // RX queue starts here, moves to worker thread on start_rx_worker()
    rx_queue: Option<Queue>,
    backend: Option<B>,
    // Raw fd for TX — the backend owns the fd, we dup it so TX and RX
    // each have their own fd to the same macvtap device
    tx_fd: RawFd,
    mac: [u8; 6],
    _rx_worker: Option<thread::JoinHandle<()>>,
}

impl<B: NetBackend + Send + 'static> VirtioNet<B> {
    pub fn new(backend: B, mac: [u8; 6], irq: u32) -> Self {
        let mmio = VirtioMmioDevice::new(
            VIRTIO_DEV_NET,
            VENDOR_ID,
            VIRTIO_NET_F_MAC,
            irq,
            NUM_QUEUES,
            QUEUE_SIZE,
        );

        // Dup the fd so TX (vCPU thread) and RX (worker thread) each have their own
        let tx_fd = unsafe { libc::dup(backend.tx_fd()) };
        assert!(tx_fd >= 0, "Failed to dup tx_fd");

        Self {
            mmio,
            tx_queue: Queue::new(QUEUE_SIZE).unwrap(),
            rx_queue: Some(Queue::new(QUEUE_SIZE).unwrap()),
            backend: Some(backend),
            tx_fd,
            mac,
            _rx_worker: None,
        }
    }

    pub fn config_read(&self, offset: u64) -> u32 {
        let config: [u8; 8] = [
            self.mac[0], self.mac[1], self.mac[2], self.mac[3],
            self.mac[4], self.mac[5],
            0x01, 0x00, // status: link up
        ];
        let off = offset as usize;
        if off < config.len() {
            let mut buf = [0u8; 4];
            let len = std::cmp::min(4, config.len() - off);
            buf[..len].copy_from_slice(&config[off..off + len]);
            u32::from_le_bytes(buf)
        } else {
            0
        }
    }

    pub fn activate_queue(&mut self, queue_idx: usize) {
        let qc = match self.mmio.queues.get(queue_idx) {
            Some(qc) => qc.clone(),
            None => return,
        };

        fn configure(q: &mut Queue, qc: &QueueConfig) {
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

        match queue_idx {
            RX_QUEUE => {
                if let Some(ref mut q) = self.rx_queue {
                    configure(q, &qc);
                }
            }
            TX_QUEUE => configure(&mut self.tx_queue, &qc),
            _ => {}
        }
    }

    /// Start the RX worker thread. Moves the RX queue and backend into the
    /// worker — after this call, only the worker touches them. No sharing.
    pub fn start_rx_worker(&mut self, memory: GuestMemoryMmap) {
        // Take ownership — these move into the worker thread
        let mut rx_queue = match self.rx_queue.take() {
            Some(q) => q,
            None => return, // already started
        };
        let backend = match self.backend.take() {
            Some(b) => b,
            None => return,
        };

        let irqfd = self.mmio.irqfd.try_clone().expect("clone irqfd");
        let interrupt_status = &self.mmio.interrupt_status as *const AtomicU32 as usize;
        let rx_fd = backend.rx_fd();

        self._rx_worker = Some(thread::spawn(move || {
            // backend is moved here — worker owns it entirely
            let _backend = backend;

            loop {
                // Block until the macvtap fd is readable
                let mut pollfd = libc::pollfd {
                    fd: rx_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut pollfd, 1, -1) } <= 0 { continue; }

                // Drain all available packets into guest memory
                let mut delivered = 0u32;
                loop {
                    let desc_chain = match rx_queue.pop_descriptor_chain(&memory) {
                        Some(dc) => dc,
                        None => break,
                    };
                    let head = desc_chain.head_index();

                    let mut iovecs: Vec<libc::iovec> = desc_chain.writable()
                        .filter_map(|desc| {
                            memory.get_host_address(desc.addr()).ok().map(|ptr| {
                                libc::iovec {
                                    iov_base: ptr as *mut libc::c_void,
                                    iov_len: desc.len() as usize,
                                }
                            })
                        })
                        .collect();

                    if iovecs.is_empty() {
                        rx_queue.add_used(&memory, head, 0).ok();
                        break;
                    }

                    match _backend.recv_iovec(&mut iovecs) {
                        Ok(n) => {
                            rx_queue.add_used(&memory, head, n as u32).ok();
                            delivered += 1;
                        }
                        Err(NetError::WouldBlock) => {
                            rx_queue.add_used(&memory, head, 0).ok();
                            break;
                        }
                        Err(_) => {
                            rx_queue.add_used(&memory, head, 0).ok();
                            break;
                        }
                    }
                }

                if delivered > 0 {
                    // SAFETY: pointer to AtomicU32 in mmio, valid for VM lifetime
                    unsafe {
                        let status = &*(interrupt_status as *const AtomicU32);
                        status.fetch_or(0x01, Ordering::Relaxed);
                    }
                    irqfd.write(1).ok();
                    std::thread::yield_now();
                }
            }
        }));
    }

    /// Process TX queue — guest sent packets via iovec, zero copy (vCPU thread).
    /// Uses the dup'd fd directly — no lock, no shared state.
    pub fn process_tx(&mut self, memory: &GuestMemoryMmap) {
        while let Some(desc_chain) = self.tx_queue.pop_descriptor_chain(memory) {
            let head_index = desc_chain.head_index();

            let iovecs: Vec<libc::iovec> = desc_chain.readable()
                .filter_map(|desc| {
                    memory.get_host_address(desc.addr()).ok().map(|ptr| {
                        libc::iovec {
                            iov_base: ptr as *mut libc::c_void,
                            iov_len: desc.len() as usize,
                        }
                    })
                })
                .collect();

            if !iovecs.is_empty() {
                // writev directly to our dup'd fd — no backend lock needed
                unsafe {
                    libc::writev(self.tx_fd, iovecs.as_ptr(), iovecs.len() as i32);
                }
            }

            self.tx_queue.add_used(memory, head_index, 0).ok();
        }
    }
}

impl<B: NetBackend + Send + 'static> Drop for VirtioNet<B> {
    fn drop(&mut self) {
        unsafe { libc::close(self.tx_fd); }
    }
}
