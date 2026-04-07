# Platform Compatibility — Host Requirements & Deployment Targets

Our VMM requires KVM with specific x86_64 capabilities. This document maps
out what the host must provide, which deployment targets work, and what
constraints apply to networking in each environment.

---

## Host Requirements

### KVM (mandatory)

The VMM exercises these KVM ioctls during startup, in order:

| Ioctl / Capability | Purpose |
|---|---|
| `/dev/kvm` (rw) | KVM subsystem access |
| `KVM_CHECK_EXTENSION(CAP_USER_MEMORY)` | Guest memory via userspace mmap |
| `KVM_CHECK_EXTENSION(CAP_IRQCHIP)` | In-kernel PIC/IOAPIC/LAPIC |
| `KVM_CHECK_EXTENSION(CAP_SET_TSS_ADDR)` | x86 TSS setup |
| `KVM_CREATE_VM` | VM file descriptor |
| `KVM_SET_TSS_ADDR(0xFFFBD000)` | TSS placement |
| `KVM_CREATE_IRQCHIP` | In-kernel interrupt controller |
| `KVM_CREATE_PIT2` | 8254 timer |
| `KVM_SET_USER_MEMORY_REGION` | 256MB guest RAM |
| `KVM_CREATE_VCPU` | vCPU 0 |
| `KVM_SET_CPUID2` | Host CPUID passthrough |
| `KVM_SET_SREGS` | Long mode (cr0/cr3/cr4/efer) |
| `KVM_SET_REGS` | Entry point, stack, flags |
| `KVM_SET_FPU` | FPU/SSE control words |
| `KVM_SET_MSRS` | 10 MSRs (SYSENTER, SYSCALL, TSC, MISC_ENABLE) |
| `KVM_IRQ_LINE` | Edge-triggered interrupt injection |

All of these are exercised by the `compat/` checker binary, which can be
built and run on any candidate host to get a pass/fail verdict.

### Network (optional)

| Backend | Device / Module | Capabilities | Use case |
|---|---|---|---|
| macvtap | `/dev/tap{N}`, `macvtap` module | none (root or device access) | Bare metal, cloud VMs |
| XDP | AF_XDP socket, BPF | `CAP_NET_ADMIN`, `CAP_BPF` | Bare metal with supported NIC |
| TAP (future) | `/dev/net/tun` | `CAP_NET_ADMIN` | Containers, cloud VMs |

macvtap is our primary backend. The orchestrator creates the macvtap
interface; the VMM just opens the character device and moves packets.
No bridges, no NAT — routing is the network layer's problem.

XDP is a performance option for bare metal with NICs that support native
XDP mode. On virtual interfaces it falls back to generic/SKB mode which
offers no advantage over macvtap.

TAP is not yet implemented but would be the portable container backend.
Structurally identical to macvtap — the only difference is the open path
(`/dev/net/tun` + `TUNSETIFF` vs `/dev/tap{N}`). The orchestrator or
container runtime handles routing.

---

## Architecture-Specific Code

Everything portable (virtio devices, S3 filesystem, network backends, run
loop) is already architecture-independent. The x86-specific pieces are
isolated in three modules:

| Module | What's x86-specific |
|---|---|
| `cpu.rs` | Long mode setup (GDT, page tables, MSRs, CPUID, cr0/cr3/cr4/efer), FPU/SSE init |
| `kernel.rs` | bzImage boot protocol, "zero page" boot params, E820 memory map |
| `mptable/` | MP table (SMP enumeration for PIC/APIC/IOAPIC) |

An aarch64 port would replace these with: GICv2/v3 setup, device tree blob,
PE/Image kernel loading, PSCI for CPU management. The rust-vmm crates
(`kvm-bindings`, `kvm-ioctls`, `linux-loader`, `vm-superio`, `virtio-queue`)
all support x86_64, aarch64, and riscv64.

---

## Deployment Targets

### Bare Metal

Full native KVM. All backends work. Best performance — no virtualization
overhead. XDP with native driver support available.

**Requirements**: Linux host with KVM module, hardware VT-x/AMD-V.

### AWS EC2

**`.metal` instances** — native KVM, no nesting. All current instance
generations have `.metal` variants (m7i, c7i, r7i, m7a, c7a, m7g, c7g,
etc.). Graviton `.metal` provides ARM KVM only (not usable with our x86
VMM without the aarch64 port).

Cheapest x86 `.metal`: ~$4/hr on-demand (m7i.metal-24xl). Spot pricing
available but with limited capacity.

**C8i / M8i / R8i** — nested KVM on non-`.metal` instances. Announced
February 2026. Intel Xeon 6 processors. Opt-in at launch:

```bash
aws ec2 run-instances \
    --instance-type c8i.xlarge \
    --cpu-options "NestedVirtualization=enabled" \
    ...
```

Or on an existing stopped instance:

```bash
aws ec2 modify-instance-cpu-options \
    --instance-id i-xxx \
    --nested-virtualization enabled
```

This is the first time AWS has exposed KVM on virtualized instances. Only
the 8th-gen Intel families support it — older families (c7i, m7i, etc.)
do not. No additional cost. Available in all commercial regions.

Nested virt adds ~10-15% overhead on VM exits vs bare metal, but our
workload (mostly userspace compute + S3 I/O) should not be heavily
affected.

**ECS**: Container tasks can access `/dev/kvm` via `linuxParameters.devices`
in the task definition, provided the underlying EC2 instances are `.metal`
or nested-virt-enabled 8th gen.

**Fargate / Lambda**: No KVM access. Sandboxed environments, no device
passthrough.

### GCP

Nested KVM on any Intel or AMD VM. Free, no special instance tier. Enabled
at instance creation:

```bash
gcloud compute instances create vm-host \
    --enable-nested-virtualization \
    --min-cpu-platform="Intel Haswell" \
    ...
```

GKE: supported via custom node images with nested virt license + KVM device
plugin (`devices.kubevirt.io/kvm`).

XDP: generic/SKB mode only (virtual NIC), no performance advantage over
macvtap.

### Hetzner

Dedicated servers provide native KVM. No nesting — you get the hardware.
Cheapest option at ~EUR40/mo for a dedicated server with full L2 networking.
API-driven provisioning available.

Cloud VMs (shared/dedicated vCPU): untested for nested KVM. Dedicated
servers are the reliable path.

### Windows (WSL2)

KVM works on the stock WSL2 kernel — `CONFIG_KVM=y`, `CONFIG_KVM_INTEL=y`,
and `CONFIG_KVM_AMD=y` are compiled in. Requires opt-in via
`%USERPROFILE%\.wslconfig`:

```ini
[wsl2]
nestedVirtualization=true
```

The virtualization chain is: Hardware (VT-x/AMD-V) → Hyper-V → WSL2 VM →
KVM → guest. All required KVM capabilities (irqchip, TSS, PIT, CPUID,
MSRs) are available. Community reports confirm QEMU/KVM and Firecracker
running successfully in WSL2.

**Caveats:**
- The host must be bare metal Windows (not a VM running Windows)
- Hyper-V may filter some CPUID leaves — the compat checker catches this
- Some users on Windows 11 24H2 see a "nested virtualization not supported"
  warning that is a false positive on supported hardware

**Networking:** macvtap does not work — WSL2's `eth0` is a Hyper-V virtual
switch NIC, not a standard ethernet device. TAP is viable (`CONFIG_TUN=y`
in the stock kernel, `CAP_NET_ADMIN` available) but routing must be handled
externally — WSL2 uses NAT or mirrored networking, no bridgeable L2
interface. XDP is not practical (virtual NIC, no native XDP support).

**Verdict:** Development and testing only. KVM works, networking needs the
future TAP backend with orchestrator-managed routing.

### Incompatible Platforms

| Platform | Why |
|---|---|
| Railway | No `/dev/kvm`, no privileged containers, no plans to change |
| AWS Fargate | Firecracker sandbox, no device access |
| AWS Lambda | Firecracker sandbox, no device access |
| GCP Cloud Run | gVisor sandbox, no device access |
| Any serverless / managed container platform | No device passthrough by design |

---

## Compatibility Checker

`compat/` contains a standalone Rust binary that exercises every KVM ioctl
the VMM uses, plus checks for network device availability. Build and run
on any candidate host:

```bash
cd compat && cargo build --release
./target/release/microvm-compat
```

Or via Docker on a remote host:

```bash
docker build -t microvm-compat compat/
docker run --device /dev/kvm microvm-compat
```

Output is pass/fail for each KVM requirement (hard fail = can't run VMM)
plus advisory warnings for missing network capabilities. Also prints host
fingerprint (kernel, CPU, virt flags, hypervisor/container detection).

---

## Network Backend Selection by Environment

| Environment | Recommended backend | Why |
|---|---|---|
| Bare metal | macvtap (or XDP with supported NIC) | Full L2 access, best performance |
| AWS `.metal` | macvtap | Native NIC, full L2 |
| AWS C8i/M8i/R8i | macvtap | ENA virtual NIC supports macvtap |
| GCP nested | macvtap | virtio-net NIC supports macvtap |
| Hetzner dedicated | macvtap (or XDP) | Real NIC, full L2 |
| Docker | TAP (future) | Container network namespace, no host NIC access |
| Kubernetes | TAP (future) + Multus for L2 | Pod network namespace |
| WSL2 | TAP (future) | Hyper-V vNIC, no macvtap, no L2 |

The VMM never creates bridges, NAT rules, or routing entries. It opens
the interface it's given and moves packets. Network topology is the
orchestrator's responsibility.
