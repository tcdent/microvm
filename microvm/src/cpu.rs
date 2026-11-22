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

/// Create vCPU `id`. The bootstrap processor (id 0) is set up to enter the
/// kernel in long mode. Application processors are left in KVM's
/// wait-for-SIPI state; the guest kernel starts them through the in-kernel
/// local APIC and KVM resets their registers on INIT, so only CPUID and
/// the FPU matter here.
pub fn create_vcpu(
    kvm: &kvm_ioctls::Kvm,
    vm: &VmFd,
    memory: &GuestMemoryMmap,
    id: u8,
    x2apic: bool,
) -> Result<VcpuFd, Box<dyn std::error::Error>> {
    let vcpu = vm.create_vcpu(id as u64)?;

    // Pass through host CPUID so the kernel sees real CPU features (RDRAND, SSE, etc)
    let mut cpuid = kvm.get_supported_cpuid(256)?;
    add_kvm_paravirt_leaves(&mut cpuid)?;
    set_apic_id(&mut cpuid, id);
    if !x2apic {
        hide_x2apic(&mut cpuid);
    }
    vcpu.set_cpuid2(&cpuid)?;
    setup_fpu(&vcpu)?;

    if id == 0 {
        setup_gdt(memory)?;
        setup_page_tables(memory)?;
        setup_sregs(&vcpu)?;
        setup_msrs(&vcpu)?;
    }

    Ok(vcpu)
}

/// Whether to offer the guest x2APIC. It is the cheaper interface when every APIC access
/// exits: an MSR write rather than an MMIO fault. With AVIC the hardware handles xAPIC
/// accesses, interrupt delivery and EOIs without exits, but before Zen 4 (`x2avic`) only
/// for xAPIC, and a guest that enables x2APIC switches AVIC off. So x2APIC is hidden exactly
/// when the host's kvm_amd has AVIC on and the CPU lacks x2AVIC. `VM_X2APIC=0|1` overrides.
pub fn guest_x2apic() -> bool {
    if let Ok(v) = std::env::var("VM_X2APIC") {
        return v != "0";
    }
    let avic = std::fs::read_to_string("/sys/module/kvm_amd/parameters/avic")
        .is_ok_and(|v| matches!(v.trim(), "Y" | "1"));
    let x2avic = std::fs::read_to_string("/proc/cpuinfo")
        .is_ok_and(|c| c.lines().find(|l| l.starts_with("flags")).is_some_and(|l| l.split_whitespace().any(|f| f == "x2avic")));
    !avic || x2avic
}

/// CPUID.1:ECX[21] advertises x2APIC.
fn hide_x2apic(cpuid: &mut kvm_bindings::CpuId) {
    for e in cpuid.as_mut_slice() {
        if e.function == 1 && e.index == 0 {
            e.ecx &= !(1 << 21);
        }
    }
}

/// CPUID carries the APIC id in leaf 1 (EBX[31:24]) and leaf 0xB (EDX);
/// the kernel matches these against the MP table.
fn set_apic_id(cpuid: &mut kvm_bindings::CpuId, id: u8) {
    for e in cpuid.as_mut_slice() {
        match e.function {
            1 => e.ebx = (e.ebx & 0x00FF_FFFF) | ((id as u32) << 24),
            0xB => e.edx = id as u32,
            _ => {}
        }
    }
}

/// Tell the guest it runs under KVM. KVM_GET_SUPPORTED_CPUID does not
/// include the hypervisor leaves, and without them the guest never finds
/// kvm-clock, so it has no wall clock from the host and boots with a
/// bogus date (certificates fail, timestamps lie). Advertise the KVM
/// signature and the clocksource features; KVM implements the MSRs
/// behind them regardless.
fn add_kvm_paravirt_leaves(cpuid: &mut kvm_bindings::CpuId) -> Result<(), Box<dyn std::error::Error>> {
    const HYPERVISOR_BIT: u32 = 1 << 31; // CPUID.1:ECX
    const KVM_CPUID_SIGNATURE: u32 = 0x4000_0000;
    const KVM_CPUID_FEATURES: u32 = 0x4000_0001;
    const KVM_FEATURE_CLOCKSOURCE: u32 = 1 << 0;
    const KVM_FEATURE_CLOCKSOURCE2: u32 = 1 << 3;
    // Oversubscription helpers: the guest scheduler sees time stolen by
    // the host, and spinlocks yield instead of spinning on a preempted vCPU.
    const KVM_FEATURE_STEAL_TIME: u32 = 1 << 5;
    const KVM_FEATURE_PV_UNHALT: u32 = 1 << 7;
    const KVM_FEATURE_CLOCKSOURCE_STABLE_BIT: u32 = 1 << 24;

    for e in cpuid.as_mut_slice() {
        if e.function == 1 && e.index == 0 {
            e.ecx |= HYPERVISOR_BIT;
        }
    }
    let sig = kvm_bindings::kvm_cpuid_entry2 {
        function: KVM_CPUID_SIGNATURE,
        eax: KVM_CPUID_FEATURES,
        ebx: u32::from_le_bytes(*b"KVMK"),
        ecx: u32::from_le_bytes(*b"VMKV"),
        edx: u32::from_le_bytes(*b"M\0\0\0"),
        ..Default::default()
    };
    let feat = kvm_bindings::kvm_cpuid_entry2 {
        function: KVM_CPUID_FEATURES,
        eax: KVM_FEATURE_CLOCKSOURCE
            | KVM_FEATURE_CLOCKSOURCE2
            | KVM_FEATURE_STEAL_TIME
            | KVM_FEATURE_PV_UNHALT
            | KVM_FEATURE_CLOCKSOURCE_STABLE_BIT,
        ..Default::default()
    };
    cpuid.push(sig)?;
    cpuid.push(feat)?;
    Ok(())
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
