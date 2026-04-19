//! VM launcher: wires config → filesystem → VM → devices → vCPU loop.
//!
//! This is the seam where an orchestrator will eventually pry apart
//! "build the filesystem" from "run the VM". Today everything happens
//! in one process on the call stack; tomorrow `prepare_filesystem`
//! moves into a host-wide daemon and `launch_vm` receives an
//! already-built `Arc<S3Fs>` over the process boundary via shared
//! memory / Unix FD passing. The API here is designed so that
//! split happens with minimal disruption — no wire format redesign,
//! just a different caller.
//!
//! No IPC here. Filesystem and VMM live in the same process, share
//! memory via `Arc<S3Fs>`, and coordinate via direct function calls.
//! Future orchestrator work will add another consumer of the `Arc<S3Fs>`
//! (e.g. for cache invalidation on host-side writes), not serialize
//! anything across a socket.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kvm_ioctls::{VcpuExit, VcpuFd, VmFd};
use vm_memory::GuestMemoryMmap;
use xdp_net::{MacvtapBackend, NetBackend};

use s3_rootfs::S3Fs;

use crate::event_loop::EventPoll;
use crate::serial::Serial;
use crate::virtio_console::VirtioConsole;
use crate::virtio_fs::VirtioFs;
use crate::virtio_net::VirtioNet;
use crate::{cpu, kernel, memory, mptable, serial, vm};

// MMIO device addresses — one 4KB page per device starting at 0xD000_0000.
const VIRTIO_FS_BASE: u64 = 0xD000_0000;
const VIRTIO_FS_IRQ: u32 = 5;
const VIRTIO_NET_BASE: u64 = 0xD000_1000;
const VIRTIO_NET_IRQ: u32 = 6;
const VIRTIO_CONSOLE_BASE: u64 = 0xD000_2000;
const VIRTIO_CONSOLE_IRQ: u32 = 7;

// ── Config ─────────────────────────────────────────────────────────────

/// Everything needed to bring a VM up.
///
/// This is what an orchestrator (or `main`) hands to the launcher.
/// Kept as a plain struct; `Default` isn't meaningful here because
/// `kernel_path` and `initrd_path` have no sensible defaults.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// Path to the kernel image (ELF vmlinux).
    pub kernel_path: String,
    /// Path to the initramfs cpio archive.
    pub initrd_path: String,
    /// Host interface name for virtio-net (macvtap). `None` disables
    /// networking entirely.
    pub net_ifname: Option<String>,
    /// Guest RAM in MB.
    pub mem_mb: usize,
    /// Identity for this VM instance. Used today only in log lines;
    /// reserved for future multi-VM orchestrator distinctions
    /// (e.g. `thread_name: format!("vm-fuse-{vm_id}")`).
    pub vm_id: String,
    /// Filesystem configuration (bucket, caching, TTLs).
    pub fs: s3_rootfs::config::FsConfig,
    /// Host-side shutdown flag. When set to true, the vcpu loop
    /// exits at the next iteration. The orchestrator's stop_vm
    /// sets this, then signals the vcpu thread to break out of
    /// KVM_RUN. `None` means no external shutdown control (the
    /// standalone binary doesn't use this).
    pub shutdown: Option<Arc<AtomicBool>>,
}

// ── Filesystem bring-up ────────────────────────────────────────────────

/// Build the `S3Fs` from config.
///
/// Isolated as a separate function because in the eventual orchestrator
/// split this is exactly the call that migrates out-of-process: a
/// host daemon owns the filesystem and hands `Arc<S3Fs>` to per-VM
/// launchers on demand.
pub fn prepare_filesystem(cfg: &LaunchConfig) -> Arc<S3Fs> {
    Arc::new(S3Fs::new(cfg.fs.clone()))
}

// ── VM bring-up ────────────────────────────────────────────────────────

/// Full launch path: build the filesystem, then run the VM.
///
/// The split between `prepare_filesystem` and `launch_vm` is the seam
/// the orchestrator will eventually cut along.
pub fn run(cfg: LaunchConfig) -> Result<(), Box<dyn std::error::Error>> {
    bench_ts("T0"); // orchestrator-start
    let fs = prepare_filesystem(&cfg);
    bench_ts("T1"); // golden-loaded
    launch_vm(cfg, fs)
}

// ── Boot-time bench instrumentation ────────────────────────────────────
//
// Three timestamps mark the boot pipeline for `dist/bench-boot.sh`:
//   T0  orchestrator-start     — top of `run`, before any work.
//   T1  golden-loaded          — after `prepare_filesystem` returns
//                                (S3Fs + optional golden caches ready).
//   T2  vm-started             — vCPU about to run (inside `System::run`).
//
// Harness computes:
//   orchestrator_init = T1 - T0   (excluded from boot number)
//   boot_time         = t_login - T2   (THE metric; research/12 §10)
//   total             = t_login - T0
//
// Prefix `__BENCH_TN__` chosen to never collide with guest console
// output or any other microvm log line; the harness greps on it
// without false positives.
fn bench_ts(marker: &str) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // stderr so it stays separate from guest console output (stdout).
    eprintln!("__BENCH_{marker}__ {nanos}");
}

/// Bring up a VM and run its vCPU loop to completion.
///
/// Takes `Arc<S3Fs>` rather than building it internally so future
/// callers (the orchestrator) can hold their own reference to the
/// same filesystem — e.g. for cache invalidation on host-side writes.
pub fn launch_vm(cfg: LaunchConfig, fs: Arc<S3Fs>) -> Result<(), Box<dyn std::error::Error>> {
    let mut system = System::new(cfg, fs)?;
    system.run();
    Ok(())
}

// ── VM instance ────────────────────────────────────────────────────────

/// Thread ID of the vcpu thread, stored after spawn for signal-based
/// KVM_RUN interruption. The orchestrator sends SIGUSR1 to kick the
/// vcpu out of a blocking KVM_RUN when the shutdown flag is set.
///
/// In-memory representation of a running VM: KVM handles, guest RAM,
/// vCPU, and the virtio devices wired into MMIO space.
///
/// Private to this module — external callers go through `launch_vm`/`run`.
struct System {
    vm: Arc<VmFd>,
    memory: GuestMemoryMmap,
    vcpu: VcpuFd,
    serial: Serial,
    console: VirtioConsole,
    virtiofs: VirtioFs,
    virtionet: Option<VirtioNet<MacvtapBackend>>,
    // epoll instance reserved for a future multi-fd main-thread event
    // loop. Currently unused: each device spawns its own blocking
    // worker thread, so the vCPU thread only waits on KVM_RUN. Once
    // we consolidate device I/O (e.g. ioeventfd/irqfd management,
    // orchestrator control channel), this becomes the single place
    // the VMM thread parks. Was already dead in main.rs pre-refactor.
    // TODO: wire into launch_vm when that consolidation lands.
    #[allow(dead_code)]
    poll: EventPoll,
    /// Host-side shutdown control. Checked between KVM_RUN iterations.
    shutdown: Option<Arc<AtomicBool>>,
}

impl System {
    fn new(
        cfg: LaunchConfig,
        fs: Arc<S3Fs>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (kvm, vm) = vm::create_vm()?;
        let vm = Arc::new(vm);

        let memory = memory::setup_guest_memory(&vm, cfg.mem_mb)?;
        mptable::write(&memory)?;
        let vcpu = cpu::create_vcpu(&kvm, &vm, &memory)?;

        let (kernel_entry, boot_params_addr) =
            kernel::load(&memory, &cfg.kernel_path, &cfg.initrd_path, cfg.mem_mb * 1024 * 1024)?;
        cpu::setup_regs(&vcpu, kernel_entry, boot_params_addr)?;

        // Serial (ttyS0): output-only for early boot messages before
        // virtio-console comes up.
        let serial = serial::create(&vm);

        // Virtio-console (hvc0): interactive post-boot console.
        let console = VirtioConsole::stdio(VIRTIO_CONSOLE_IRQ, VIRTIO_CONSOLE_BASE);
        vm.register_irqfd(console.mmio.irqfd_fd(), VIRTIO_CONSOLE_IRQ)?;

        // Virtio-fs (rootfs): backed by S3Fs via virtio-fs-transport worker.
        let virtiofs = VirtioFs::new(fs, "rootfs", VIRTIO_FS_IRQ, VIRTIO_FS_BASE);
        vm.register_irqfd(virtiofs.mmio.irqfd_fd(), VIRTIO_FS_IRQ)?;

        // Virtio-net (eth0): optional, only if a host interface was given.
        let virtionet = if let Some(ref ifname) = cfg.net_ifname {
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

        Ok(Self { vm, memory, vcpu, serial, console, virtiofs, virtionet, poll, shutdown: cfg.shutdown })
    }

    fn run(&mut self) {
        log!("=== microvm started ===");
        bench_ts("T2"); // vm-started — vCPU about to enter KVM_RUN
        // stdin goes to virtio-console (hvc0), not serial (ttyS0).
        // Serial is output-only for boot messages.
        loop {
            // Host-side shutdown check.
            if let Some(ref flag) = self.shutdown {
                if flag.load(Ordering::Relaxed) {
                    log!("Host requested shutdown");
                    break;
                }
            }
            let exit = match self.vcpu.run() {
                Ok(exit) => exit,
                Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => {
                    continue; // interrupted by signal (shutdown kick)
                }
                Err(e) => panic!("KVM_RUN failed: {e}"),
            };
            match exit {
                VcpuExit::IoOut(port, data) => match port {
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
                },
                VcpuExit::IoIn(port, data) => match port {
                    0x3F8..=0x3FF => {
                        let offset = port - 0x3F8;
                        data[0] = self.serial.read(offset as u8);
                    }
                    _ => data.fill(0),
                },
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
                            } else {
                                0
                            }
                        }
                        VIRTIO_CONSOLE_BASE..=0xD000_2FFF => {
                            let offset = addr - VIRTIO_CONSOLE_BASE;
                            self.console.mmio.read(offset, |off| self.console.config_read(off))
                        }
                        _ => {
                            data.fill(0);
                            continue;
                        }
                    };
                    let bytes = val.to_le_bytes();
                    let len = data.len().min(4);
                    data[..len].copy_from_slice(&bytes[..len]);
                }
                VcpuExit::MmioWrite(addr, data) => {
                    let val = u32::from_le_bytes(data[..4.min(data.len())].try_into().unwrap_or([0; 4]));
                    match addr {
                        VIRTIO_FS_BASE..=0xD000_0FFF => {
                            let offset = addr - VIRTIO_FS_BASE;
                            self.virtiofs.mmio.write(offset, val, |_, _| {});
                            if offset == 0x044 && val == 1 {
                                let qs = self.virtiofs.mmio.queue_sel();
                                self.virtiofs.activate_queue(qs as usize);
                                // Start worker when the request queue (1) is activated.
                                if qs == 1 {
                                    // Wrap into Arc because virtio-fs-transport's
                                    // FsSession<F> requires Arc<GuestMemoryMmap>
                                    // — the worker lives in a separate crate and
                                    // can't assume anything about guest-memory
                                    // ownership on the VMM side. Net/console
                                    // workers are VM-local (microvm crate) and
                                    // pass GuestMemoryMmap directly via its cheap
                                    // internal Arc-based Clone.
                                    self.virtiofs.start_worker(&self.vm, Arc::new(self.memory.clone()));
                                }
                            }
                            // QueueNotify (0x050) goes to the worker via ioeventfd — no vCPU exit.
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
                        VIRTIO_CONSOLE_BASE..=0xD000_2FFF => {
                            let offset = addr - VIRTIO_CONSOLE_BASE;
                            let notify = self.console.mmio.write(offset, val, |_, _| {});
                            if offset == 0x044 && val == 1 {
                                let qs = self.console.mmio.queue_sel();
                                self.console.activate_queue(qs as usize);
                            }
                            // Start RX worker on DRIVER_OK.
                            if offset == 0x070 && (val & 0x04) != 0 {
                                self.console.start_rx_worker(self.memory.clone());
                            }
                            if notify {
                                self.console.process_tx(&self.memory);
                                self.console.mmio.inject_irq();
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
            // TX completions are signalled inline above.
        }
    }
}

