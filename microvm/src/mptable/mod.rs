// MP Table writer — tells the kernel about LAPIC, IO-APIC, and IRQ routing.
// Adapted from vmm-reference (Apache-2.0 / BSD-3-Clause).

use std::{mem, slice};

use libc::c_char;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]
mod mpspec;

// Newtype wrappers for ByteValued impl
#[derive(Copy, Clone, Default)] #[repr(transparent)]
struct MpfIntel(mpspec::mpf_intel);
#[derive(Copy, Clone, Default)] #[repr(transparent)]
struct MpcTable(mpspec::mpc_table);
#[derive(Copy, Clone, Default)] #[repr(transparent)]
struct MpcCpu(mpspec::mpc_cpu);
#[derive(Copy, Clone, Default)] #[repr(transparent)]
struct MpcBus(mpspec::mpc_bus);
#[derive(Copy, Clone, Default)] #[repr(transparent)]
struct MpcIoapic(mpspec::mpc_ioapic);
#[derive(Copy, Clone, Default)] #[repr(transparent)]
struct MpcIntsrc(mpspec::mpc_intsrc);
#[derive(Copy, Clone, Default)] #[repr(transparent)]
struct MpcLintsrc(mpspec::mpc_lintsrc);

unsafe impl vm_memory::ByteValued for MpfIntel {}
unsafe impl vm_memory::ByteValued for MpcTable {}
unsafe impl vm_memory::ByteValued for MpcCpu {}
unsafe impl vm_memory::ByteValued for MpcBus {}
unsafe impl vm_memory::ByteValued for MpcIoapic {}
unsafe impl vm_memory::ByteValued for MpcIntsrc {}
unsafe impl vm_memory::ByteValued for MpcLintsrc {}

const MPTABLE_START: u64 = 0x9fc00;
const IO_APIC_ADDR: u32 = 0xfec0_0000;
const LAPIC_ADDR: u32 = 0xfee0_0000;
const APIC_VERSION: u8 = 0x14;
const NUM_IRQS: u8 = 24; // ISA IRQs 0-15 + 8 extra IO-APIC pins

macro_rules! char_array {
    ($t:ty; $( $c:expr ),*) => ( [ $( $c as $t ),* ] )
}

const SMP_MAGIC: [c_char; 4] = char_array!(c_char; '_', 'M', 'P', '_');
const MPC_SIGNATURE: [c_char; 4] = char_array!(c_char; 'P', 'C', 'M', 'P');
const MPC_OEM: [c_char; 8] = char_array!(c_char; 'm', 'i', 'c', 'r', 'o', 'v', 'm', ' ');
const MPC_PRODUCT: [c_char; 12] = ['0' as c_char; 12];
const BUS_TYPE_ISA: [u8; 6] = char_array!(u8; 'I', 'S', 'A', ' ', ' ', ' ');

fn checksum<T: Copy>(v: &T) -> u8 {
    let bytes = unsafe { slice::from_raw_parts(v as *const T as *const u8, mem::size_of::<T>()) };
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

pub fn write(memory: &GuestMemoryMmap, num_cpus: u8) -> Result<(), Box<dyn std::error::Error>> {
    let mut addr = GuestAddress(MPTABLE_START);
    let num_cpus = num_cpus.max(1);
    let ioapic_id: u8 = num_cpus; // CPUs take APIC IDs 0..N-1, the IO-APIC the next one

    // Zero the region
    let total_size = mem::size_of::<MpfIntel>()
        + mem::size_of::<MpcTable>()
        + mem::size_of::<MpcCpu>() * num_cpus as usize
        + mem::size_of::<MpcBus>()
        + mem::size_of::<MpcIoapic>()
        + mem::size_of::<MpcIntsrc>() * NUM_IRQS as usize
        + mem::size_of::<MpcLintsrc>() * 2;
    let zeros = vec![0u8; total_size];
    memory.write_slice(&zeros, addr)?;

    // Floating pointer — points at the config table that follows
    let mpf_size = mem::size_of::<MpfIntel>() as u64;
    let mut mpf = MpfIntel(mpspec::mpf_intel {
        signature: SMP_MAGIC,
        length: 1,
        specification: 4,
        physptr: (addr.raw_value() + mpf_size) as u32,
        ..Default::default()
    });
    let cs = checksum(&mpf.0).wrapping_sub(mpf.0.checksum);
    mpf.0.checksum = (!cs).wrapping_add(1);
    memory.write_obj(mpf, addr)?;
    addr = addr.unchecked_add(mpf_size);

    // Remember where the config table header goes — fill it in at the end
    let table_base = addr;
    addr = addr.unchecked_add(mem::size_of::<MpcTable>() as u64);

    let mut table_checksum: u8 = 0;

    // One processor entry per vCPU; APIC id matches what cpu.rs puts in CPUID.
    for id in 0..num_cpus {
        let bsp = if id == 0 { mpspec::CPU_BOOTPROCESSOR } else { 0 };
        let cpu = MpcCpu(mpspec::mpc_cpu {
            type_: mpspec::MP_PROCESSOR as u8,
            apicid: id,
            apicver: APIC_VERSION,
            cpuflag: (mpspec::CPU_ENABLED | bsp) as u8,
            cpufeature: 0x600, // stepping
            featureflag: 0x201, // FPU + APIC
            ..Default::default()
        });
        memory.write_obj(cpu, addr)?;
        table_checksum = table_checksum.wrapping_add(checksum(&cpu.0));
        addr = addr.unchecked_add(mem::size_of::<MpcCpu>() as u64);
    }

    // ISA bus
    let bus = MpcBus(mpspec::mpc_bus {
        type_: mpspec::MP_BUS as u8,
        busid: 0,
        bustype: BUS_TYPE_ISA,
    });
    memory.write_obj(bus, addr)?;
    table_checksum = table_checksum.wrapping_add(checksum(&bus.0));
    addr = addr.unchecked_add(mem::size_of::<MpcBus>() as u64);

    // IO-APIC
    let ioapic = MpcIoapic(mpspec::mpc_ioapic {
        type_: mpspec::MP_IOAPIC as u8,
        apicid: ioapic_id,
        apicver: APIC_VERSION,
        flags: mpspec::MPC_APIC_USABLE as u8,
        apicaddr: IO_APIC_ADDR,
    });
    memory.write_obj(ioapic, addr)?;
    table_checksum = table_checksum.wrapping_add(checksum(&ioapic.0));
    addr = addr.unchecked_add(mem::size_of::<MpcIoapic>() as u64);

    // IRQ source entries: ISA IRQ i → IO-APIC pin i
    for i in 0..NUM_IRQS {
        let intsrc = MpcIntsrc(mpspec::mpc_intsrc {
            type_: mpspec::MP_INTSRC as u8,
            irqtype: mpspec::mp_irq_source_types_mp_INT as u8,
            irqflag: mpspec::MP_IRQDIR_DEFAULT as u16,
            srcbus: 0,
            srcbusirq: i,
            dstapic: ioapic_id,
            dstirq: i,
        });
        memory.write_obj(intsrc, addr)?;
        table_checksum = table_checksum.wrapping_add(checksum(&intsrc.0));
        addr = addr.unchecked_add(mem::size_of::<MpcIntsrc>() as u64);
    }

    // Local interrupt: ExtINT on LINT0
    let lint0 = MpcLintsrc(mpspec::mpc_lintsrc {
        type_: mpspec::MP_LINTSRC as u8,
        irqtype: mpspec::mp_irq_source_types_mp_ExtINT as u8,
        irqflag: mpspec::MP_IRQDIR_DEFAULT as u16,
        srcbusid: 0,
        srcbusirq: 0,
        destapic: 0,
        destapiclint: 0,
    });
    memory.write_obj(lint0, addr)?;
    table_checksum = table_checksum.wrapping_add(checksum(&lint0.0));
    addr = addr.unchecked_add(mem::size_of::<MpcLintsrc>() as u64);

    // Local interrupt: NMI on LINT1 (broadcast to all CPUs)
    let lint1 = MpcLintsrc(mpspec::mpc_lintsrc {
        type_: mpspec::MP_LINTSRC as u8,
        irqtype: mpspec::mp_irq_source_types_mp_NMI as u8,
        irqflag: mpspec::MP_IRQDIR_DEFAULT as u16,
        srcbusid: 0,
        srcbusirq: 0,
        destapic: 0xFF,
        destapiclint: 1,
    });
    memory.write_obj(lint1, addr)?;
    table_checksum = table_checksum.wrapping_add(checksum(&lint1.0));
    addr = addr.unchecked_add(mem::size_of::<MpcLintsrc>() as u64);

    // Now write the config table header
    let mut table = MpcTable(mpspec::mpc_table {
        signature: MPC_SIGNATURE,
        length: addr.unchecked_offset_from(table_base) as u16,
        spec: 4,
        oem: MPC_OEM,
        productid: MPC_PRODUCT,
        lapic: LAPIC_ADDR,
        ..Default::default()
    });
    table_checksum = table_checksum.wrapping_add(checksum(&table.0));
    table.0.checksum = (!table_checksum).wrapping_add(1) as i8;
    memory.write_obj(table, table_base)?;

    Ok(())
}
