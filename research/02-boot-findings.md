# Boot Findings — Getting a vmlinux ELF Kernel Running

Notes from the initial bring-up of our KVM microVM, booting a Debian
Trixie 6.12 kernel.

---

## What Works

**vmlinux ELF** loaded via `linux-loader`'s `Elf::load()`, entering at the
ELF entry point (`0x1000c43`) in 64-bit long mode. The kernel's entry
code is already 64-bit — it uses RIP-relative addressing and REX prefixes
from the first instruction.

**bzImage** loads and starts decompressing (UART init happens) but never
produces output. The decompressor appears to hang. We did not debug this
further — vmlinux is the better path anyway since it skips decompression
entirely and is what Firecracker uses.

---

## Kernel Image

The Debian netboot `linux` file is a bzImage (PE/EFI format). To get a
vmlinux ELF, extract it with the kernel's `scripts/extract-vmlinux`:

```
extract-vmlinux vmlinuz > vmlinux
```

The Trixie kernel uses **zstd** compression. The resulting vmlinux is
~55MB, ELF 64-bit, statically linked. The ELF has segments loaded at
physical addresses starting at `0x1000000` (16MB), with virtual addresses
at `0xffffffff81000000`. The loader uses physical addresses.

---

## ELF Entry Point

The ELF entry (`0x1000c43`) is `startup_64` — already 64-bit code.
This is the correct entry for the Linux 64-bit boot protocol when the
VMM enters in long mode.

The kernel also advertises a **PVH entry** (`0x1000720`) via Xen ELF
notes. PVH is a different boot protocol — expects 32-bit protected mode,
no paging, `%ebx` pointing at `hvm_start_info`. We don't use PVH.

The `+0x200` offset (used for bzImage to skip the real-mode header) does
**not** apply to vmlinux ELF images. Use the ELF entry directly.

---

## Critical Register Setup

### What caused triple faults

The kernel triple-faulted immediately when entering with minimal
register setup (just PE + PG + PAE + LME/LMA). The crash RIP was
`0xFFF0` (reset vector), confirming a triple fault.

### What fixed it

The full CR4 and EFER values from the original design doc:

```
CR0  = 0x8005_0033   PE | MP | ET | NE | WP | AM | PG
CR3  = PML4 address  (0x9000)
CR4  = 0x0000_06A0   PAE | MCE | OSFXSR | OSXMMEXCPT
EFER = 0x0000_0D01   SCE | LME | LMA | NXE
```

The critical bits that the minimal setup was missing:

| Bit | Register | Name | Why it matters |
|-----|----------|------|----------------|
| OSFXSR | CR4 | SSE support | Kernel uses SSE from early boot |
| OSXMMEXCPT | CR4 | SSE exceptions | Required with OSFXSR |
| NXE | EFER | No-execute enable | Kernel expects NX page protection |
| SCE | EFER | Syscall enable | Kernel sets up syscall handlers early |

The vmm-reference project uses a minimal CR4 (just PAE) which works for
their test kernels but not for a full Debian production kernel. Firecracker
sets these via CPUID template configuration rather than direct register
writes, which is why their minimal sregs setup works.

### Other register state

From vmm-reference, confirmed necessary:

- **FPU**: `fcw=0x37f, mxcsr=0x1f80` — masks FP/SSE exceptions
- **MSRs**: SYSENTER_CS/ESP/EIP, STAR, LSTAR, CSTAR, SYSCALL_MASK,
  KERNEL_GS_BASE, TSC (all zeroed), MISC_ENABLE=1 (fast string)
- **Stack**: `RSP=RBP=0x8ff0` (not at top of guest RAM)
- **RSI**: points to boot_params at `0x7000`
- **RFLAGS**: `0x2` (bit 1 always set)

---

## Memory Layout

```
0x0000_0500  GDT (4 entries: null, 64-bit code, 64-bit data, TSS)
0x0000_0520  IDT (zeroed)
0x0000_7000  boot_params (zero page)
0x0000_8FF0  initial stack pointer
0x0000_9000  PML4 page table
0x0000_A000  PDPT
0x0000_B000  PD (512 entries, 2MB pages, identity maps 0-1GB)
0x0002_0000  kernel command line
0x0100_0000  kernel load (vmlinux ELF, PhysAddr from ELF headers)
0x0420_0000  kernel end / initrd start
```

The page tables identity-map the first 1GB with 2MB pages. This covers
the kernel (16-66MB), initrd, and all boot structures.

---

## GDT

Matching Firecracker's Linux boot GDT:

```
Entry 0: 0x0000_0000_0000_0000  null
Entry 1: 0x00A0_9B00_0000_FFFF  64-bit code (0xa09b flags)
Entry 2: 0x00C0_9300_0000_FFFF  64-bit data (0xc093 flags)
Entry 3: 0x0080_8B00_0000_FFFF  TSS (0x808b flags)
```

Selectors: CS=0x08, DS/ES/FS/GS/SS=0x10. The TSS entry exists but isn't
loaded into TR during initial setup.

---

## Boot Parameters

For vmlinux ELF, there is no `setup_header` from the loader (unlike
bzImage). We build `boot_params` from scratch:

- `type_of_loader = 0xFF`
- `loadflags |= 0x01` (LOADED_HIGH)
- `cmd_line_ptr` and `cmdline_size` pointing at the command line
- `ramdisk_image` and `ramdisk_size` for the initrd
- E820 map with two entries: base memory (0-640KB) and high memory
  (1MB to end of guest RAM)

The E820 map is what the kernel uses to know how much RAM it has.

---

## What We Didn't Need

- **CPUID configuration** — works without it for basic boot
- **LAPIC configuration** — kernel prints "No local APIC present" but continues
- **ACPI tables** — kernel prints RSDP error but continues
- **MP table** — not needed for single vCPU

---

## Kernel Command Line

```
console=ttyS0 earlyprintk=ttyS0 root=/dev/vda rw
```

`earlyprintk=ttyS0` gives us output during very early kernel init
(before the full console driver is up). Without it, there's a gap
between kernel start and first serial output.

---

## References

- vmm-reference: working rust-vmm example at `/home/slomp/Work/rust-vmm/vmm-reference/`
- Firecracker regs: `/home/slomp/Work/rust-vmm/firecracker/src/vmm/src/arch/x86_64/regs.rs`
- Linux boot protocol: https://www.kernel.org/doc/html/latest/x86/boot.html
