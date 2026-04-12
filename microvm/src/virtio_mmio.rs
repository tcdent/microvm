// Virtio MMIO transport — shared infrastructure for all virtio devices.
// Implements the virtio-mmio register interface (spec v2) and virtqueue management.

use std::os::fd::RawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use vmm_sys_util::eventfd::EventFd;

// MMIO register offsets (virtio spec 4.2.2)
pub const MMIO_MAGIC: u32 = 0x7472_6976; // "virt"
pub const MMIO_VERSION: u32 = 2;

const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// Device status bits
const STATUS_ACKNOWLEDGE: u32 = 0x01;
const STATUS_DRIVER: u32 = 0x02;
const STATUS_FEATURES_OK: u32 = 0x08;
const STATUS_DRIVER_OK: u32 = 0x04;
const STATUS_FAILED: u32 = 0x80;

/// State shared by all virtio MMIO devices
pub struct VirtioMmioDevice {
    pub device_id: u32,
    pub vendor_id: u32,
    pub device_features: u64,
    pub irq: u32,
    pub irqfd: EventFd,

    // Negotiation state
    pub status: u32,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    /// Pending-interrupt bit visible to the guest via the
    /// InterruptStatus MMIO register (offset 0x060). Shared with
    /// worker threads (virtio-fs, virtio-net, virtio-console) so
    /// they can set the bit from their own threads before firing
    /// irqfd. `Arc<AtomicU32>` replaces the older pattern of
    /// casting `&AtomicU32` to `usize` to cross thread boundaries.
    pub interrupt_status: Arc<AtomicU32>,

    // Queue configuration (indexed by queue_sel)
    queue_sel: u32,
    pub queues: Vec<QueueConfig>,
}

#[derive(Clone)]
pub struct QueueConfig {
    pub max_size: u16,
    pub size: u16,
    pub ready: bool,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
}

impl QueueConfig {
    pub fn new(max_size: u16) -> Self {
        Self {
            max_size,
            size: 0,
            ready: false,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
        }
    }
}

impl VirtioMmioDevice {
    pub fn new(device_id: u32, vendor_id: u32, device_features: u64, irq: u32, num_queues: usize, queue_size: u16) -> Self {
        let queues = (0..num_queues).map(|_| QueueConfig::new(queue_size)).collect();
        let irqfd = EventFd::new(libc::EFD_NONBLOCK).expect("failed to create irqfd");
        Self {
            device_id,
            vendor_id,
            device_features: device_features | VIRTIO_F_VERSION_1,
            irq,
            irqfd,
            status: 0,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            interrupt_status: Arc::new(AtomicU32::new(0)),
            queue_sel: 0,
            queues,
        }
    }

    fn selected_queue(&self) -> Option<&QueueConfig> {
        self.queues.get(self.queue_sel as usize)
    }

    fn selected_queue_mut(&mut self) -> Option<&mut QueueConfig> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// Handle MMIO read. Returns the value to give back to the guest.
    pub fn read(&self, offset: u64, config_read: impl Fn(u64) -> u32) -> u32 {
        match offset {
            0x000 => MMIO_MAGIC,
            0x004 => MMIO_VERSION,
            0x008 => self.device_id,
            0x00C => self.vendor_id,
            0x010 => {
                // DeviceFeatures — depends on selector
                let shift = self.device_features_sel as u64 * 32;
                (self.device_features >> shift) as u32
            }
            0x034 => self.selected_queue().map_or(0, |q| q.max_size as u32),
            0x044 => self.selected_queue().map_or(0, |q| q.ready as u32),
            0x060 => self.interrupt_status.load(Ordering::Relaxed),
            0x070 => self.status,
            // Config space starts at 0x100
            off @ 0x100.. => config_read(off - 0x100),
            _ => 0,
        }
    }

    /// Handle MMIO write. Returns true if a queue notification was triggered.
    pub fn write(&mut self, offset: u64, val: u32, config_write: impl Fn(u64, u32)) -> bool {
        match offset {
            0x014 => self.device_features_sel = val,
            0x020 => {
                let shift = self.driver_features_sel as u64 * 32;
                self.driver_features |= (val as u64) << shift;
            }
            0x024 => self.driver_features_sel = val,
            0x030 => self.queue_sel = val,
            0x038 => {
                if let Some(q) = self.selected_queue_mut() {
                    q.size = val as u16;
                }
            }
            0x044 => {
                if let Some(q) = self.selected_queue_mut() {
                    q.ready = val == 1;
                }
            }
            0x050 => {
                // QueueNotify — guest says there are new buffers
                return true;
            }
            0x064 => {
                // InterruptACK
                self.interrupt_status.fetch_and(!val, Ordering::Relaxed);
            }
            0x070 => self.status = val,
            // Queue address registers (low/high halves)
            0x080 => {
                if let Some(q) = self.selected_queue_mut() {
                    q.desc_addr = (q.desc_addr & !0xFFFF_FFFF) | val as u64;
                }
            }
            0x084 => {
                if let Some(q) = self.selected_queue_mut() {
                    q.desc_addr = (q.desc_addr & 0xFFFF_FFFF) | ((val as u64) << 32);
                }
            }
            0x090 => {
                if let Some(q) = self.selected_queue_mut() {
                    q.avail_addr = (q.avail_addr & !0xFFFF_FFFF) | val as u64;
                }
            }
            0x094 => {
                if let Some(q) = self.selected_queue_mut() {
                    q.avail_addr = (q.avail_addr & 0xFFFF_FFFF) | ((val as u64) << 32);
                }
            }
            0x0A0 => {
                if let Some(q) = self.selected_queue_mut() {
                    q.used_addr = (q.used_addr & !0xFFFF_FFFF) | val as u64;
                }
            }
            0x0A4 => {
                if let Some(q) = self.selected_queue_mut() {
                    q.used_addr = (q.used_addr & 0xFFFF_FFFF) | ((val as u64) << 32);
                }
            }
            // Config space writes start at 0x100
            off @ 0x100.. => config_write(off - 0x100, val),
            _ => {}
        }
        false
    }

    pub fn queue_sel(&self) -> u32 {
        self.queue_sel
    }

    /// Inject an interrupt — sets status and writes to the irqfd.
    /// KVM delivers the interrupt entirely in kernel space.
    pub fn inject_irq(&mut self) {
        self.interrupt_status.fetch_or(0x01, Ordering::Relaxed);
        self.irqfd.write(1).ok();
    }

    /// Raw fd for the irqfd — register with KVM via register_irqfd().
    pub fn irqfd_fd(&self) -> &EventFd {
        &self.irqfd
    }

    pub fn is_driver_ready(&self) -> bool {
        self.status & STATUS_DRIVER_OK != 0
    }
}
