#[macro_use]
mod log;

mod cpu;
mod kernel;
mod memory;
mod mptable;
mod s3fs;
mod serial;
mod virtio_fs;
mod virtio_mmio;
mod vm;

use std::sync::Arc;

use kvm_ioctls::{VcpuExit, VcpuFd, VmFd};
use vm_memory::GuestMemoryMmap;

use serial::Serial;
use virtio_fs::VirtioFs;
use s3fs::S3Fs;

const VIRTIO_FS_BASE: u64 = 0xD000_0000;
const VIRTIO_FS_IRQ: u32 = 5;

struct System {
    vm: Arc<VmFd>,
    memory: GuestMemoryMmap,
    vcpu: VcpuFd,
    serial: Serial,
    virtiofs: VirtioFs<S3Fs>,
}

impl System {
    fn new(kernel_path: &str, initrd_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let (kvm, vm) = vm::create_vm()?;
        let vm = Arc::new(vm);
        let memory = memory::setup_guest_memory(&vm)?;
        mptable::write(&memory)?;
        let vcpu = cpu::create_vcpu(&kvm, &vm, &memory)?;

        let (kernel_entry, boot_params_addr) = kernel::load(&memory, kernel_path, initrd_path)?;
        cpu::setup_regs(&vcpu, kernel_entry, boot_params_addr)?;

        let serial = serial::create(vm.clone());
        let s3fs = S3Fs::new(
            "http://localhost:9090",
            "rootfs",
            "minioadmin",
            "minioadmin",
        );
        let virtiofs = VirtioFs::new(s3fs, "rootfs", VIRTIO_FS_IRQ);

        Ok(Self { vm, memory, vcpu, serial, virtiofs })
    }

    fn run(&mut self) {
        log!("=== microvm started ===");
        // Test: read the magic value to verify MMIO works
        let magic = self.virtiofs.mmio.read(0x000, |_| 0);
        log!("virtio-fs magic test: 0x{:x}", magic);
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
                    match addr {
                        VIRTIO_FS_BASE..=0xD000_0FFF => {
                            let offset = addr - VIRTIO_FS_BASE;
                            let val = self.virtiofs.mmio.read(offset, |off| {
                                self.virtiofs.config_read(off)
                            });
                            if offset >= 0x100 {
                                log!("MMIO R config[0x{:x}] = 0x{:08x}", offset - 0x100, val);
                            } else {
                                log!("MMIO R reg[0x{:03x}] = 0x{:08x}", offset, val);
                            }
                            let bytes = val.to_le_bytes();
                            let len = data.len().min(4);
                            data[..len].copy_from_slice(&bytes[..len]);
                        }
                        _ => data.fill(0),
                    }
                }
                VcpuExit::MmioWrite(addr, data) => {
                    match addr {
                        VIRTIO_FS_BASE..=0xD000_0FFF => {
                            let offset = addr - VIRTIO_FS_BASE;
                            let val = u32::from_le_bytes(
                                data[..4.min(data.len())]
                                    .try_into()
                                    .unwrap_or([0; 4])
                            );
                            log!("MMIO W reg[0x{:03x}] = 0x{:08x}", offset, val);
                            let notify = self.virtiofs.mmio.write(offset, val, |_, _| {});

                            if offset == 0x044 && val == 1 {
                                let queue_sel = self.virtiofs.mmio.queue_sel();
                                log!("  queue {} activated", queue_sel);
                                self.virtiofs.activate_queue(queue_sel as usize);
                            }

                            if notify {
                                log!("  queue notify -> processing FUSE");
                                self.virtiofs.process_queue(&self.memory);
                                self.virtiofs.mmio.interrupt_status |= 0x01;
                                self.vm.set_irq_line(VIRTIO_FS_IRQ, true).ok();
                                self.vm.set_irq_line(VIRTIO_FS_IRQ, false).ok();
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
        }
    }
}

fn main() {
    log::init();

    let kernel_path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: microvm <kernel> <initrd>");
        std::process::exit(1);
    });
    let initrd_path = std::env::args().nth(2).unwrap_or_else(|| {
        eprintln!("Usage: microvm <kernel> <initrd>");
        std::process::exit(1);
    });

    match System::new(&kernel_path, &initrd_path) {
        Ok(mut system) => system.run(),
        Err(e) => eprintln!("Error: {e}"),
    }
}
