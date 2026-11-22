// Virtio-blk — a file-backed disk. One request queue, processed inline
// on the vCPU thread when the guest kicks (same shape as console TX):
// a read or write syscall per data descriptor is fast enough for a
// project environment and keeps the device to one screen of code.
//
// Features: FLUSH always; RO when the backing file is opened read-only.
// Config space is the capacity in 512-byte sectors. A writable disk grows while the guest
// runs: extend the backing file (`truncate -s 50G`) and a watcher raises the capacity and
// sends the config-change interrupt; the guest's driver resizes the disk on the spot.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestMemoryMmap};

use crate::virtio_mmio::{ConfigNotifier, VirtioMmioDevice};

const VIRTIO_DEV_BLK: u32 = 2;
const VENDOR_ID: u32 = 0x554D_4551;
const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 1;

const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const SECTOR: u64 = 512;
/// How often the watcher looks at the backing file's size.
const GROW_CHECK: Duration = Duration::from_secs(2);

pub struct VirtioBlk {
    pub mmio: VirtioMmioDevice,
    queue: Queue,
    file: File,
    read_only: bool,
    /// In sectors. Shared with the watcher, which only ever raises it.
    capacity: Arc<AtomicU64>,
    notifier: ConfigNotifier,
    id: [u8; 20],
}

impl VirtioBlk {
    pub fn new(file: File, read_only: bool, irq: u32, id: &str) -> std::io::Result<Self> {
        let capacity = Arc::new(AtomicU64::new(file.metadata()?.len() / SECTOR));
        let features = VIRTIO_BLK_F_FLUSH | if read_only { VIRTIO_BLK_F_RO } else { 0 };
        let mmio = VirtioMmioDevice::new(VIRTIO_DEV_BLK, VENDOR_ID, features, irq, NUM_QUEUES, QUEUE_SIZE);
        let mut idb = [0u8; 20];
        let n = id.len().min(20);
        idb[..n].copy_from_slice(&id.as_bytes()[..n]);
        if !read_only {
            let (watched, cap, notifier) = (file.try_clone()?, capacity.clone(), mmio.config_notifier());
            std::thread::Builder::new().name(format!("{id}-grow")).spawn(move || loop {
                std::thread::sleep(GROW_CHECK);
                let Ok(md) = watched.metadata() else { continue };
                let now = md.len() / SECTOR;
                // Growing only: a disk shrunk under a running guest would lose whatever the
                // guest keeps past the new end.
                if now > cap.load(Ordering::Acquire) {
                    cap.store(now, Ordering::Release);
                    notifier.notify();
                }
            })?;
        }
        let notifier = mmio.config_notifier();
        Ok(Self { mmio, queue: Queue::new(QUEUE_SIZE).unwrap(), file, read_only, capacity, notifier, id: idb })
    }

    /// The driver has set the device up (DRIVER_OK). It read the capacity while probing, but a
    /// guest resuming from hibernation still has the one from before it stopped; announcing a
    /// config change makes it read the capacity again, which costs nothing when it is unchanged.
    pub fn driver_ready(&self) {
        self.notifier.notify();
    }

    pub fn config_read(&self, offset: u64) -> u32 {
        let cap = self.capacity.load(Ordering::Acquire).to_le_bytes();
        let off = offset as usize;
        if off < 8 {
            let mut b = [0u8; 4];
            let n = (8 - off).min(4);
            b[..n].copy_from_slice(&cap[off..off + n]);
            u32::from_le_bytes(b)
        } else {
            0
        }
    }

    /// The driver reset the device: forget the ring positions, the next activation brings
    /// fresh rings.
    pub fn reset(&mut self) {
        self.queue.reset();
    }

    pub fn activate_queue(&mut self, queue_idx: usize) {
        if queue_idx != 0 {
            return;
        }
        if let Some(qc) = self.mmio.queues.get(0) {
            let q = &mut self.queue;
            q.set_size(qc.size);
            q.set_desc_table_address(Some((qc.desc_addr & 0xFFFF_FFFF) as u32), Some((qc.desc_addr >> 32) as u32));
            q.set_avail_ring_address(Some((qc.avail_addr & 0xFFFF_FFFF) as u32), Some((qc.avail_addr >> 32) as u32));
            q.set_used_ring_address(Some((qc.used_addr & 0xFFFF_FFFF) as u32), Some((qc.used_addr >> 32) as u32));
            q.set_ready(true);
        }
    }

    /// Service every pending request. Returns true if any completed.
    pub fn process(&mut self, mem: &GuestMemoryMmap) -> bool {
        let mut any = false;
        while let Some(chain) = self.queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let descs: Vec<_> = chain.collect();
            let (status, used) = self.request(mem, &descs);
            if let Some(st) = descs.last() {
                mem.write_slice(&[status], st.addr()).ok();
            }
            self.queue.add_used(mem, head, used + 1).ok();
            any = true;
        }
        any
    }

    /// Execute one request. Returns (status, data bytes written to guest).
    fn request(&mut self, mem: &GuestMemoryMmap, descs: &[virtio_queue::desc::split::Descriptor]) -> (u8, u32) {
        if descs.len() < 2 {
            return (S_IOERR, 0);
        }
        let mut hdr = [0u8; 16];
        if mem.read_slice(&mut hdr, descs[0].addr()).is_err() {
            return (S_IOERR, 0);
        }
        let kind = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        let data = &descs[1..descs.len() - 1];
        let mut offset = sector * SECTOR;
        let mut used = 0u32;

        match kind {
            T_IN => {
                for d in data {
                    let len = d.len() as usize;
                    if offset + len as u64 > self.capacity.load(Ordering::Acquire) * SECTOR {
                        return (S_IOERR, used);
                    }
                    let mut buf = vec![0u8; len];
                    if self.file.read_exact_at(&mut buf, offset).is_err() || mem.write_slice(&buf, d.addr()).is_err() {
                        return (S_IOERR, used);
                    }
                    offset += len as u64;
                    used += len as u32;
                }
                (S_OK, used)
            }
            T_OUT => {
                if self.read_only {
                    return (S_IOERR, 0);
                }
                for d in data {
                    let len = d.len() as usize;
                    if offset + len as u64 > self.capacity.load(Ordering::Acquire) * SECTOR {
                        return (S_IOERR, 0);
                    }
                    let mut buf = vec![0u8; len];
                    if mem.read_slice(&mut buf, d.addr()).is_err() || self.file.write_all_at(&buf, offset).is_err() {
                        return (S_IOERR, 0);
                    }
                    offset += len as u64;
                }
                (S_OK, 0)
            }
            T_FLUSH => {
                if self.file.sync_data().is_err() {
                    return (S_IOERR, 0);
                }
                (S_OK, 0)
            }
            T_GET_ID => match data.first() {
                Some(d) => {
                    let n = (d.len() as usize).min(20);
                    if mem.write_slice(&self.id[..n], d.addr()).is_err() {
                        return (S_IOERR, 0);
                    }
                    (S_OK, n as u32)
                }
                None => (S_IOERR, 0),
            },
            _ => (S_UNSUPP, 0),
        }
    }
}
