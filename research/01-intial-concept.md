# Minimal KVM VM Manager — rust-vmm Crate Reference (Rust)

Same architecture as the raw syscall reference, but using rust-vmm crates for
KVM interaction, guest memory, and kernel loading. Everything else — virtio
devices, serial console, TAP networking, the run loop — stays hand-rolled.

The ethos: use community crates where they eliminate boilerplate struct definitions
and unsafe ioctl wrappers. Write everything else ourselves.

---

## Dependencies

```toml
[package]
name = "microvm"
version = "0.1.0"
edition = "2024"

[dependencies]
kvm-bindings = { version = "0.13", features = ["fam-wrappers"] }
kvm-ioctls = "0.23"
vm-memory = { version = "0.16", features = ["backend-mmap"] }
linux-loader = { version = "0.14", features = ["bzimage", "elf", "cmdline"] }
nix = { version = "0.30", features = ["fs", "mman", "ioctl", "net", "poll"] }
libc = "0.2"
```

### What each crate gives us

| Crate           | What we use it for                             | What we still do ourselves        |
|-----------------|------------------------------------------------|-----------------------------------|
| `kvm-ioctls`    | `Kvm`, `VmFd`, `VcpuFd`, `VcpuExit` enum      | Run loop logic, exit handling     |
| `kvm-bindings`  | All KVM structs (`kvm_regs`, `kvm_sregs`, etc) | Setting the actual register values|
| `vm-memory`     | `GuestMemoryMmap`, `GuestAddress`, read/write  | CoW mapping, golden image logic   |
| `linux-loader`  | bzImage parsing, cmdline, boot params          | Memory layout decisions           |
| `nix`           | `mmap`, `poll`, TAP ioctl helpers              | Everything — this is just libc++  |
| `libc`          | Constants and types nix doesn't cover          | —                                 |

### What we intentionally do NOT pull in

- `virtio-bindings` / `virtio-queue` / `virtio-devices` (we write our own)
- `vm-superio` (serial emulation — we write our own)
- `event-manager` (we write our own event loop)
- `vmm-sys-util` (we use nix directly)

---

## 1. KVM Subsystem Setup

### 1.1 Open KVM and create VM

```rust
use kvm_ioctls::{Kvm, VmFd, VcpuFd, VcpuExit};
use kvm_bindings::kvm_pit_config;

fn create_vm() -> (Kvm, VmFd) {
    let kvm = Kvm::new().expect("Failed to open /dev/kvm");

    // Kvm::new() internally:
    //   open("/dev/kvm", O_RDWR | O_CLOEXEC)
    //   ioctl(fd, KVM_GET_API_VERSION) — asserts == 12

    // Check capabilities we need
    assert!(kvm.check_extension(kvm_ioctls::Cap::UserMemory));
    assert!(kvm.check_extension(kvm_ioctls::Cap::Irqchip));
    assert!(kvm.check_extension(kvm_ioctls::Cap::SetTssAddr));

    let vm = kvm.create_vm().expect("Failed to create VM");
    // Internally: ioctl(kvm_fd, KVM_CREATE_VM, 0)

    // x86_64 setup
    vm.set_tss_address(0xFFFB_D000)
        .expect("Failed to set TSS address");

    vm.create_irq_chip()
        .expect("Failed to create in-kernel irqchip");

    let pit_config = kvm_pit_config::default();
    vm.create_pit2(pit_config)
        .expect("Failed to create PIT");

    (kvm, vm)
}
```

**What the crate handles**: Opening `/dev/kvm`, API version check, ioctl
dispatch, error wrapping. All the raw ioctl numbers and fd management
are hidden behind typed methods.

**What we still control**: Which capabilities to check, TSS address
choice, PIT configuration.

---

## 2. Provisioning RAM

### 2.1 Guest memory with vm-memory

```rust
use vm_memory::{GuestMemoryMmap, GuestAddress, GuestMemory, GuestMemoryRegion};
use kvm_bindings::kvm_userspace_memory_region;

const GUEST_MEM_SIZE: usize = 256 * 1024 * 1024; // 256MB

fn setup_guest_memory(vm: &VmFd) -> GuestMemoryMmap {
    // GuestMemoryMmap handles the mmap internally
    // This calls mmap with MAP_PRIVATE | MAP_ANONYMOUS
    let guest_memory = GuestMemoryMmap::from_ranges(
        &[(GuestAddress(0), GUEST_MEM_SIZE)]
    ).expect("Failed to create guest memory");

    // Register each region with KVM
    guest_memory.iter().enumerate().for_each(|(index, region)| {
        let mem_region = kvm_userspace_memory_region {
            slot: index as u32,
            flags: 0,
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.len(),
            userspace_addr: guest_memory
                .get_host_address(region.start_addr())
                .unwrap() as u64,
        };

        unsafe {
            vm.set_user_memory_region(mem_region)
                .expect("Failed to set memory region");
        }
    });

    guest_memory
}
```

**What vm-memory gives us**: Type-safe `GuestAddress`, bounds-checked
read/write into guest memory, region management, host pointer lookup.

**What we'd do differently for golden image CoW**: The `from_ranges`
constructor always does a fresh anonymous mmap. For CoW from a golden
image, we'd bypass this and create the mapping ourselves:

```rust
use nix::sys::mman::{mmap_anonymous, mmap, MapFlags, ProtFlags};
use std::num::NonZeroUsize;

/// Create guest memory as a CoW mapping of an existing golden image.
/// The golden image is an fd to a file (or memfd) containing the
/// pre-booted memory snapshot.
fn setup_cow_memory(
    vm: &VmFd,
    golden_image_fd: std::os::fd::RawFd,
    size: usize,
) -> (*mut u8, usize) {
    let host_ptr = unsafe {
        mmap(
            None,
            NonZeroUsize::new(size).unwrap(),
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_PRIVATE | MapFlags::MAP_NORESERVE,
            Some(std::os::fd::BorrowedFd::borrow_raw(golden_image_fd)),
            0,
        ).expect("Failed to mmap golden image")
    };

    let mem_region = kvm_userspace_memory_region {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size: size as u64,
        userspace_addr: host_ptr.as_ptr() as u64,
    };

    unsafe {
        vm.set_user_memory_region(mem_region)
            .expect("Failed to set memory region");
    }

    (host_ptr.as_ptr() as *mut u8, size)
}
```

### 2.2 Reading and writing guest memory

vm-memory provides bounds-checked access. We use this everywhere we
need to interact with guest memory from the host side (loading kernel,
virtio descriptor processing, etc):

```rust
use vm_memory::{Bytes, GuestAddress};

// Write bytes into guest memory at a guest physical address
guest_memory.write_slice(&data, GuestAddress(0x100000))
    .expect("Failed to write to guest memory");

// Read bytes from guest memory
let mut buf = vec![0u8; 512];
guest_memory.read_slice(&mut buf, GuestAddress(some_gpa))
    .expect("Failed to read from guest memory");

// Write a struct into guest memory
guest_memory.write_obj(my_struct, GuestAddress(some_gpa))
    .expect("Failed to write struct");

// Read a struct from guest memory
let val: u64 = guest_memory.read_obj(GuestAddress(some_gpa))
    .expect("Failed to read obj");

// Get raw host pointer for a guest address (for KVM register setup, etc)
let host_addr = guest_memory.get_host_address(GuestAddress(0))
    .expect("Failed to get host address");
```

**This replaces**: All the raw pointer arithmetic and unsafe memcpy
we'd otherwise need for every guest memory interaction. The bounds
checking catches bugs where we accidentally read/write outside the
guest's allocated region.

---

## 3. Provisioning CPU

### 3.1 Create vCPU

```rust
fn create_vcpu(kvm: &Kvm, vm: &VmFd) -> (VcpuFd, usize) {
    let vcpu = vm.create_vcpu(0).expect("Failed to create vCPU");
    // Internally: ioctl(vm_fd, KVM_CREATE_VCPU, 0)
    // Also mmaps the kvm_run struct automatically

    let mmap_size = kvm.get_vcpu_mmap_size()
        .expect("Failed to get vCPU mmap size");

    (vcpu, mmap_size)
}
```

**What the crate handles**: The vCPU fd, the kvm_run mmap, and later
the `VcpuExit` enum that parses exit reasons into typed variants.

### 3.2 Set CPU registers

```rust
use kvm_bindings::{kvm_regs, kvm_sregs, kvm_segment};

fn setup_long_mode_regs(vcpu: &VcpuFd, kernel_entry: u64, boot_params_addr: u64) {
    // --- Special registers (segments, control regs) ---
    let mut sregs = vcpu.get_sregs().expect("Failed to get sregs");

    // Enable protected mode + paging + long mode
    sregs.cr0 = 0x8005_0033; // PE | MP | ET | NE | WP | AM | PG
    sregs.cr4 = 0x0000_06A0; // PAE | MCE | OSFXSR | OSXMMEXCPT
    sregs.efer = 0x0000_0D01; // SCE | LME | LMA | NXE

    // 64-bit code segment
    sregs.cs = kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: 0x10,
        type_: 11,     // execute/read, accessed
        present: 1,
        dpl: 0,        // ring 0
        db: 0,         // not 32-bit
        s: 1,          // code/data
        l: 1,          // 64-bit mode
        g: 1,          // 4KB granularity
        ..Default::default()
    };

    // Data segment (applied to ds, es, fs, gs, ss)
    let data_seg = kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: 0x18,
        type_: 3,      // read/write, accessed
        present: 1,
        dpl: 0,
        db: 1,
        s: 1,
        l: 0,
        g: 1,
        ..Default::default()
    };
    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.fs = data_seg;
    sregs.gs = data_seg;
    sregs.ss = data_seg;

    vcpu.set_sregs(&sregs).expect("Failed to set sregs");

    // --- General purpose registers ---
    let regs = kvm_regs {
        rip: kernel_entry,
        rsi: boot_params_addr,  // Linux boot protocol: RSI = boot_params ptr
        rflags: 0x2,            // bit 1 always set
        rsp: GUEST_MEM_SIZE as u64, // stack at top of memory
        ..Default::default()
    };

    vcpu.set_regs(&regs).expect("Failed to set regs");
}
```

**What kvm-bindings gives us**: `kvm_regs`, `kvm_sregs`, `kvm_segment`
structs with proper layout, alignment, and Default implementations.
No hand-rolled `#[repr(C)]` structs. The `..Default::default()` pattern
zeroes all the fields we don't care about.

### 3.3 GDT and page tables

We still need to write these into guest memory ourselves. The crates
don't handle this — it's VMM-specific policy:

```rust
fn setup_gdt(guest_memory: &GuestMemoryMmap) {
    // Minimal GDT: null + code + data
    let gdt_addr = GuestAddress(0x1000);
    let gdt_entries: [u64; 4] = [
        0x0000_0000_0000_0000, // null
        0x0000_0000_0000_0000, // unused
        0x00AF_9A00_0000_FFFF, // 64-bit code
        0x00CF_9200_0000_FFFF, // 64-bit data
    ];

    for (i, entry) in gdt_entries.iter().enumerate() {
        guest_memory
            .write_obj(*entry, GuestAddress(gdt_addr.0 + (i as u64 * 8)))
            .expect("Failed to write GDT entry");
    }

    // Page tables for identity-mapping the first N GB
    // PML4 → PDPT → PD with 2MB pages
    setup_page_tables(guest_memory);
}

fn setup_page_tables(guest_memory: &GuestMemoryMmap) {
    let pml4_addr = GuestAddress(0x2000);
    let pdpt_addr = GuestAddress(0x3000);
    let pd_addr   = GuestAddress(0x4000);

    // PML4[0] → PDPT (present + writable)
    guest_memory.write_obj(pdpt_addr.0 | 0x03u64, pml4_addr)
        .expect("Failed to write PML4");

    // PDPT[0] → PD (present + writable)
    guest_memory.write_obj(pd_addr.0 | 0x03u64, pdpt_addr)
        .expect("Failed to write PDPT");

    // PD entries: 512 x 2MB pages = 1GB identity mapped
    for i in 0..512u64 {
        let entry = (i * (2 << 20)) | 0x83; // present + writable + PS (2MB page)
        guest_memory
            .write_obj(entry, GuestAddress(pd_addr.0 + i * 8))
            .expect("Failed to write PD entry");
    }
}
```

---

## 4. Loading the Kernel (linux-loader)

This is where the crate saves the most manual work. The bzImage format
parsing, setup header patching, and boot params configuration is all
handled for us.

### 4.1 Load bzImage into guest memory

```rust
use linux_loader::loader::bzimage::BzImage;
use linux_loader::loader::KernelLoader;
use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams};
use vm_memory::GuestAddress;
use std::fs::File;

const HIMEM_START: u64 = 0x0010_0000;  // 1MB — kernel load address
const CMDLINE_ADDR: u64 = 0x0002_0000; // 128KB — command line
const BOOT_PARAMS_ADDR: u64 = 0x0000_7000; // "zero page"

fn load_kernel(guest_memory: &GuestMemoryMmap) -> u64 {
    let mut kernel_file = File::open("/path/to/vmlinuz")
        .expect("Failed to open kernel");

    // BzImage::load handles:
    //   - Parsing the setup header at offset 0x1F1
    //   - Splitting setup from compressed kernel
    //   - Copying compressed kernel to HIMEM_START in guest memory
    //   - Returning the setup header for boot params configuration
    let kernel_load = BzImage::load(
        guest_memory,
        None,                              // no kernel offset override
        &mut kernel_file,
        Some(GuestAddress(HIMEM_START)),    // high memory start
    ).expect("Failed to load bzImage");

    // kernel_load.kernel_end — where the kernel ends in guest memory
    // kernel_load.setup_header — the parsed setup header (Option)

    kernel_load.kernel_end.raw_value()
}
```

### 4.2 Load initrd

```rust
fn load_initrd(
    guest_memory: &GuestMemoryMmap,
    kernel_end: u64,
) -> (u64, u64) {
    let initrd_data = std::fs::read("/path/to/initrd.img")
        .expect("Failed to read initrd");

    // Place initrd after kernel, page-aligned
    let initrd_addr = (kernel_end + 0xFFF) & !0xFFF;

    guest_memory
        .write_slice(&initrd_data, GuestAddress(initrd_addr))
        .expect("Failed to write initrd to guest memory");

    (initrd_addr, initrd_data.len() as u64)
}
```

### 4.3 Build and load command line

```rust
use linux_loader::cmdline::Cmdline;

fn load_cmdline(guest_memory: &GuestMemoryMmap) -> Cmdline {
    let mut cmdline = Cmdline::new(256)
        .expect("Failed to create cmdline");

    cmdline.insert_str("console=ttyS0 root=/dev/vda rw quiet")
        .expect("Failed to set base cmdline");

    // linux-loader has built-in helpers for virtio-mmio device strings
    cmdline.add_virtio_mmio_device(
        4096,           // size
        GuestAddress(0xD000_0000),  // base address
        5,              // IRQ
        None,           // no device ID override
    ).expect("Failed to add virtio block device to cmdline");

    cmdline.add_virtio_mmio_device(
        4096,
        GuestAddress(0xD000_1000),
        6,
        None,
    ).expect("Failed to add virtio net device to cmdline");

    // Write cmdline to guest memory
    linux_loader::loader::load_cmdline(
        guest_memory,
        GuestAddress(CMDLINE_ADDR),
        &cmdline,
    ).expect("Failed to load cmdline into guest memory");

    cmdline
}
```

### 4.4 Configure boot parameters (zero page)

```rust
use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams};
use kvm_bindings::kvm_segment; // we reuse this for the boot_params struct

fn configure_boot_params(
    guest_memory: &GuestMemoryMmap,
    cmdline: &Cmdline,
    initrd_addr: u64,
    initrd_size: u64,
) {
    // linux-loader exports the boot_params struct from the Linux kernel headers
    // We create one, fill in our values, and let the configurator write it
    let mut params = linux_loader::bootparam::boot_params::default();

    // Setup header fields
    params.hdr.type_of_loader = 0xFF;
    params.hdr.loadflags |= 0x01;  // LOADED_HIGH
    params.hdr.cmd_line_ptr = CMDLINE_ADDR as u32;
    params.hdr.cmdline_size = cmdline.as_str().len() as u32 + 1;
    params.hdr.ramdisk_image = initrd_addr as u32;
    params.hdr.ramdisk_size = initrd_size as u32;

    // E820 memory map — tell Linux what memory it has
    add_e820_entry(&mut params, 0, 0x0009_FC00, E820_RAM);        // base memory
    add_e820_entry(&mut params, 0x0010_0000,
        GUEST_MEM_SIZE as u64 - 0x0010_0000, E820_RAM);           // high memory

    // Write boot params to guest memory
    LinuxBootConfigurator::write_bootparams(
        BootParams::new(&params, GuestAddress(BOOT_PARAMS_ADDR)),
        guest_memory,
    ).expect("Failed to write boot params");
}

const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;

fn add_e820_entry(
    params: &mut linux_loader::bootparam::boot_params,
    addr: u64,
    size: u64,
    type_: u32,
) {
    let i = params.e820_entries as usize;
    params.e820_table[i].addr = addr;
    params.e820_table[i].size = size;
    params.e820_table[i].type_ = type_;
    params.e820_entries += 1;
}
```

**What linux-loader eliminates**: All the manual bzImage header
parsing, the memcpy offset calculations, the command line string
management, and the boot_params struct definition. This is easily
150+ lines of tedious, error-prone code that the crate handles
correctly and that's shared with Firecracker and Cloud Hypervisor.

---

## 5. The Run Loop (hand-rolled)

This is ours. The crate gives us the `VcpuExit` enum which is much
cleaner than parsing raw exit reason integers, but all the handling
logic is our code.

```rust
use kvm_ioctls::VcpuExit;
use kvm_bindings::kvm_irq_level;

fn run_vm(
    vcpu: &VcpuFd,
    vm: &VmFd,
    guest_memory: &GuestMemoryMmap,
    block_device: &mut BlockDevice,  // our type, see §6
    net_device: &mut NetDevice,      // our type, see §7
    serial: &mut Serial,             // our type, see §8
) {
    loop {
        match vcpu.run().expect("KVM_RUN failed") {

            VcpuExit::IoOut(port, data) => {
                serial.handle_io_out(port, data);
            }

            VcpuExit::IoIn(port, data) => {
                serial.handle_io_in(port, data);
            }

            VcpuExit::MmioWrite(addr, data) => {
                match addr {
                    0xD000_0000..=0xD000_0FFF => {
                        let offset = addr - 0xD000_0000;
                        if let Some(irq) = block_device.handle_mmio_write(
                            offset, data, guest_memory,
                        ) {
                            inject_irq(vm, irq);
                        }
                    }
                    0xD000_1000..=0xD000_1FFF => {
                        let offset = addr - 0xD000_1000;
                        if let Some(irq) = net_device.handle_mmio_write(
                            offset, data, guest_memory,
                        ) {
                            inject_irq(vm, irq);
                        }
                    }
                    _ => {} // unmapped MMIO — ignore
                }
            }

            VcpuExit::MmioRead(addr, data) => {
                match addr {
                    0xD000_0000..=0xD000_0FFF => {
                        let offset = addr - 0xD000_0000;
                        block_device.handle_mmio_read(offset, data);
                    }
                    0xD000_1000..=0xD000_1FFF => {
                        let offset = addr - 0xD000_1000;
                        net_device.handle_mmio_read(offset, data);
                    }
                    _ => {
                        // Return zeros for unmapped reads
                        data.fill(0);
                    }
                }
            }

            VcpuExit::Hlt => {
                // Guest is idle. In a real implementation:
                // poll for TAP fd readability or stdin, then re-enter.
                // For now, just re-enter immediately.
            }

            VcpuExit::Shutdown => {
                println!("Guest requested shutdown");
                break;
            }

            VcpuExit::InternalError => {
                eprintln!("KVM internal error");
                break;
            }

            exit => {
                eprintln!("Unhandled exit: {:?}", exit);
            }
        }
    }
}

fn inject_irq(vm: &VmFd, irq: u32) {
    let assert = kvm_irq_level {
        irq,
        level: 1, // ..Default::default() for union padding
    };
    let deassert = kvm_irq_level {
        irq,
        level: 0,
    };
    vm.set_irq_line(&assert).expect("Failed to assert IRQ");
    vm.set_irq_line(&deassert).expect("Failed to deassert IRQ");
}
```

**What VcpuExit gives us**: Instead of reading a raw `u32` exit_reason
and then unsafely casting the union in `kvm_run`, we get a Rust enum
with typed payloads. `IoOut(port, data)` gives us the port and a
reference to the data bytes directly. `MmioWrite(addr, data)` gives
us the guest physical address and data. No pointer arithmetic.

---

## 6. Virtio Block Device (hand-rolled)

We define our own types. No `virtio-queue` or `virtio-devices` crate.
We use `vm-memory` for guest memory access but everything else is ours.

```rust
use vm_memory::{GuestMemoryMmap, GuestAddress, Bytes};

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;    // "virt"
const VIRTIO_MMIO_VERSION: u32 = 2;
const VIRTIO_DEV_BLOCK: u32 = 2;
const VIRTIO_DEV_NET: u32 = 1;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 0x01;
const VIRTIO_STATUS_DRIVER: u32 = 0x02;
const VIRTIO_STATUS_FEATURES_OK: u32 = 0x08;
const VIRTIO_STATUS_DRIVER_OK: u32 = 0x04;
const VIRTIO_STATUS_FAILED: u32 = 0x80;

const VIRTIO_BLK_T_IN: u32 = 0;   // read
const VIRTIO_BLK_T_OUT: u32 = 1;  // write
const VIRTIO_BLK_T_FLUSH: u32 = 4;

const QUEUE_SIZE: u16 = 256;

/// Virtqueue descriptor — matches the virtio spec layout exactly
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

const VIRTQ_DESC_F_NEXT: u16 = 0x1;
const VIRTQ_DESC_F_WRITE: u16 = 0x2;

/// Block request header
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VirtioBlkReq {
    type_: u32,
    reserved: u32,
    sector: u64,
}

struct BlockDevice {
    // MMIO register state
    status: u32,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    queue_sel: u32,
    queue_num: u16,
    queue_ready: bool,
    interrupt_status: u32,

    // Virtqueue addresses (set by guest)
    desc_addr: u64,
    avail_addr: u64,
    used_addr: u64,

    // Host tracking
    last_avail_idx: u16,

    // Backing store
    disk: Vec<u8>, // simplest: in-memory disk image
    disk_sectors: u64,

    irq: u32,
}

impl BlockDevice {
    fn new(disk_data: Vec<u8>, irq: u32) -> Self {
        let disk_sectors = disk_data.len() as u64 / 512;
        Self {
            status: 0,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            queue_sel: 0,
            queue_num: 0,
            queue_ready: false,
            interrupt_status: 0,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
            last_avail_idx: 0,
            disk: disk_data,
            disk_sectors,
            irq,
        }
    }

    fn handle_mmio_read(&self, offset: u64, data: &mut [u8]) {
        let val: u32 = match offset {
            0x000 => VIRTIO_MMIO_MAGIC,
            0x004 => VIRTIO_MMIO_VERSION,
            0x008 => VIRTIO_DEV_BLOCK,
            0x00C => 0x554D_4551, // vendor ID
            0x010 => {
                // DeviceFeatures — depends on selector
                match self.device_features_sel {
                    0 => 0, // low 32 bits: no special block features
                    1 => (VIRTIO_F_VERSION_1 >> 32) as u32,
                    _ => 0,
                }
            }
            0x034 => QUEUE_SIZE as u32,         // QueueNumMax
            0x044 => self.queue_ready as u32,   // QueueReady
            0x060 => self.interrupt_status,      // InterruptStatus
            0x070 => self.status,                // Status

            // Config space: capacity (u64 at offset 0x100)
            0x100 => self.disk_sectors as u32,
            0x104 => (self.disk_sectors >> 32) as u32,

            _ => 0,
        };

        let bytes = val.to_le_bytes();
        let len = data.len().min(4);
        data[..len].copy_from_slice(&bytes[..len]);
    }

    fn handle_mmio_write(
        &mut self,
        offset: u64,
        data: &[u8],
        guest_memory: &GuestMemoryMmap,
    ) -> Option<u32> {
        let val = u32::from_le_bytes(data[..4].try_into().unwrap_or([0; 4]));

        match offset {
            0x014 => self.device_features_sel = val,
            0x020 => {
                let shift = self.driver_features_sel as u64 * 32;
                self.driver_features |= (val as u64) << shift;
            }
            0x024 => self.driver_features_sel = val,
            0x030 => self.queue_sel = val,
            0x038 => self.queue_num = val as u16,
            0x044 => self.queue_ready = val == 1,

            0x050 => {
                // QueueNotify — guest says there are new buffers
                return self.process_queue(guest_memory);
            }

            0x064 => {
                // InterruptACK
                self.interrupt_status &= !val;
            }
            0x070 => self.status = val,

            // Queue address registers
            0x080 => self.desc_addr = (self.desc_addr & !0xFFFF_FFFF) | val as u64,
            0x084 => self.desc_addr = (self.desc_addr & 0xFFFF_FFFF) | ((val as u64) << 32),
            0x090 => self.avail_addr = (self.avail_addr & !0xFFFF_FFFF) | val as u64,
            0x094 => self.avail_addr = (self.avail_addr & 0xFFFF_FFFF) | ((val as u64) << 32),
            0x0A0 => self.used_addr = (self.used_addr & !0xFFFF_FFFF) | val as u64,
            0x0A4 => self.used_addr = (self.used_addr & 0xFFFF_FFFF) | ((val as u64) << 32),

            _ => {}
        }
        None
    }

    fn process_queue(
        &mut self,
        guest_memory: &GuestMemoryMmap,
    ) -> Option<u32> {
        // Read current avail index from guest memory
        let avail_idx: u16 = guest_memory
            .read_obj(GuestAddress(self.avail_addr + 2))
            .expect("Failed to read avail idx");

        let mut did_work = false;

        while self.last_avail_idx != avail_idx {
            let ring_idx = (self.last_avail_idx % QUEUE_SIZE) as u64;
            let head: u16 = guest_memory
                .read_obj(GuestAddress(self.avail_addr + 4 + ring_idx * 2))
                .expect("Failed to read avail ring entry");

            self.process_descriptor_chain(guest_memory, head);
            self.push_used(guest_memory, head as u32, 0);

            self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
            did_work = true;
        }

        if did_work {
            self.interrupt_status |= 0x01;
            Some(self.irq)
        } else {
            None
        }
    }

    fn process_descriptor_chain(
        &mut self,
        guest_memory: &GuestMemoryMmap,
        head: u16,
    ) {
        // Read descriptor chain: header → data → status
        let desc0 = self.read_desc(guest_memory, head);
        let req: VirtioBlkReq = guest_memory
            .read_obj(GuestAddress(desc0.addr))
            .expect("Failed to read block request");

        let desc1 = self.read_desc(guest_memory, desc0.next);
        let data_addr = GuestAddress(desc1.addr);
        let data_len = desc1.len as usize;

        let desc2 = self.read_desc(guest_memory, desc1.next);
        let status_addr = GuestAddress(desc2.addr);

        let disk_offset = req.sector as usize * 512;
        let status: u8 = match req.type_ {
            VIRTIO_BLK_T_IN => {
                // Read from disk → guest memory
                if disk_offset + data_len <= self.disk.len() {
                    guest_memory
                        .write_slice(&self.disk[disk_offset..disk_offset + data_len], data_addr)
                        .expect("Failed to write to guest");
                    0 // OK
                } else {
                    1 // IOERR
                }
            }
            VIRTIO_BLK_T_OUT => {
                // Read from guest memory → disk
                let mut buf = vec![0u8; data_len];
                guest_memory
                    .read_slice(&mut buf, data_addr)
                    .expect("Failed to read from guest");
                if disk_offset + data_len <= self.disk.len() {
                    self.disk[disk_offset..disk_offset + data_len].copy_from_slice(&buf);
                    0
                } else {
                    1
                }
            }
            VIRTIO_BLK_T_FLUSH => 0, // no-op for in-memory
            _ => 2, // UNSUPP
        };

        guest_memory
            .write_obj(status, status_addr)
            .expect("Failed to write status");
    }

    fn read_desc(&self, guest_memory: &GuestMemoryMmap, idx: u16) -> VirtqDesc {
        let desc_size = std::mem::size_of::<VirtqDesc>() as u64;
        guest_memory
            .read_obj(GuestAddress(self.desc_addr + idx as u64 * desc_size))
            .expect("Failed to read descriptor")
    }

    fn push_used(&self, guest_memory: &GuestMemoryMmap, id: u32, len: u32) {
        let used_idx: u16 = guest_memory
            .read_obj(GuestAddress(self.used_addr + 2))
            .expect("Failed to read used idx");

        let ring_offset = self.used_addr + 4 + (used_idx % QUEUE_SIZE) as u64 * 8;
        guest_memory.write_obj(id, GuestAddress(ring_offset)).unwrap();
        guest_memory.write_obj(len, GuestAddress(ring_offset + 4)).unwrap();

        guest_memory
            .write_obj(used_idx.wrapping_add(1), GuestAddress(self.used_addr + 2))
            .unwrap();
    }
}
```

Note how `vm-memory` makes all the guest memory access safe and
bounds-checked. Every `read_obj` and `write_slice` validates that
the address is within the guest's allocated memory. No raw pointer
arithmetic anywhere.

---

## 7. Virtual NIC (hand-rolled)

Same virtio MMIO transport pattern as block, different device ID
and two queues instead of one. TAP setup uses `nix` for the ioctl.

```rust
use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;

const TUNSETIFF: libc::c_ulong = 0x400454CA;
const IFF_TAP: libc::c_short = 0x0002;
const IFF_NO_PI: libc::c_short = 0x1000;

#[repr(C)]
struct ifreq {
    ifr_name: [u8; 16],
    ifr_flags: libc::c_short,
    _pad: [u8; 22],
}

fn create_tap(name: &str) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;

    let fd = open("/dev/net/tun", OFlag::O_RDWR, Mode::empty())
        .expect("Failed to open /dev/net/tun");

    let mut ifr = ifreq {
        ifr_name: [0u8; 16],
        ifr_flags: IFF_TAP | IFF_NO_PI,
        _pad: [0u8; 22],
    };

    let name_bytes = name.as_bytes();
    ifr.ifr_name[..name_bytes.len()].copy_from_slice(name_bytes);

    unsafe {
        let ret = libc::ioctl(fd, TUNSETIFF, &ifr as *const ifreq);
        assert!(ret >= 0, "TUNSETIFF failed: {}", std::io::Error::last_os_error());
    }

    // Bring up interface and set IP via netlink or shell out
    // (netlink is cleaner but adds ~50 lines; shell is 2 lines)
    std::process::Command::new("ip")
        .args(["link", "set", name, "up"])
        .status()
        .expect("Failed to bring up TAP");

    std::process::Command::new("ip")
        .args(["addr", "add", "10.0.0.1/24", "dev", name])
        .status()
        .expect("Failed to set TAP address");

    unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }
}
```

The virtio-net device struct follows the same pattern as BlockDevice
but with `device_id = 1`, two queues (RX=0, TX=1), and the process
loop reading/writing raw Ethernet frames from the TAP fd.

```rust
/// Virtio-net header prepended to every packet
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VirtioNetHdr {
    flags: u8,
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    // num_buffers: u16, // only with VIRTIO_NET_F_MRG_RXBUF
}

struct NetDevice {
    // Same MMIO state fields as BlockDevice...
    // Plus:
    mac: [u8; 6],
    tap_fd: std::os::fd::RawFd,
    irq: u32,

    // Two queues
    rx_desc_addr: u64,
    rx_avail_addr: u64,
    rx_used_addr: u64,
    rx_last_avail_idx: u16,

    tx_desc_addr: u64,
    tx_avail_addr: u64,
    tx_used_addr: u64,
    tx_last_avail_idx: u16,

    // ... same MMIO register state
    status: u32,
    device_features_sel: u32,
    queue_sel: u32,
    // etc.
}
```

TX processing: read descriptor chain from guest memory, strip the
10-byte virtio-net header, write the raw Ethernet frame to TAP fd.

RX processing: read from TAP fd, prepend zeroed virtio-net header,
write into a pre-posted guest buffer from the RX queue, inject IRQ.

---

## 8. Serial Console (hand-rolled)

Minimal 16550 UART. No crate, just port matching in the I/O exit handler.

```rust
struct Serial {
    // Minimal register state
    ier: u8,    // interrupt enable register
    lcr: u8,    // line control register
    mcr: u8,    // modem control register
    scr: u8,    // scratch register
}

impl Serial {
    fn new() -> Self {
        Self { ier: 0, lcr: 0x03, mcr: 0, scr: 0 }
    }

    fn handle_io_out(&mut self, port: u16, data: &[u8]) {
        match port {
            0x3F8 => {
                // THR — transmit: write byte to stdout
                use std::io::Write;
                std::io::stdout().write_all(&data[..1]).ok();
                std::io::stdout().flush().ok();
            }
            0x3F9 => self.ier = data[0],
            0x3FB => self.lcr = data[0],
            0x3FC => self.mcr = data[0],
            0x3FF => self.scr = data[0],
            _ => {} // ignore writes to other registers
        }
    }

    fn handle_io_in(&mut self, port: u16, data: &mut [u8]) {
        data[0] = match port {
            0x3F8 => {
                // RBR — receive: read byte from stdin (non-blocking)
                let mut buf = [0u8; 1];
                match nix::unistd::read(libc::STDIN_FILENO, &mut buf) {
                    Ok(1) => buf[0],
                    _ => 0,
                }
            }
            0x3F9 => self.ier,
            0x3FA => 0x01, // IIR: no pending interrupt
            0x3FB => self.lcr,
            0x3FC => self.mcr,
            0x3FD => {
                // LSR: transmitter empty + check if stdin has data
                let mut lsr = 0x60u8; // bits 5+6: TX empty
                // Non-blocking check for stdin data
                let mut pollfd = libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut pollfd, 1, 0) };
                if ready > 0 {
                    lsr |= 0x01; // data ready
                }
                lsr
            }
            0x3FE => 0x00, // MSR
            0x3FF => self.scr,
            _ => 0,
        };
    }
}
```

---

## 9. Putting It Together

```rust
fn main() {
    // Terminal raw mode for serial console
    let original_termios = setup_raw_terminal();

    // KVM
    let (kvm, vm) = create_vm();

    // RAM
    let guest_memory = setup_guest_memory(&vm);

    // Page tables and GDT
    setup_gdt(&guest_memory);

    // Kernel
    let kernel_end = load_kernel(&guest_memory);
    let (initrd_addr, initrd_size) = load_initrd(&guest_memory, kernel_end);
    let cmdline = load_cmdline(&guest_memory);
    configure_boot_params(&guest_memory, &cmdline, initrd_addr, initrd_size);

    // vCPU
    let (vcpu, _) = create_vcpu(&kvm, &vm);
    setup_long_mode_regs(&vcpu, HIMEM_START, BOOT_PARAMS_ADDR);

    // Devices
    let disk_data = std::fs::read("/path/to/rootfs.img")
        .expect("Failed to read disk image");
    let mut block_device = BlockDevice::new(disk_data, 5);

    let tap = create_tap("vmtap0");
    let mut net_device = NetDevice::new(tap, 6);

    let mut serial = Serial::new();

    // Run
    run_vm(&vcpu, &vm, &guest_memory, &mut block_device, &mut net_device, &mut serial);

    // Cleanup
    restore_terminal(original_termios);
}
```

---

## 10. Boundary Summary

### We use crates for (definitions, parsing, safety wrappers):

| Task                         | Crate            | Why                                |
|------------------------------|------------------|------------------------------------|
| KVM fd management            | `kvm-ioctls`     | No raw ioctl numbers, typed exits  |
| KVM struct definitions       | `kvm-bindings`   | No hand-rolled `#[repr(C)]` for KVM|
| Guest memory access          | `vm-memory`      | Bounds checking, type-safe r/w     |
| Kernel image loading         | `linux-loader`   | bzImage parsing, boot params, cmdline |
| Host syscalls                | `nix` / `libc`   | mmap, poll, ioctl for TAP          |

### We write ourselves (all logic, all devices, all policy):

| Component                    | Lines (est.) | Why hand-roll                      |
|------------------------------|--------------|------------------------------------|
| Virtio MMIO transport        | 120-150      | Full control over device surface   |
| Virtio block device          | 150-180      | Backing store is our abstraction   |
| Virtio net device            | 150-180      | TAP integration is trivial         |
| Serial console               | 60-80        | Too simple to justify a crate      |
| Run loop + exit dispatch     | 40-60        | Core logic, must be ours           |
| TAP device creation          | 30-40        | One ioctl + two shell commands     |
| GDT + page tables            | 40-50        | Memory layout is policy            |
| CoW golden image setup       | 20-30        | Our key differentiator             |
| **Total hand-rolled**        | **610-770**  |                                    |

### Final dependency tree

```
microvm
├── kvm-bindings    (structs only, no logic)
├── kvm-ioctls      (thin ioctl wrappers)
│   └── kvm-bindings
├── vm-memory       (guest memory abstraction)
├── linux-loader    (kernel loading)
│   └── vm-memory
├── nix             (libc wrappers)
└── libc            (constants)
```

Six crates. No frameworks. No async runtime. No tokio.
No virtio crate. No event manager. No VMM.

The crates handle the parts that are standardized and tedious
(KVM ABI, Linux boot protocol). We handle the parts that
define our system's character (device model, backing store,
networking, lifecycle).
