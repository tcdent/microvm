// Virtio-net device — bridges guest networking to xdp-net NetBackend.
//
// TX: inline on vCPU thread (guest kicks QueueNotify → MMIO exit → writev)
// RX: dedicated worker thread (poll tap fd → readv into guest → irqfd)
//
// RX uses readv directly into guest memory via iovecs built from the
// descriptor chain — zero copies. TSO/GRO features allow the macvtap
// to deliver aggregated 60KB+ packets, giving us ~2.4 Gbps throughput.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use virtio_queue::{Queue, QueueOwnedT, QueueT};
use vm_memory::{GuestMemory, GuestMemoryMmap};
use xdp_net::NetBackend;

use crate::virtio_mmio::{QueueConfig, VirtioMmioDevice};

const VIRTIO_DEV_NET: u32 = 1;
const VENDOR_ID: u32 = 0x554D_4551;
const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;

const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;

// Feature bits — enable all offloads for maximum throughput.
// TSO/GRO is the biggest win: macvtap delivers 60KB+ aggregated packets.
// ECN is required alongside TSO for ECN-marked connections.
const VIRTIO_NET_F_CSUM: u64 = 1 << 0;
const VIRTIO_NET_F_GUEST_CSUM: u64 = 1 << 1;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_GSO: u64 = 1 << 6;
const VIRTIO_NET_F_GUEST_TSO4: u64 = 1 << 7;
const VIRTIO_NET_F_GUEST_TSO6: u64 = 1 << 8;
const VIRTIO_NET_F_GUEST_ECN: u64 = 1 << 9;
const VIRTIO_NET_F_GUEST_UFO: u64 = 1 << 10;
const VIRTIO_NET_F_HOST_TSO4: u64 = 1 << 11;
const VIRTIO_NET_F_HOST_TSO6: u64 = 1 << 12;
const VIRTIO_NET_F_HOST_ECN: u64 = 1 << 13;
const VIRTIO_NET_F_HOST_UFO: u64 = 1 << 14;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;

/// Max iovec entries per descriptor chain.
const MAX_IOVECS: usize = 16;

pub struct VirtioNet<B: NetBackend + Send + 'static> {
    pub mmio: VirtioMmioDevice,
    tx_queue: Queue,
    rx_queue: Option<Queue>,
    backend: Option<B>,
    /// Dup'd fd for TX on the vCPU thread. The worker thread owns the
    /// backend (and its fd) for RX. Full duplex, no interference.
    tx_fd: RawFd,
    mac: [u8; 6],
    _rx_worker: Option<thread::JoinHandle<()>>,
}

impl<B: NetBackend + Send + 'static> VirtioNet<B> {
    pub fn new(backend: B, mac: [u8; 6], irq: u32) -> Self {
        let mmio = VirtioMmioDevice::new(
            VIRTIO_DEV_NET,
            VENDOR_ID,
            VIRTIO_NET_F_CSUM | VIRTIO_NET_F_GUEST_CSUM
            | VIRTIO_NET_F_HOST_TSO4 | VIRTIO_NET_F_HOST_TSO6
            | VIRTIO_NET_F_GUEST_TSO4 | VIRTIO_NET_F_GUEST_TSO6
            | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS,
            irq,
            NUM_QUEUES,
            QUEUE_SIZE,
        );

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

    /// Start the RX worker thread. Call after the guest writes DRIVER_OK.
    pub fn start_rx_worker(&mut self, memory: GuestMemoryMmap) {
        let rx_queue = match self.rx_queue.take() {
            Some(q) => q,
            None => return,
        };
        let backend = match self.backend.take() {
            Some(b) => b,
            None => return,
        };

        let irqfd = self.mmio.irqfd.try_clone().expect("clone irqfd");
        let interrupt_status = &self.mmio.interrupt_status as *const AtomicU32 as usize;
        let rx_fd = backend.rx_fd();

        self._rx_worker = Some(thread::spawn(move || {
            let _backend = backend;
            let mut rx_queue = rx_queue;

            // Wait for the guest to post RX buffers. This happens after
            // DRIVER_OK when the interface comes up (virtnet_open).
            loop {
                if rx_queue.pop_descriptor_chain(&memory).is_some() {
                    rx_queue.go_to_previous_position();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            // Outer loop: block on poll when idle, busy-drain when active.
            // After delivering packets, skip poll and immediately retry —
            // the IRQ is in-flight, more packets likely arriving. Only
            // fall back to blocking poll after two consecutive empty drains.
            let mut idle = true;
            loop {
                if idle {
                    let mut pollfd = libc::pollfd {
                        fd: rx_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    if unsafe { libc::poll(&mut pollfd, 1, -1) } <= 0 {
                        continue;
                    }
                }

                let mut delivered = 0u32;
                loop {
                    let dc = match rx_queue.pop_descriptor_chain(&memory) {
                        Some(dc) => dc,
                        None => break,
                    };
                    let head = dc.head_index();

                    let mut iovecs = [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; MAX_IOVECS];
                    let mut iov_count = 0usize;
                    for desc in dc.writable() {
                        if iov_count >= MAX_IOVECS { break; }
                        if let Ok(ptr) = memory.get_host_address(desc.addr()) {
                            iovecs[iov_count] = libc::iovec {
                                iov_base: ptr as *mut libc::c_void,
                                iov_len: desc.len() as usize,
                            };
                            iov_count += 1;
                        }
                    }

                    if iov_count == 0 {
                        rx_queue.add_used(&memory, head, 0).ok();
                        break;
                    }

                    let ret = unsafe {
                        libc::readv(rx_fd, iovecs.as_ptr(), iov_count as i32)
                    };

                    if ret <= 0 {
                        rx_queue.go_to_previous_position();
                        break;
                    }

                    rx_queue.add_used(&memory, head, ret as u32).ok();
                    delivered += 1;
                }

                if delivered > 0 {
                    unsafe {
                        let status = &*(interrupt_status as *const AtomicU32);
                        status.fetch_or(0x01, Ordering::Relaxed);
                    }
                    irqfd.write(1).ok();
                    idle = false; // stay in busy-drain mode
                } else {
                    if idle {
                        // Already was idle and got nothing — stay in poll
                    } else {
                        // Was active, got nothing — one more try before sleeping
                        idle = true;
                    }
                }
            }
        }));
    }

    /// Process TX — guest sent packets, forward to tap. Inline on vCPU thread.
    pub fn process_tx(&mut self, memory: &GuestMemoryMmap) {
        while let Some(desc_chain) = self.tx_queue.pop_descriptor_chain(memory) {
            let head_index = desc_chain.head_index();

            // Stack-allocated iovecs — no heap allocation per packet.
            let mut iovecs = [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; MAX_IOVECS];
            let mut iov_count = 0usize;
            for desc in desc_chain.readable() {
                if iov_count >= MAX_IOVECS { break; }
                if let Ok(ptr) = memory.get_host_address(desc.addr()) {
                    iovecs[iov_count] = libc::iovec {
                        iov_base: ptr as *mut libc::c_void,
                        iov_len: desc.len() as usize,
                    };
                    iov_count += 1;
                }
            }

            if iov_count > 0 {
                unsafe {
                    libc::writev(self.tx_fd, iovecs.as_ptr(), iov_count as i32);
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
