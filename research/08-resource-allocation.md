# Resource Allocation — Memory, vCPUs, and Limits

How KVM guest resources are allocated and controlled. Covers what
Firecracker does for production multi-tenancy and what we can do
more simply for a single-purpose coding agent VM.

---

## Memory

### How guest memory works in KVM

Guest RAM is host userspace memory registered with KVM via
`KVM_SET_USER_MEMORY_REGION`. We `mmap` a contiguous region and tell
KVM "this host virtual range is guest physical range 0 to N."

The host kernel demand-pages this region — physical pages are only
committed when the guest writes to them. A VM configured with 512MB
that only touches 80MB uses ~80MB of host physical RAM. This is
standard Linux behavior, not something we opt into.

Our current setup (`memory.rs`):
```rust
pub const GUEST_MEM_SIZE: usize = 512 * 1024 * 1024; // 512MB
let guest_memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), GUEST_MEM_SIZE)])?;
```

One contiguous region, registered as a single KVM memory slot.

### What we get for free

**Demand paging.** The 512MB mmap creates a virtual address reservation.
Host physical pages are allocated on first write (page fault). A guest
that boots into a shell and runs a coding agent might touch 60-100MB.
The remaining 400MB+ costs nothing on the host — it's just virtual
address space.

**Transparent huge pages (THP).** Linux can back guest memory with 2MB
huge pages automatically, reducing TLB pressure for the guest. Enabled
by default on most distributions. No configuration needed.

**KSM (Kernel Same-page Merging).** If multiple VMs are running, the
host kernel can deduplicate identical pages (common in VMs booting the
same image). We don't need this for single-VM deployments but it's
available.

### The high-water-mark problem

Once the guest touches a page, the host keeps it committed even after
the guest frees the memory internally. The host kernel can't see inside
the guest's page tables — it just knows "this page was written to."

For a short-lived agent VM (boots, runs task, shuts down), this doesn't
matter — the VM process exits and everything is freed. For long-running
VMs where memory usage spikes and recedes, committed memory only grows.

### Virtio-balloon: guest-cooperative memory reclaim

The virtio balloon device (virtio device ID 5) lets the host reclaim
guest memory cooperatively:

**Inflate (reclaim):** Host tells guest "give me back N pages." The
guest's balloon driver identifies free pages and reports their physical
addresses. The VMM calls `madvise(MADV_DONTNEED)` on the corresponding
host pages, releasing them back to the host.

**Deflate (return):** Host tells guest "you can have N pages back." The
guest's balloon driver releases the pages for general use. The host
re-commits them on next access (demand paging).

**How Firecracker does it:**
- Two virtio queues: inflate and deflate
- Guest sends page frame numbers (PFNs) in descriptor chains
- Firecracker compacts PFNs into ranges and calls `madvise(MADV_DONTNEED)`
- Also supports `MADV_FREE` (lazy reclaim — kernel keeps pages but can
  drop them under pressure, avoids unnecessary page faults if the guest
  re-uses them quickly)
- Stats collection via a third queue for memory pressure monitoring
- `deflate_on_oom` option: automatically deflate if guest hits OOM

**What it costs us to implement:**
- A new virtio-mmio device (simpler than net or fs — just two queues
  processing lists of PFNs)
- `madvise` calls on the host side
- Guest kernel already has the `virtio_balloon` driver built in

**When we'd need it:** Only if VMs are long-running and memory usage
fluctuates. A coding agent that runs for 30 seconds doesn't need
balloon. A persistent development VM that compiles, runs tests, then
idles would benefit.

### Virtio-mem: dynamic memory hotplug

More powerful than balloon — actually changes the amount of memory
visible to the guest. The host can add or remove memory blocks at
runtime, and the guest kernel onlines/offlines them.

Firecracker doesn't implement this. QEMU does. It's significantly more
complex than balloon (requires cooperation with the guest's memory
hotplug subsystem, ACPI or virtio signaling for topology changes).

Not relevant for us. Balloon covers the reclaim case, and we can
size guest memory generously upfront since demand paging makes the
idle cost zero.

### Practical recommendation

**Set guest memory to a generous maximum (512MB-1GB).** Demand paging
means idle cost is near zero. The guest never hits artificial limits
during peak usage (compilation, large file operations). No balloon
complexity needed.

If we run multiple VMs per host and need to overcommit host RAM,
balloon is the mechanism. But for single-VM-per-host or low-density
deployments, static allocation with demand paging is simpler and
sufficient.

---

## vCPUs

### Current: single vCPU

Our VMM creates one vCPU (`cpu.rs`):
```rust
let vcpu = vm.create_vcpu(0)?;
```

The run loop is a single `loop { vcpu.run() }` in the main thread.
No threading complexity in the VMM.

### How multi-vCPU works in KVM

Each vCPU is a separate KVM file descriptor created via
`vm.create_vcpu(index)`. Each runs in its own host thread, calling
`KVM_RUN` independently. KVM handles the scheduling internally — the
host kernel treats vCPU threads like normal threads and schedules them
across physical cores.

### What Firecracker does

- Creates N vCPU threads via `thread::Builder::new().name(fc_vcpu N)`
- Each thread runs its own KVM_RUN loop with a state machine
  (Paused → Running → Exited)
- **No CPU pinning** — vCPU threads are scheduled by the host kernel
- Synchronization via `Barrier` at startup (all vCPUs + VMM thread
  synchronize before guest execution begins)
- IPC via channels: VMM sends events (Pause, Resume, SaveState),
  vCPU responds with status
- vCPU kicking via `SIGRTMIN` signal + `immediate_exit` flag to
  interrupt KVM_RUN for state transitions
- MMIO and PIO buses are cloned and shared across all vCPU threads
  (read-only dispatch tables, no locking on the lookup path)

### What multi-vCPU would mean for us

**VMM changes:**
- Spawn N threads, each with its own `VcpuFd` and KVM_RUN loop
- Device MMIO dispatch needs to be thread-safe (currently our
  `VirtioMmioDevice` is owned by the main thread)
- irqfd and ioeventfd are already thread-safe (kernel-side)
- Worker threads (net RX, virtio-fs) already run independently —
  no change needed there

**Guest benefits:**
- `make -j4` would actually parallelize
- The coding agent could run background tasks while the main process
  works
- Boot might be slightly faster (kernel init parallelizes some work)

**Complexity cost:**
- The run loop becomes N threads instead of 1
- MMIO register access needs synchronization (or per-vCPU dispatch)
- Interrupt routing becomes per-vCPU (which vCPU gets the interrupt?)
- Debugging is harder (non-deterministic scheduling)

### Practical recommendation

**Stay with 1 vCPU for now.** Our workload is a single-threaded coding
agent. The guest kernel handles SMP internally if we add more vCPUs,
but the VMM complexity isn't justified until we have a workload that
needs it.

When we need multi-vCPU:
1. Create N VcpuFds, spawn N threads
2. Each thread runs the same KVM_RUN loop
3. Share the MMIO bus via `Arc` (reads are lockless, writes are rare)
4. Use irqfd for all interrupt delivery (already done — thread-safe)
5. No CPU pinning needed (host scheduler handles it)

The migration path is straightforward because our device I/O is already
off the vCPU thread (virtio-fs worker, net RX worker). Adding vCPUs
is mostly about the run loop threading, not device architecture.

---

## Rate Limiting

### What Firecracker does

Token-bucket rate limiters on block and network devices:
- Two independent buckets: bandwidth (bytes/sec) and ops (ops/sec)
- Each bucket has capacity, refill rate, and optional one-time burst
- When tokens are exhausted, a `TimerFd` blocks further I/O until
  refill
- Precise fractional token tracking to avoid integer rounding losses

This exists for multi-tenant isolation — preventing one VM from
monopolizing host I/O bandwidth.

### What we need

Nothing, currently. We're the only VM on the host. S3 throughput is
limited by network/MinIO, not by our device emulation. Network
throughput is limited by the tap device and host network stack.

If we run multiple VMs per host, rate limiting at the device level
would prevent one VM's S3 traffic from starving another's. The
pattern is simple: check a token bucket before issuing each S3
request or network send.

---

## Summary

| Resource | Current | What Firecracker does | What we should do |
|----------|---------|----------------------|-------------------|
| Memory size | 512MB static | Configurable, static | Keep static, bump to 1GB if needed — demand paging makes it free |
| Memory reclaim | None | Virtio-balloon + madvise | Not needed for short-lived VMs |
| vCPUs | 1 | N threads, no pinning | Stay at 1, migration path is clear |
| I/O rate limits | None | Token bucket per device | Not needed for single-VM |
| CPU capping | None | Host cgroups (external) | Use cgroups if deploying |

---

## Core Pinning (Measured 2026-04-08)

### Results

100MB local HTTP download, release build:

| Configuration | Throughput | Notes |
|---------------|-----------|-------|
| No pinning (default) | 127 MB/s | Scheduler bounces threads across cores |
| All threads on 1 core (`taskset -c 0`) | 180 MB/s | **Best** — L1/L2 cache locality |
| 2 cores (`taskset -c 0,1`) | 179 MB/s | Equivalent to single core |

### Analysis

Single-core pinning wins by 42% over unpinned. The vCPU thread and RX
worker thread share the virtqueue descriptor rings in guest memory —
the vCPU writes available ring entries, the RX worker reads them, and
vice versa for the used ring. On one core, those cache lines stay in
L1/L2. Unpinned, the scheduler bounces threads across cores, causing
cache line migration on every virtqueue interaction.

Two cores is equivalent to one core because the scheduler keeps the
threads adjacent on the same physical core's SMT pair or neighboring
cores that share L2.

### Updated with TSO/GRO (iperf3 benchmarks)

With TSO/GRO enabled (60KB packets), results reversed:

| Configuration | Download | Upload |
|---------------|----------|--------|
| Unpinned | **2.70 Gbps** | **1.97 Gbps** |
| Single core | 2.63 Gbps | 1.69 Gbps |

Large packets amortize per-packet overhead, so the vCPU and RX worker
benefit from separate cores. The scheduler keeps them close enough for
cache sharing without explicit pinning.

### Deployment recommendation

**Don't pin.** Let the kernel scheduler handle core assignment. With
TSO/GRO, unpinned is optimal — the scheduler naturally co-locates
threads that share memory while giving each enough CPU time.

For multi-VM density, use cgroups to limit CPU shares rather than
hard-pinning to cores. This lets the scheduler balance load across
the host dynamically.

---

## Summary

The key insight: demand paging and KVM's scheduling give us most of
what we need without any VMM complexity. Balloon and multi-vCPU are
well-understood extensions we can add when the workload demands it,
not speculative infrastructure to build now.
