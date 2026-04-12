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
    fn new(kernel_path: &str, initrd_path: &str, net_ifname: Option<&str>, mem_mb: usize, s3_endpoint: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let (kvm, vm) = vm::create_vm()?;
        let vm = Arc::new(vm);
        let memory = memory::setup_guest_memory(&vm, mem_mb)?;
        mptable::write(&memory)?;
        let vcpu = cpu::create_vcpu(&kvm, &vm, &memory)?;

        let (kernel_entry, boot_params_addr) = kernel::load(&memory, kernel_path, initrd_path, mem_mb * 1024 * 1024)?;
        cpu::setup_regs(&vcpu, kernel_entry, boot_params_addr)?;

        let serial = serial::create(&vm);
        let mut s3_config = S3Config::default();
        s3_config.endpoint = s3_endpoint.to_string();
        let s3fs = S3Fs::new(s3_config);
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
        self.serial.start_stdin_thread();
        loop {
            match self.vcpu.run().expect("KVM_RUN failed") {
                VcpuExit::IoOut(port, data) => {
                    match port {
                        0x3F8..=0x3FF => {
                            let offset = port - 0x3F8;
                            for &byte in data {
                                self.serial.write(offset as u8, byte);
                            }
                        }
                        0x64 if data.first() == Some(&0xFE) => {
                            log!("Guest requested shutdown (i8042 reset)");
                            break;
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
                                }
                                if offset == 0x070 && (val & 0x04) != 0 {
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

            // RX is handled by the worker thread via irqfd.
            // TX completions are signaled inline above.
        }
    }
}

fn setup_raw_terminal() -> Option<libc::termios> {
    unsafe {
        let mut orig: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
            return None;
        }
        let mut raw = orig;
        libc::cfmakeraw(&mut raw);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        Some(orig)
    }
}

fn restore_terminal(orig: &libc::termios) {
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, orig);
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn main() {
    log::init();
    s3_rootfs::set_log_fn(|msg| crate::log::write(msg));

    let kernel_path = env_or("VM_KERNEL", "dist/vmlinux");
    let initrd_path = env_or("VM_INITRD", "dist/initramfs.cpio.gz");
    let net_ifname = std::env::var("VM_NET").ok();
    let mem_mb: usize = env_or("VM_MEMORY", "1024").parse().unwrap_or(1024);
    let s3_endpoint = env_or("VM_S3_ENDPOINT", "http://192.168.88.15:9090");

    log!("Config: memory={}MB kernel={} initrd={} net={:?} s3={}",
        mem_mb, kernel_path, initrd_path, net_ifname, s3_endpoint);

    let orig_term = setup_raw_terminal();

    match System::new(&kernel_path, &initrd_path, net_ifname.as_deref(), mem_mb, &s3_endpoint) {
        Ok(mut system) => system.run(),
        Err(e) => eprintln!("Error: {e}"),
    }

    if let Some(ref orig) = orig_term {
        restore_terminal(orig);
    }
}
