// Virtio-net device — bridges guest networking to xdp-net NetBackend.
//
// Guest sees a standard virtio-net device over MMIO.
// Host side uses xdp-net's MacvtapBackend (or any NetBackend) for packet I/O.
//
// Designed to be decoupled: the MMIO transport is shared infrastructure,
// the packet backend is pluggable via the NetBackend trait.

use std::os::fd::RawFd;

use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use xdp_net::{NetBackend, NetError};

use crate::virtio_mmio::VirtioMmioDevice;

const VIRTIO_DEV_NET: u32 = 1;
const VENDOR_ID: u32 = 0x554D_4551;
const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;

const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;

// virtio-net feature bits
const VIRTIO_NET_F_MAC: u64 = 1 << 5;

pub struct VirtioNet<B: NetBackend> {
    pub mmio: VirtioMmioDevice,
    queues: Vec<Queue>,
    backend: B,
    mac: [u8; 6],
}

impl<B: NetBackend> VirtioNet<B> {
    pub fn new(backend: B, mac: [u8; 6], irq: u32) -> Self {
        let mmio = VirtioMmioDevice::new(
            VIRTIO_DEV_NET,
            VENDOR_ID,
            VIRTIO_NET_F_MAC,
            irq,
            NUM_QUEUES,
            QUEUE_SIZE,
        );

        let queues = (0..NUM_QUEUES)
            .map(|_| Queue::new(QUEUE_SIZE).unwrap())
            .collect();

        Self { mmio, queues, backend, mac }
    }

    /// Config space read — returns MAC address and status
    pub fn config_read(&self, offset: u64) -> u32 {
        // Config layout: mac[6 bytes] + status[2 bytes]
        match offset {
            0 => u32::from_le_bytes([self.mac[0], self.mac[1], self.mac[2], self.mac[3]]),
            4 => u32::from_le_bytes([self.mac[4], self.mac[5], 0x01, 0x00]), // status: link up
            _ => 0,
        }
    }

    pub fn activate_queue(&mut self, queue_idx: usize) {
        if let Some(qc) = self.mmio.queues.get(queue_idx) {
            if let Some(q) = self.queues.get_mut(queue_idx) {
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

    /// RX fd for polling — when readable, call process_rx
    pub fn rx_fd(&self) -> RawFd {
        self.backend.rx_fd()
    }

    /// Process TX queue — guest sent packets, forward to backend
    pub fn process_tx(&mut self, memory: &GuestMemoryMmap) {
        let queue = &mut self.queues[TX_QUEUE];

        while let Some(desc_chain) = queue.pop_descriptor_chain(memory) {
            let head_index = desc_chain.head_index();
            let mut buf = vec![0u8; 65536];
            let mut len = 0usize;

            // Collect all readable descriptors into a flat buffer
            for desc in desc_chain.readable() {
                let chunk_len = desc.len() as usize;
                if len + chunk_len <= buf.len() {
                    memory.read_slice(&mut buf[len..len + chunk_len], desc.addr()).ok();
                    len += chunk_len;
                }
            }

            if len > 0 {
                self.backend.send(&buf[..len]).ok();
            }

            queue.add_used(memory, head_index, 0).ok();
        }
    }

    /// Process RX — read packet from backend, deliver to guest
    pub fn process_rx(&mut self, memory: &GuestMemoryMmap) -> bool {
        let mut buf = vec![0u8; 65536];
        let n = match self.backend.recv(&mut buf) {
            Ok(n) => n,
            Err(NetError::WouldBlock) => return false,
            Err(_) => return false,
        };

        let queue = &mut self.queues[RX_QUEUE];
        if let Some(desc_chain) = queue.pop_descriptor_chain(memory) {
            let head_index = desc_chain.head_index();
            let mut written = 0usize;

            for desc in desc_chain.writable() {
                let chunk_len = std::cmp::min(desc.len() as usize, n - written);
                if chunk_len == 0 { break; }
                memory.write_slice(&buf[written..written + chunk_len], desc.addr()).ok();
                written += chunk_len;
            }

            queue.add_used(memory, head_index, written as u32).ok();
            true
        } else {
            false // no RX buffers available
        }
    }
}
