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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use kvm_ioctls::{VcpuExit, VcpuFd, VmFd};
use vm_memory::GuestMemoryMmap;
use xdp_net::{MacvtapBackend, NetBackend};

use s3_rootfs::S3Fs;

use crate::event_loop::EventPoll;
use crate::serial::Serial;
use crate::virtio_blk::VirtioBlk;
use crate::virtio_console::VirtioConsole;
use crate::virtio_fs::{FsDevice, VirtioFs};
use crate::virtio_net::VirtioNet;
use crate::{cpu, kernel, memory, mptable, serial, vm};

// MMIO device addresses — one 4KB page per device starting at 0xD000_0000.
const VIRTIO_FS_BASE: u64 = 0xD000_0000;
const VIRTIO_FS_IRQ: u32 = 5;
const VIRTIO_NET_BASE: u64 = 0xD000_1000;
const VIRTIO_NET_IRQ: u32 = 6;
const VIRTIO_CONSOLE_BASE: u64 = 0xD000_2000;
const VIRTIO_CONSOLE_IRQ: u32 = 7;
// Disks: one page per device from here, IRQs 11, 12, ...
const BLK_BASE: u64 = 0xD000_4000;
const BLK_IRQ0: u32 = 11;
const MAX_DISKS: usize = 4;
/// A virtio-blk backing file. The first disk is `/dev/vda`.
pub struct DiskSpec {
    pub path: std::path::PathBuf,
    pub read_only: bool,
}

// ── Config ─────────────────────────────────────────────────────────────

/// Everything needed to bring a VM up.
///
/// This is what an orchestrator (or `main`) hands to the launcher.
/// Kept as a plain struct; `Default` isn't meaningful here because
/// `kernel_path` and `initrd_path` have no sensible defaults.
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
    /// vCPUs. One thread each; devices are shared behind one lock.
    pub vcpus: usize,
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
    /// Optional file path for serial console output.
    pub log_file: Option<std::path::PathBuf>,
    /// Custom console input source. When set, the virtio-console RX
    /// worker reads from this instead of stdin. Used by the
    /// orchestrator to inject commands (e.g. `poweroff` for graceful
    /// shutdown). None = stdin (standalone interactive mode).
    pub console_input: Option<Box<dyn std::io::Read + Send>>,
    /// Block devices, in order. With any disk present the guest boots
    /// from the initrd without a virtiofs root.
    pub disks: Vec<DiskSpec>,
    /// Appended verbatim to the kernel command line (e.g. `systemd.hostname=`).
    pub cmdline_extra: String,
    /// Guest MAC for virtio-net. Must match the macvtap's own address, since
    /// macvtap filters on it. `None` keeps the historical fixed MAC.
    pub net_mac: Option<[u8; 6]>,
    /// Host directory served to the guest over virtio-fs with tag `work`,
    /// the interim path until the sharedfs guest driver exists. Only with
    /// disks present (it takes the virtio-fs slot the S3 root used).
    pub virtiofs_root: Option<std::path::PathBuf>,
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

/// Everything that answers a guest I/O exit. One lock around all of it:
/// exits are rare relative to guest work, and a single lock keeps the
/// device code free of per-device synchronization.
struct Devices {
    serial: Serial,
    console: VirtioConsole,
    virtiofs: Option<Box<dyn FsDevice + Send>>,
    virtionet: Option<VirtioNet<MacvtapBackend>>,
    disks: Vec<VirtioBlk>,
}

/// In-memory representation of a running VM: KVM handles, guest RAM,
/// vCPUs, and the virtio devices wired into MMIO space.
struct System {
    vm: Arc<VmFd>,
    memory: GuestMemoryMmap,
    vcpus: Vec<VcpuFd>,
    devices: Arc<Mutex<Devices>>,
    #[allow(dead_code)]
    poll: EventPoll,
    /// Host-side shutdown control. Checked between KVM_RUN iterations.
    shutdown: Option<Arc<AtomicBool>>,
}

enum Flow {
    Continue,
    Stop,
}

impl System {
    fn new(cfg: LaunchConfig, fs: Arc<S3Fs>) -> Result<Self, Box<dyn std::error::Error>> {
        let (kvm, vm) = vm::create_vm()?;
        let vm = Arc::new(vm);

        let memory = memory::setup_guest_memory(&vm, cfg.mem_mb)?;
        let ncpu = cfg.vcpus.clamp(1, 64);
        mptable::write(&memory, ncpu as u8)?;
        let x2apic = cpu::guest_x2apic();
        log!("guest APIC: {}", if x2apic { "x2APIC" } else { "xAPIC, for AVIC" });
        let mut vcpus = Vec::with_capacity(ncpu);
        for id in 0..ncpu {
            vcpus.push(cpu::create_vcpu(&kvm, &vm, &memory, id as u8, x2apic)?);
        }

        let mut disks = Vec::new();
        for (i, d) in cfg.disks.iter().take(MAX_DISKS).enumerate() {
            let file = std::fs::OpenOptions::new().read(true).write(!d.read_only).open(&d.path)?;
            let irq = BLK_IRQ0 + i as u32;
            let dev = VirtioBlk::new(file, d.read_only, irq, &format!("env-vd{}", (b'a' + i as u8) as char))?;
            vm.register_irqfd(dev.mmio.irqfd_fd(), irq)?;
            log!("disk vd{}: {} ({})", (b'a' + i as u8) as char, d.path.display(), if d.read_only { "ro" } else { "rw" });
            disks.push(dev);
        }

        // Device lines the base command line does not know about, then the root.
        let mut extra = String::new();
        for i in 0..disks.len() {
            extra.push_str(&format!(" virtio_mmio.device=4K@0x{:x}:{}", BLK_BASE + i as u64 * 0x1000, BLK_IRQ0 + i as u32));
        }
        if disks.is_empty() {
            extra.push_str(" rootfstype=virtiofs root=rootfs rw systemd.unit=multi-user.target systemd.show_status=no");
        } else {
            extra.push_str(" rw");
        }
        if !cfg.cmdline_extra.is_empty() {
            extra.push(' ');
            extra.push_str(&cfg.cmdline_extra);
        }

        let (kernel_entry, boot_params_addr) =
            kernel::load(&memory, &cfg.kernel_path, &cfg.initrd_path, cfg.mem_mb * 1024 * 1024, &extra)?;
        cpu::setup_regs(&vcpus[0], kernel_entry, boot_params_addr)?;

        // Serial (ttyS0): output-only for early boot messages before
        // virtio-console comes up. Output goes to log_file if set,
        // otherwise stdout.
        let serial_output: Box<dyn std::io::Write + Send> = match &cfg.log_file {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap_or_else(|e| {
                    log!("serial log open failed ({}), falling back to /dev/null: {}", path.display(), e);
                    std::fs::File::open("/dev/null").unwrap()
                });
                Box::new(f)
            }
            None => Box::new(std::io::stdout()),
        };
        let serial = serial::create(&vm, serial_output);

        // Virtio-console (hvc0): interactive post-boot console.
        let console_input: Box<dyn std::io::Read + Send> =
            cfg.console_input.unwrap_or_else(|| Box::new(std::io::stdin()));
        let console = VirtioConsole::new(VIRTIO_CONSOLE_IRQ, VIRTIO_CONSOLE_BASE, console_input, Box::new(std::io::stdout()));
        vm.register_irqfd(console.mmio.irqfd_fd(), VIRTIO_CONSOLE_IRQ)?;

        // Virtio-fs: with disks, a passthrough of a host directory (tag
        // "work"); without, the legacy S3-backed root (tag "rootfs").
        let virtiofs: Option<Box<dyn FsDevice + Send>> = if cfg.disks.is_empty() {
            Some(Box::new(VirtioFs::new(fs, "rootfs", VIRTIO_FS_IRQ, VIRTIO_FS_BASE)))
        } else if let Some(root) = &cfg.virtiofs_root {
            let pfs = fuse_backend_rs::passthrough::PassthroughFs::<()>::new(fuse_backend_rs::passthrough::Config {
                root_dir: root.to_string_lossy().into_owned(),
                do_import: true,
                ..Default::default()
            })?;
            pfs.import()?;
            log!("virtiofs work: {}", root.display());
            Some(Box::new(VirtioFs::new(Arc::new(pfs), "work", VIRTIO_FS_IRQ, VIRTIO_FS_BASE)))
        } else {
            None
        };
        if let Some(d) = &virtiofs {
            vm.register_irqfd(d.mmio().irqfd_fd(), VIRTIO_FS_IRQ)?;
        }

        // Virtio-net (eth0): optional, only if a host interface was given.
        let virtionet = if let Some(ref ifname) = cfg.net_ifname {
            log!("Creating virtio-net with interface {}", ifname);
            let backend = MacvtapBackend::new(ifname)?;
            let mac = cfg.net_mac.unwrap_or([0x06, 0x00, 0xAC, 0x10, 0xFF, 0x01]);
            let net = VirtioNet::new(backend, mac, VIRTIO_NET_IRQ);
            vm.register_irqfd(net.mmio.irqfd_fd(), VIRTIO_NET_IRQ)?;
            Some(net)
        } else {
            None
        };

        let devices = Arc::new(Mutex::new(Devices { serial, console, virtiofs, virtionet, disks }));
        Ok(Self { vm, memory, vcpus, devices, poll: EventPoll::new(), shutdown: cfg.shutdown })
    }

    fn run(&mut self) {
        log!("=== microvm started ({} vcpus) ===", self.vcpus.len());
        bench_ts("T2"); // vm-started — vCPU about to enter KVM_RUN
        install_kick_handler();

        let stop = Arc::new(AtomicBool::new(false));
        let threads: Arc<Mutex<Vec<libc::pthread_t>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        let mut vcpus = std::mem::take(&mut self.vcpus);
        let mut bsp = vcpus.remove(0);
        for (i, mut vcpu) in vcpus.into_iter().enumerate() {
            let (vm, memory, devices, stop, shutdown, threads) = (
                self.vm.clone(),
                self.memory.clone(),
                self.devices.clone(),
                stop.clone(),
                self.shutdown.clone(),
                threads.clone(),
            );
            handles.push(
                thread::Builder::new()
                    .name(format!("vcpu{}", i + 1))
                    .spawn(move || {
                        threads.lock().unwrap().push(unsafe { libc::pthread_self() });
                        vcpu_loop(&mut vcpu, &vm, &memory, &devices, &stop, shutdown.as_deref());
                    })
                    .expect("spawn vcpu thread"),
            );
        }
        vcpu_loop(&mut bsp, &self.vm, &self.memory, &self.devices, &stop, self.shutdown.as_deref());

        // The BSP's loop ended: stop the rest. A signal makes KVM_RUN return
        // EINTR, and the loop notices the flag.
        stop.store(true, Ordering::SeqCst);
        for t in threads.lock().unwrap().iter() {
            unsafe { libc::pthread_kill(*t, libc::SIGUSR1) };
        }
        for h in handles {
            h.join().ok();
        }
    }
}

extern "C" fn kick_handler(_: libc::c_int) {}

/// SIGUSR1 interrupts KVM_RUN without killing the process.
fn install_kick_handler() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = kick_handler as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
    }
}

/// Run one vCPU until the guest stops or the host asks us to.
fn vcpu_loop(
    vcpu: &mut VcpuFd,
    vm: &VmFd,
    memory: &GuestMemoryMmap,
    devices: &Mutex<Devices>,
    stop: &AtomicBool,
    shutdown: Option<&AtomicBool>,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(flag) = shutdown {
            if flag.load(Ordering::Relaxed) {
                log!("Host requested shutdown");
                stop.store(true, Ordering::SeqCst);
                break;
            }
        }
        let exit = match vcpu.run() {
            Ok(exit) => exit,
            Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => continue,
            Err(e) => panic!("KVM_RUN failed: {e}"),
        };
        let flow = handle_exit(&mut devices.lock().unwrap(), vm, memory, exit);
        if let Flow::Stop = flow {
            stop.store(true, Ordering::SeqCst);
            break;
        }
    }
}

fn handle_exit(d: &mut Devices, vm: &VmFd, memory: &GuestMemoryMmap, exit: VcpuExit<'_>) -> Flow {
    match exit {
        VcpuExit::IoOut(port, data) => match port {
            0x3F8..=0x3FF => {
                let offset = port - 0x3F8;
                for &byte in data {
                    d.serial.write(offset as u8, byte);
                }
            }
            0x64 if data.first() == Some(&0xFE) => {
                log!("Guest requested shutdown (i8042 reset)");
                return Flow::Stop;
            }
            _ => {}
        },
        VcpuExit::IoIn(port, data) => match port {
            0x3F8..=0x3FF => {
                let offset = port - 0x3F8;
                data[0] = d.serial.read(offset as u8);
            }
            _ => data.fill(0),
        },
        VcpuExit::MmioRead(addr, data) => {
            let val: Option<u32> = match addr {
                VIRTIO_FS_BASE..=0xD000_0FFF => Some(match d.virtiofs {
                    Some(ref f) => f.mmio().read(addr - VIRTIO_FS_BASE, |off| f.config_read(off)),
                    None => 0,
                }),
                VIRTIO_NET_BASE..=0xD000_1FFF => Some(match d.virtionet {
                    Some(ref net) => net.mmio.read(addr - VIRTIO_NET_BASE, |off| net.config_read(off)),
                    None => 0,
                }),
                VIRTIO_CONSOLE_BASE..=0xD000_2FFF => {
                    Some(d.console.mmio.read(addr - VIRTIO_CONSOLE_BASE, |off| d.console.config_read(off)))
                }
                BLK_BASE..=0xD000_7FFF => {
                    let i = ((addr - BLK_BASE) >> 12) as usize;
                    Some(d.disks.get(i).map_or(0, |b| b.mmio.read(addr & 0xFFF, |off| b.config_read(off))))
                }
                _ => None,
            };
            match val {
                Some(v) => {
                    let bytes = v.to_le_bytes();
                    let len = data.len().min(4);
                    data[..len].copy_from_slice(&bytes[..len]);
                }
                None => data.fill(0),
            }
        }
        VcpuExit::MmioWrite(addr, data) => {
            let val = u32::from_le_bytes(data[..4.min(data.len())].try_into().unwrap_or([0; 4]));
            match addr {
                VIRTIO_FS_BASE..=0xD000_0FFF => {
                    if let Some(ref mut f) = d.virtiofs {
                        let offset = addr - VIRTIO_FS_BASE;
                        f.mmio_mut().write(offset, val, |_, _| {});
                        if offset == 0x044 && val == 1 {
                            let qs = f.mmio().queue_sel();
                            f.activate_queue(qs as usize);
                            // Start worker when the request queue (1) is activated.
                            if qs == 1 {
                                f.start_worker(vm, Arc::new(memory.clone()));
                            }
                        }
                    }
                }
                VIRTIO_NET_BASE..=0xD000_1FFF => {
                    if let Some(ref mut net) = d.virtionet {
                        let offset = addr - VIRTIO_NET_BASE;
                        let notify = net.mmio.write(offset, val, |_, _| {});
                        if offset == 0x070 && val == 0 {
                            net.reset();
                        }
                        if offset == 0x044 && val == 1 {
                            let qs = net.mmio.queue_sel();
                            net.activate_queue(qs as usize);
                        }
                        if offset == 0x070 && (val & 0x04) != 0 {
                            net.start_rx_worker(memory.clone());
                        }
                        if notify {
                            net.process_tx(memory);
                            net.mmio.inject_irq();
                        }
                    }
                }
                VIRTIO_CONSOLE_BASE..=0xD000_2FFF => {
                    let offset = addr - VIRTIO_CONSOLE_BASE;
                    let notify = d.console.mmio.write(offset, val, |_, _| {});
                    if offset == 0x070 && val == 0 {
                        d.console.reset();
                    }
                    if offset == 0x044 && val == 1 {
                        let qs = d.console.mmio.queue_sel();
                        d.console.activate_queue(qs as usize);
                    }
                    if offset == 0x070 && (val & 0x04) != 0 {
                        d.console.start_rx_worker(memory.clone());
                    }
                    if notify {
                        d.console.process_tx(memory);
                        d.console.mmio.inject_irq();
                    }
                }
                BLK_BASE..=0xD000_7FFF => {
                    let i = ((addr - BLK_BASE) >> 12) as usize;
                    if let Some(b) = d.disks.get_mut(i) {
                        let offset = addr & 0xFFF;
                        let was_ready = b.mmio.is_driver_ready();
                        let notify = b.mmio.write(offset, val, |_, _| {});
                        if offset == 0x070 && val == 0 {
                            b.reset();
                        }
                        if offset == 0x070 && !was_ready && b.mmio.is_driver_ready() {
                            b.driver_ready();
                        }
                        if offset == 0x044 && val == 1 {
                            let qs = b.mmio.queue_sel();
                            b.activate_queue(qs as usize);
                        }
                        if notify && b.process(memory) {
                            b.mmio.inject_irq();
                        }
                    }
                }
                _ => {}
            }
        }
        VcpuExit::Hlt => {}
        VcpuExit::Shutdown => {
            log!("Guest shutdown");
            return Flow::Stop;
        }
        VcpuExit::InternalError => {
            log!("KVM internal error");
            return Flow::Stop;
        }
        exit => {
            log!("Unhandled exit: {:?}", exit);
            return Flow::Stop;
        }
    }
    Flow::Continue
}
