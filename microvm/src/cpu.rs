use kvm_bindings::{kvm_dtable, kvm_fpu, kvm_msr_entry, kvm_regs, kvm_segment, Msrs};
use kvm_ioctls::{VcpuFd, VmFd};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

// Memory layout for CPU structures
const GDT_ADDR: u64 = 0x500;
const IDT_ADDR: u64 = 0x520;
const PML4_ADDR: u64 = 0x9000;
const PDPT_ADDR: u64 = 0xa000;
const PD_ADDR: u64 = 0xb000;   // PD0: 0-1GB
const PD1_ADDR: u64 = 0xc000;  // PD1: 1-2GB
const PD2_ADDR: u64 = 0xd000;  // PD2: 2-3GB
const PD3_ADDR: u64 = 0xe000;  // PD3: 3-4GB (covers LAPIC at 0xFEE00000)

const BOOT_STACK_POINTER: u64 = 0x8ff0;

pub fn create_vcpu(
    kvm: &kvm_ioctls::Kvm,
    vm: &VmFd,
    memory: &GuestMemoryMmap,
) -> Result<VcpuFd, Box<dyn std::error::Error>> {
    let vcpu = vm.create_vcpu(0)?;

    // Pass through host CPUID so the kernel sees real CPU features (RDRAND, SSE, etc)
    let mut cpuid = kvm.get_supported_cpuid(256)?;
    vcpu.set_cpuid2(&cpuid)?;

    setup_gdt(memory)?;
    setup_page_tables(memory)?;
    setup_sregs(&vcpu)?;
    setup_fpu(&vcpu)?;
    setup_msrs(&vcpu)?;

    Ok(vcpu)
}

pub fn setup_regs(vcpu: &VcpuFd, kernel_entry: u64, boot_params_addr: u64) -> Result<(), Box<dyn std::error::Error>> {
    let regs = kvm_regs {
        rip: kernel_entry,
        rsi: boot_params_addr,
        rflags: 0x2,
        rsp: BOOT_STACK_POINTER,
        rbp: BOOT_STACK_POINTER,
        ..Default::default()
    };
    vcpu.set_regs(&regs)?;
    Ok(())
}

fn setup_fpu(vcpu: &VcpuFd) -> Result<(), Box<dyn std::error::Error>> {
    let fpu = kvm_fpu {
        fcw: 0x37f,      // FPU control word: exceptions masked, round to nearest
        mxcsr: 0x1f80,   // SSE control: exceptions masked, default rounding
        ..Default::default()
    };
    vcpu.set_fpu(&fpu)?;
    Ok(())
}

fn setup_msrs(vcpu: &VcpuFd) -> Result<(), Box<dyn std::error::Error>> {
    let entries = [
        kvm_msr_entry { index: 0x174, data: 0, ..Default::default() }, // SYSENTER_CS
        kvm_msr_entry { index: 0x175, data: 0, ..Default::default() }, // SYSENTER_ESP
        kvm_msr_entry { index: 0x176, data: 0, ..Default::default() }, // SYSENTER_EIP
        kvm_msr_entry { index: 0xc000_0081, data: 0, ..Default::default() }, // STAR
        kvm_msr_entry { index: 0xc000_0082, data: 0, ..Default::default() }, // LSTAR
        kvm_msr_entry { index: 0xc000_0083, data: 0, ..Default::default() }, // CSTAR
        kvm_msr_entry { index: 0xc000_0084, data: 0, ..Default::default() }, // SYSCALL_MASK
        kvm_msr_entry { index: 0xc000_0102, data: 0, ..Default::default() }, // KERNEL_GS_BASE
        kvm_msr_entry { index: 0x10, data: 0, ..Default::default() },        // TSC
        kvm_msr_entry { index: 0x1a0, data: 1, ..Default::default() },       // MISC_ENABLE (fast string)
    ];

    let mut msrs = Msrs::new(entries.len())?;
    msrs.as_mut_slice().copy_from_slice(&entries);
    vcpu.set_msrs(&msrs)?;
    Ok(())
}

fn setup_sregs(vcpu: &VcpuFd) -> Result<(), Box<dyn std::error::Error>> {
    let mut sregs = vcpu.get_sregs()?;

    // 64-bit long mode with paging
    sregs.cr0 = 0x8005_0033; // PE | MP | ET | NE | WP | AM | PG
    sregs.cr3 = PML4_ADDR;
    sregs.cr4 = 0x0000_06A0;  // PAE | MCE | OSFXSR | OSXMMEXCPT
    sregs.efer = 0x0000_0D01; // SCE | LME | LMA | NXE

    // GDTR and IDTR
    sregs.gdt = kvm_dtable {
        base: GDT_ADDR,
        limit: 31,
        ..Default::default()
    };
    sregs.idt = kvm_dtable {
        base: IDT_ADDR,
        limit: 7,
        ..Default::default()
    };

    // 64-bit code segment (selector 0x08 = GDT entry 1)
    sregs.cs = kvm_segment {
        base: 0,
        limit: 0x000F_FFFF,
        selector: 0x08,
        type_: 11,     // execute/read, accessed
        present: 1,
        dpl: 0,
        db: 0,         // not 32-bit
        s: 1,
        l: 1,          // 64-bit mode
        g: 1,
        ..Default::default()
    };

    // Data segment (selector 0x10 = GDT entry 2)
    let data_seg = kvm_segment {
        base: 0,
        limit: 0x000F_FFFF,
        selector: 0x10,
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

    vcpu.set_sregs(&sregs)?;
    Ok(())
}

fn setup_gdt(memory: &GuestMemoryMmap) -> Result<(), Box<dyn std::error::Error>> {
    // Matches vmm-reference: null, 64-bit code (0xa09b), 64-bit data (0xc093), TSS (0x808b)
    let gdt_entries: [u64; 4] = [
        0x0000_0000_0000_0000, // 0x00: null
        0x00A0_9B00_0000_FFFF, // 0x08: 64-bit code
        0x00C0_9300_0000_FFFF, // 0x10: 64-bit data
        0x0080_8B00_0000_FFFF, // 0x18: TSS
    ];

    for (i, entry) in gdt_entries.iter().enumerate() {
        memory.write_obj(*entry, GuestAddress(GDT_ADDR + (i as u64 * 8)))?;
    }

    memory.write_obj(0u64, GuestAddress(IDT_ADDR))?;

    Ok(())
}

// Identity-map the first 4GB using 2MB pages: PML4 -> PDPT -> 4x PD
// 4GB is needed to cover the LAPIC MMIO region at 0xFEE00000
fn setup_page_tables(memory: &GuestMemoryMmap) -> Result<(), Box<dyn std::error::Error>> {
    // PML4[0] -> PDPT
    memory.write_obj(PDPT_ADDR | 0x03u64, GuestAddress(PML4_ADDR))?;

    // PDPT entries: 4 x 1GB regions
    let pd_addrs = [PD_ADDR, PD1_ADDR, PD2_ADDR, PD3_ADDR];
    for (i, &pd_addr) in pd_addrs.iter().enumerate() {
        memory.write_obj(pd_addr | 0x03u64, GuestAddress(PDPT_ADDR + i as u64 * 8))?;

        // 512 PD entries x 2MB = 1GB per PD
        for j in 0..512u64 {
            let phys = (i as u64 * 512 + j) << 21;
            memory.write_obj(phys | 0x83u64, GuestAddress(pd_addr + j * 8))?;
        }
    }

    Ok(())
}
