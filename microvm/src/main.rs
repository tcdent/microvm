#[macro_use]
mod log;

mod cpu;
mod event_loop;
mod kernel;
mod memory;
mod mptable;
mod serial;
mod virtio_fs;
mod virtio_mmio;
mod virtio_net;
mod vm;

use std::sync::Arc;

use kvm_ioctls::{VcpuExit, VcpuFd, VmFd};
use vm_memory::GuestMemoryMmap;
use xdp_net::{MacvtapBackend, NetBackend};

use s3_rootfs::{S3Fs, S3Config};

use event_loop::EventPoll;
use serial::Serial;
use virtio_fs::VirtioFs;
use virtio_net::VirtioNet;

// MMIO device addresses
const VIRTIO_FS_BASE: u64 = 0xD000_0000;
const VIRTIO_FS_IRQ: u32 = 5;
const VIRTIO_NET_BASE: u64 = 0xD000_1000;
const VIRTIO_NET_IRQ: u32 = 6;

// Event tokens for epoll
const EVENT_FUSE_COMPLETION: u64 = 1;
const EVENT_NET_RX: u64 = 2;

struct System {
    vm: Arc<VmFd>,
    memory: GuestMemoryMmap,
    vcpu: VcpuFd,
    serial: Serial,
    virtiofs: VirtioFs,
    virtionet: Option<VirtioNet<MacvtapBackend>>,
    poll: EventPoll,
}

impl System {
    fn new(kernel_path: &str, initrd_path: &str, net_ifname: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        let (kvm, vm) = vm::create_vm()?;
        let vm = Arc::new(vm);
        let memory = memory::setup_guest_memory(&vm)?;
        mptable::write(&memory)?;
        let vcpu = cpu::create_vcpu(&kvm, &vm, &memory)?;

        let (kernel_entry, boot_params_addr) = kernel::load(&memory, kernel_path, initrd_path)?;
        cpu::setup_regs(&vcpu, kernel_entry, boot_params_addr)?;

        let serial = serial::create(&vm);
        let s3fs = S3Fs::new(S3Config::default());
        let virtiofs = VirtioFs::new(s3fs, "rootfs", VIRTIO_FS_IRQ, VIRTIO_FS_BASE);
        vm.register_irqfd(virtiofs.mmio.irqfd_fd(), VIRTIO_FS_IRQ)?;

        let virtionet = if let Some(ifname) = net_ifname {
            log!("Creating virtio-net with interface {}", ifname);
            let backend = MacvtapBackend::new(ifname)?;
            let mac = [0x06, 0x00, 0xAC, 0x10, 0xFF, 0x01];
            let net = VirtioNet::new(backend, mac, VIRTIO_NET_IRQ);
            vm.register_irqfd(net.mmio.irqfd_fd(), VIRTIO_NET_IRQ)?;
            Some(net)
        } else {
            None
        };

        let poll = EventPoll::new();

        Ok(Self { vm, memory, vcpu, serial, virtiofs, virtionet, poll })
    }

    fn run(&mut self) {
        log!("=== microvm started ===");
        loop {
            match self.vcpu.run().expect("KVM_RUN failed") {
                VcpuExit::IoOut(port, data) => {
                    match port {
                        0x3F8..=0x3FF => {
                            let offset = port - 0x3F8;
                            for &byte in data {
                                self.serial.write(offset as u8, byte).ok();
                            }
                        }
                        _ => {}
                    }
                }
                VcpuExit::IoIn(port, data) => {
                    match port {
                        0x3F8..=0x3FF => {
                            let offset = port - 0x3F8;
                            data[0] = self.serial.read(offset as u8);
                        }
                        _ => data.fill(0),
                    }
                }
                VcpuExit::MmioRead(addr, data) => {
                    let val = match addr {
                        VIRTIO_FS_BASE..=0xD000_0FFF => {
                            let offset = addr - VIRTIO_FS_BASE;
                            self.virtiofs.mmio.read(offset, |off| self.virtiofs.config_read(off))
                        }
                        VIRTIO_NET_BASE..=0xD000_1FFF => {
                            if let Some(ref net) = self.virtionet {
                                let offset = addr - VIRTIO_NET_BASE;
                                net.mmio.read(offset, |off| net.config_read(off))
                            } else { 0 }
                        }
                        _ => { data.fill(0); continue; }
                    };
                    let bytes = val.to_le_bytes();
                    let len = data.len().min(4);
                    data[..len].copy_from_slice(&bytes[..len]);
                }
                VcpuExit::MmioWrite(addr, data) => {
                    let val = u32::from_le_bytes(
                        data[..4.min(data.len())].try_into().unwrap_or([0; 4])
                    );
                    match addr {
                        VIRTIO_FS_BASE..=0xD000_0FFF => {
                            let offset = addr - VIRTIO_FS_BASE;
                            self.virtiofs.mmio.write(offset, val, |_, _| {});
                            if offset == 0x044 && val == 1 {
                                let qs = self.virtiofs.mmio.queue_sel();
                                self.virtiofs.activate_queue(qs as usize);
                                // Start worker when the request queue (1) is activated
                                if qs == 1 {
                                    self.virtiofs.start_worker(&self.vm, self.memory.clone());
                                }
                            }
                            // QueueNotify (0x050) is handled by ioeventfd →
                            // worker thread. No dispatch needed here.
                        }
                        VIRTIO_NET_BASE..=0xD000_1FFF => {
                            if let Some(ref mut net) = self.virtionet {
                                let offset = addr - VIRTIO_NET_BASE;
                                let notify = net.mmio.write(offset, val, |_, _| {});
                                if offset == 0x044 && val == 1 {
                                    let qs = net.mmio.queue_sel();
                                    net.activate_queue(qs as usize);
                                    // Start RX worker once both queues are ready
                                    net.start_rx_worker(self.memory.clone());
                                }
                                if notify {
                                    net.process_tx(&self.memory);
                                    net.mmio.inject_irq();
                                }
                            }
                        }
                        _ => {}
                    }
                }
                VcpuExit::Hlt => {}
                VcpuExit::Shutdown => {
                    log!("Guest shutdown");
                    break;
                }
                VcpuExit::InternalError => {
                    log!("KVM internal error");
                    break;
                }
                exit => {
                    log!("Unhandled exit: {:?}", exit);
                    break;
                }
            }

            // After every vCPU exit, check event sources (non-blocking).
            // Currently both FUSE and NET signal via irqfd directly from
            // their worker threads, so the epoll drain is available for
            // future devices that need main-thread processing.
            for _token in self.poll.poll() {}
        }
    }
}

fn main() {
    log::init();
    s3_rootfs::set_log_fn(|msg| crate::log::write(msg));

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: microvm <kernel> <initrd> [net-interface]");
        std::process::exit(1);
    }

    let kernel_path = &args[1];
    let initrd_path = &args[2];
    let net_ifname = args.get(3).map(|s| s.as_str());

    match System::new(kernel_path, initrd_path, net_ifname) {
        Ok(mut system) => system.run(),
        Err(e) => eprintln!("Error: {e}"),
    }
}
