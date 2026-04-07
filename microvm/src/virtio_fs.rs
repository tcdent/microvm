// Virtio-fs device — bridges virtio MMIO transport to fuse-backend-rs.
// Speaks the virtio-fs spec (device ID 26) and dispatches FUSE messages
// to a FileSystem implementation via fuse-backend-rs Server.
//
// Designed to be decoupled: the MMIO transport is generic, the FUSE
// handling comes from fuse-backend-rs, and the filesystem backend is
// pluggable via the FileSystem trait.

use std::sync::Arc;

use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::api::filesystem::FileSystem;
use fuse_backend_rs::transport::{Reader, Writer, VirtioFsWriter};
use virtio_queue::{Queue, QueueT};
use vm_memory::{GuestAddress, GuestMemoryMmap, GuestMemory, Bytes};

use crate::virtio_mmio::VirtioMmioDevice;

const VIRTIO_DEV_FS: u32 = 26;
const VENDOR_ID: u32 = 0x554D_4551;
const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;

// virtio-fs queues
const HIPRIO_QUEUE: usize = 0;
const REQUEST_QUEUE: usize = 1;

pub struct VirtioFs<F: FileSystem + Sync> {
    pub mmio: VirtioMmioDevice,
    server: Server<F>,
    queues: Vec<Queue>,
    tag: String,
}

impl<F: FileSystem + Sync> VirtioFs<F> {
    pub fn new(fs: F, tag: &str, irq: u32) -> Self {
        let mmio = VirtioMmioDevice::new(
            VIRTIO_DEV_FS,
            VENDOR_ID,
            0,
            irq,
            NUM_QUEUES,
            QUEUE_SIZE,
        );

        let queues = (0..NUM_QUEUES)
            .map(|_| Queue::new(QUEUE_SIZE).unwrap())
            .collect();

        Self {
            mmio,
            server: Server::new(fs),
            queues,
            tag: tag.to_string(),
        }
    }

    /// Config space read — returns the tag name (virtio-fs spec 5.11.4)
    pub fn config_read(&self, offset: u64) -> u32 {
        let tag_bytes = self.tag.as_bytes();
        match offset {
            // tag[36 bytes]
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
            // num_request_queues
            36 => 1,
            _ => 0,
        }
    }

    /// Called when the guest sets a queue to ready — configure the
    /// virtio-queue Queue object with the addresses from MMIO config.
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

    /// Process queued FUSE requests from the request queue
    pub fn process_queue(&mut self, memory: &GuestMemoryMmap) {
        let queue = &mut self.queues[REQUEST_QUEUE];

        while let Some(desc_chain) = queue.pop_descriptor_chain(memory) {
            let head_index = desc_chain.head_index();

            // Create Reader (readable descriptors) and Writer (writable descriptors)
            let reader = match Reader::from_descriptor_chain(memory, desc_chain.clone()) {
                Ok(r) => r,
                Err(_) => continue,
            };

            let writer = match VirtioFsWriter::new(memory, desc_chain) {
                Ok(w) => Writer::VirtioFs(w),
                Err(_) => continue,
            };

            // Dispatch FUSE message to filesystem implementation
            let _ = self.server.handle_message(reader, writer, None, None);

            // Mark the descriptor as used
            queue.add_used(memory, head_index, 0).ok();
        }
    }
}
