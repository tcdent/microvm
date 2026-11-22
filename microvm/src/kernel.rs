use linux_loader::bootparam::boot_params;
use linux_loader::cmdline::Cmdline;
use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams};
use linux_loader::loader::elf::{Elf, PvhBootCapability};
use linux_loader::loader::{KernelLoader, KernelLoaderResult};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

const HIMEM_START: u64 = 0x0010_0000;       // 1MB — where the kernel is loaded
const CMDLINE_ADDR: u64 = 0x0002_0000;      // 128KB — kernel command line
pub const BOOT_PARAMS_ADDR: u64 = 0x0000_7000; // "zero page" — boot parameters

const E820_RAM: u32 = 1;

/// Load a bzImage kernel + initrd into guest memory and configure boot parameters.
/// Returns (kernel_entry, boot_params_addr) for setting CPU registers.
pub fn load(
    memory: &GuestMemoryMmap,
    kernel_path: &str,
    initrd_path: &str,
    mem_size: usize,
    extra_cmdline: &str,
) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let kernel_result = load_kernel(memory, kernel_path)?;
    let (initrd_addr, initrd_size) = if initrd_path.is_empty() {
        (0, 0)
    } else {
        load_initrd(memory, kernel_result.kernel_end, initrd_path)?
    };
    let cmdline = load_cmdline(memory, extra_cmdline)?;
    configure_boot_params(memory, &kernel_result, &cmdline, initrd_addr, initrd_size, mem_size)?;

    let entry = kernel_result.kernel_load.raw_value();
    log!("Kernel loaded: entry=0x{:x} boot_params=0x{:x}", entry, BOOT_PARAMS_ADDR);
    log!("Kernel end: 0x{:x}, initrd: 0x{:x} ({}KB)", kernel_result.kernel_end, initrd_addr, initrd_size / 1024);

    Ok((entry, BOOT_PARAMS_ADDR))
}

fn load_kernel(memory: &GuestMemoryMmap, path: &str) -> Result<KernelLoaderResult, Box<dyn std::error::Error>> {
    let mut kernel_file = std::fs::File::open(path)?;

    let result = Elf::load(
        memory,
        None,
        &mut kernel_file,
        None,
    )?;

    Ok(result)
}

fn load_initrd(
    memory: &GuestMemoryMmap,
    kernel_end: u64,
    path: &str,
) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let initrd_data = std::fs::read(path)?;

    // Place initrd after kernel, page-aligned
    let initrd_addr = (kernel_end + 0xFFF) & !0xFFF;
    memory.write_slice(&initrd_data, GuestAddress(initrd_addr))?;

    Ok((initrd_addr, initrd_data.len() as u64))
}

/// Base command line plus the launcher's device and root arguments.
fn load_cmdline(memory: &GuestMemoryMmap, extra: &str) -> Result<Cmdline, Box<dyn std::error::Error>> {
    let mut cmdline = Cmdline::new(1024)?;
    cmdline.insert_str("console=ttyS0 earlyprintk=ttyS0 random.trust_cpu=on pci=off")?;
    // Register virtio-mmio devices: size@base:irq
    cmdline.insert_str(" virtio_mmio.device=4K@0xd0000000:5")?; // virtio-fs
    cmdline.insert_str(" virtio_mmio.device=4K@0xd0001000:6")?; // virtio-net
    cmdline.insert_str(" virtio_mmio.device=4K@0xd0002000:7")?; // virtio-console
    cmdline.insert_str(extra)?;

    linux_loader::loader::load_cmdline(memory, GuestAddress(CMDLINE_ADDR), &cmdline)?;

    Ok(cmdline)
}

fn configure_boot_params(
    memory: &GuestMemoryMmap,
    kernel_result: &KernelLoaderResult,
    cmdline: &Cmdline,
    initrd_addr: u64,
    initrd_size: u64,
    mem_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut params = boot_params::default();

    // Copy the setup header from the kernel image — contains critical boot protocol fields
    if let Some(hdr) = kernel_result.setup_header {
        params.hdr = hdr;
    }

    if params.hdr.type_of_loader == 0 {
        params.hdr.type_of_loader = 0xFF;
    }
    params.hdr.loadflags |= 0x01; // LOADED_HIGH
    params.hdr.cmd_line_ptr = CMDLINE_ADDR as u32;
    params.hdr.cmdline_size = cmdline.as_cstring()?.as_bytes().len() as u32;
    params.hdr.ramdisk_image = initrd_addr as u32;
    params.hdr.ramdisk_size = initrd_size as u32;

    // E820 memory map — tell Linux what memory it has. Same split as
    // memory::ram_layout: low RAM up to the MMIO hole, the rest above 4 GiB.
    add_e820_entry(&mut params, 0, 0x0009_FC00, E820_RAM);
    for (start, len) in crate::memory::ram_layout(mem_size as u64) {
        if start == 0 {
            add_e820_entry(&mut params, 0x0010_0000, len - 0x0010_0000, E820_RAM);
        } else {
            add_e820_entry(&mut params, start, len, E820_RAM);
        }
    }

    LinuxBootConfigurator::write_bootparams::<GuestMemoryMmap>(
        &BootParams::new::<boot_params>(&params, GuestAddress(BOOT_PARAMS_ADDR)),
        memory,
    )?;

    Ok(())
}

fn add_e820_entry(params: &mut boot_params, addr: u64, size: u64, type_: u32) {
    let i = params.e820_entries as usize;
    params.e820_table[i].addr = addr;
    params.e820_table[i].size = size;
    params.e820_table[i].type_ = type_;
    params.e820_entries += 1;
}
