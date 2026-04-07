# Run Loop Performance — Architectural Analysis

Our VMM is ~2,000 lines of single-threaded Rust. The run loop is a tight
`loop { vcpu.run() }` with match arms for each KVM exit type. This document
catalogs the performance characteristics of that loop, identifies where we're
leaving cycles on the table, and proposes changes that keep the codebase small.

Reference implementation: Firecracker (same rust-vmm crates). We don't want
Firecracker's complexity — its event-manager, bus abstraction, metrics
framework, and snapshot machinery exist for production multi-tenancy concerns
we don't share. But its hot-path decisions are well-reasoned and worth stealing
selectively.

---

## Current Architecture

```
loop {
    vcpu.run()  ──────────────────────────  KVM_RUN ioctl (blocks)
    match exit {
        IoIn/IoOut      → serial register access
        MmioRead        → virtio register read
        MmioWrite       → virtio register write
                          if queue notify:
                            process_queue() / process_tx()     ← synchronous
                            set_irq_line(true)                 ← ioctl
                            set_irq_line(false)                ← ioctl
        Hlt             → process_rx() (one packet)
        Shutdown        → break
    }
}
```

Everything is synchronous, single-threaded, in the vCPU thread. The guest
cannot execute while we handle an exit. This is actually a fine starting point
— Firecracker's vCPU run loop is also a tight single-thread loop per vCPU.
The differences are in what happens *inside* the exit handlers.

---

## 1. Interrupt Injection: set_irq_line vs irqfd

### Problem

Every interrupt requires two KVM ioctls:

```rust
self.vm.set_irq_line(IRQ, true).ok();   // assert
self.vm.set_irq_line(IRQ, false).ok();  // deassert
```

Each ioctl is a syscall: userspace → kernel → ioctl dispatch → irqchip
emulation → return. Two per interrupt, on every virtio-fs FUSE response,
every TX completion, every RX delivery, and every serial character.

### What Firecracker Does

Registers an `EventFd` with KVM via `KVM_IRQFD` once at device activation.
Interrupt injection becomes a single `eventfd.write(1)` — which KVM handles
internally without a VMM round-trip. KVM also coalesces multiple eventfd
writes into a single interrupt delivery if the guest hasn't consumed the
previous one yet.

### Proposed Change

```rust
// At device setup (once per device):
let irqfd = EventFd::new(EFD_NONBLOCK)?;
vm.register_irqfd(&irqfd, device_irq)?;

// In hot path (replaces two set_irq_line calls):
irqfd.write(1)?;
```

This is ~5 lines of new code per device. The `kvm-ioctls` crate already
exposes `register_irqfd`. The serial device's `IrqTrigger` can switch to
the same mechanism.

**Impact**: Eliminates 1 syscall per interrupt (2 → 1), with KVM-side
coalescing as a bonus. Applies to every device path.

---

## 2. Network RX: HLT-Triggered Polling

### Problem

Network packet receive only happens when the guest executes HLT:

```rust
VcpuExit::Hlt => {
    if let Some(ref mut net) = self.virtionet {
        if net.process_rx(&self.memory) { ... }
    }
}
```

Three issues:
1. **Latency**: packets only arrive when the guest is idle (HLT). If the
   guest is busy, inbound packets are invisible to it.
2. **Throughput**: only one packet per HLT exit — `process_rx` does a single
   `recv()` and returns. A burst of 100 packets takes 100 HLT cycles.
3. **No wakeup**: if a packet arrives while the guest is running (not halted),
   nothing interrupts the guest. The packet sits in the kernel socket buffer
   until the next HLT.

The `NetBackend` trait already exposes `rx_fd()` for exactly this purpose —
it's designed for epoll registration but we never use it.

### Options (Simplest to Most Complex)

**A. Drain loop + post-exit poll (minimal change)**

After every KVM_RUN exit (not just HLT), do a non-blocking check:

```rust
// After the main match block, before looping:
if let Some(ref mut net) = self.virtionet {
    while net.process_rx(&self.memory) {  // drain, not just one
        net.mmio.interrupt_status |= 0x01;
        irqfd.write(1)?;
    }
}
```

Cost: one extra `read()` syscall per exit that returns EAGAIN. But it means
packets are noticed on any exit, not just HLT, and bursts are drained
completely.

**B. epoll on tap fd with KVM signal mask (proper, still single-threaded)**

Use `epoll_create` + register `rx_fd()` with EPOLLIN. Before `vcpu.run()`,
set up `KVM_SET_SIGNAL_MASK` so that a signal can interrupt the vCPU when
the tap fd becomes readable. On EINTR from KVM_RUN, check epoll, drain RX,
re-enter.

This is the canonical single-threaded VMM approach. It adds maybe 40 lines
but gives true event-driven RX with zero polling overhead when there are no
packets.

**C. Dedicated RX thread**

Spawn a thread that blocks on `read(rx_fd)`, writes packets into the RX
virtqueue, and signals via irqfd. Completely decouples RX from vCPU
scheduling.

More complex (shared queue access needs care), but eliminates any RX latency
from vCPU exit handling.

### Recommendation

Start with **A** — it's a 5-line change that fixes the single-packet and
HLT-only problems. Move to **B** when we want to eliminate the per-exit
polling syscall. **C** is only needed if RX latency becomes critical and the
guest is doing sustained CPU work.

---

## 3. Hot-Path Heap Allocations

### Problem

Every packet processed allocates and zeroes 64KB on the heap:

```rust
// virtio_net.rs:95 (process_tx) and :117 (process_rx)
let mut buf = vec![0u8; 65536];
```

`vec![0; 65536]` calls `alloc::alloc_zeroed` (mmap or malloc+memset depending
on allocator) on every TX descriptor chain and every RX attempt. Under load
this is thousands of allocations per second, each touching 16 pages.

### Fix

Make `buf` a field on `VirtioNet`, allocated once at construction:

```rust
pub struct VirtioNet<B: NetBackend> {
    pub mmio: VirtioMmioDevice,
    queues: Vec<Queue>,
    backend: B,
    mac: [u8; 6],
    buf: Box<[u8; 65536]>,  // single allocation, reused forever
}
```

The xdp-net crate already does this internally — `XdpBackend` pre-allocates
all its batch vectors at construction time. We should match that pattern in
the virtio-net layer.

**Impact**: Eliminates ~65KB alloc+zero per packet. Trivial change.

---

## 4. Virtio-fs Blocks the vCPU on S3 HTTP

### Problem

This is the elephant in the room. When the guest does a file read:

```
guest read() → FUSE request → virtio queue → MMIO write exit
  → process_queue() → server.handle_message()
    → S3Client::get_range() → HTTP GET to MinIO → blocks until response
  → write response to virtqueue → inject interrupt → re-enter guest
```

The entire vCPU is stalled for the duration of an HTTP round-trip. No guest
instructions execute. No other I/O is serviced. Serial output freezes. Network
packets pile up.

For sequential file access this might be tolerable (the guest is waiting for
data anyway). But for concurrent guest activity — a build system reading
headers while writing object files while running tests — every file operation
serializes everything.

### The Simple Truth

This is an inherent consequence of synchronous FUSE-over-virtio in a
single-threaded VMM. There are two ways to break the serialization:

**A. Thread pool for FUSE dispatch**

Move `server.handle_message()` to a thread pool. The main loop submits FUSE
requests to the pool and continues running the vCPU. When a request completes,
the pool thread writes the response to the virtqueue and signals via irqfd.

This is what virtiofsd (the reference C implementation) does — it runs a
thread per request queue. Firecracker's vhost-user-fs delegates to an
external daemon that does the same.

The complexity here is shared access to the virtqueue between the vCPU thread
(which handles MMIO exits) and the worker threads (which write responses).
This requires a lock on the queue, but it's not the hot path — the lock is
only held briefly to pop/push descriptors.

**B. Async FUSE with io_uring or epoll**

Make the S3 client async and integrate its event loop with the vCPU run loop.
When a FUSE request arrives, start the HTTP request and immediately return to
the guest. When the response arrives (signaled via epoll), inject the
completion.

This is architecturally cleaner but requires an async HTTP client and an event
loop that multiplexes with KVM_RUN. It's a bigger change to the overall
architecture.

### Recommendation

**A** is the pragmatic choice. The S3 client stays synchronous (ureq is
simple and correct). The thread pool is a bounded `std::thread::scope` or
a small channel-based worker. The main loop complexity increase is modest:
instead of calling `process_queue()` directly, it sends work to the pool
and checks for completions.

But this is not urgent until we've measured actual latency bottlenecks. If
the guest workload is "run a coding agent that reads files sequentially,"
the synchronous path may be fast enough — the guest is blocked on the read
anyway. If the workload is "compile a project with make -j4," the
serialization will hurt.

---

## 5. MMIO Dispatch: Address Matching in the Run Loop

### Current

```rust
VcpuExit::MmioWrite(addr, data) => {
    match addr {
        VIRTIO_FS_BASE..=0xD000_0FFF => { /* virtiofs handling */ }
        VIRTIO_NET_BASE..=0xD000_1FFF => { /* virtionet handling */ }
        _ => {}
    }
}
```

Each device is a hardcoded match arm with inline logic. With 2 devices this
is perfectly fine. Every MMIO exit takes a single comparison to dispatch.

### What Firecracker Does

Uses a `Bus` struct backed by a `BTreeMap<u64, BusDevice>`. MMIO exits call
`bus.read(addr, data)` or `bus.write(addr, data)`, which does O(log n) lookup
to find the device. Devices implement a `BusDevice` trait. This decouples the
run loop from device specifics entirely.

### Assessment

We don't need this yet. The BTreeMap lookup is actually *slower* than our
flat match for 2 devices (branch prediction on the match arm vs. tree walk).
The Bus pattern pays off when:
- You have many devices and adding each one to the run loop is error-prone
- Devices are dynamically added/removed (hotplug)
- You want device implementations to be self-contained modules

If we add a third device, refactoring to a trait-based dispatch is worth
considering. Until then, the flat match is simpler and faster.

One cleanup that *is* worth doing: extract the per-device MMIO+notify+IRQ
logic into methods on the device structs so the run loop match arms are
one-liners. This doesn't change performance but keeps the run loop readable
as device logic grows.

---

## 6. Notification Suppression (VIRTIO_F_EVENT_IDX)

### Problem

Every time the guest posts a buffer to any virtqueue, it writes to the
QueueNotify MMIO register, causing a VM exit. Every time we complete a
request, we inject an interrupt. Neither side has a way to say "don't
bother me until you've caught up."

Under load, this means:
- Guest posts 10 buffers → 10 MMIO exits (10 KVM_RUN round-trips)
- We complete 10 requests → 10 interrupts (10 irqfd writes or ioctl pairs)

### What the Virtio Spec Provides

`VIRTIO_F_EVENT_IDX` (feature bit 29) enables notification suppression:

- **Device → driver** (used_event): the device writes a `used_event` index
  in the available ring. The driver only notifies when it passes that index.
  Effect: the guest batches buffer submissions.

- **Driver → device** (avail_event): the driver writes an `avail_event`
  index in the used ring. The device only interrupts when it passes that
  index. Effect: we batch completion interrupts.

Firecracker implements this in `Queue::prepare_kick()`:
```rust
fn prepare_kick(&mut self) -> bool {
    if !self.uses_notif_suppression { return true; }
    fence(Ordering::SeqCst);
    let new = self.next_used;
    let old = self.next_used - self.num_added;
    let used_event = Wrapping(self.avail_ring_used_event_get());
    self.num_added = Wrapping(0);
    new - used_event - Wrapping(1) < new - old
}
```

### Assessment

This is a medium-sized change — the MMIO transport needs to negotiate the
feature bit, and the queue processing paths need to check suppression indices
before notifying/interrupting. The virtio-queue crate may already support
some of this.

Worth implementing when we see high exit rates under load. For now, every
MMIO register access is an exit anyway (we don't use ioeventfd), so
suppressing some notifications won't eliminate the exit cost — just the
interrupt injection cost. It becomes much more valuable after ioeventfd
(see below).

---

## 7. ioeventfd: Eliminating Queue Notify Exits

### Problem

When the guest writes to the QueueNotify register (offset 0x050), KVM
exits to userspace so we can process the queue. This is the most frequent
MMIO exit — every buffer the guest posts triggers one.

### What KVM Provides

`KVM_IOEVENTFD` lets you register an eventfd for a specific MMIO address.
When the guest writes to that address, KVM signals the eventfd *without
exiting to userspace*. The VMM thread (or a worker thread) can poll the
eventfd to know when to process the queue.

### Combined with irqfd

Together, irqfd and ioeventfd create a **fully in-kernel virtio fast path**:

```
Guest writes to QueueNotify → KVM signals ioeventfd (no exit)
VMM thread sees ioeventfd   → processes queue → writes irqfd
KVM delivers interrupt      → guest sees completion (no exit)
```

The only VM exits that remain are MMIO register reads/writes for device
configuration (initialization, feature negotiation) — not the data path.

### Assessment

This is the single highest-impact optimization for steady-state throughput.
It eliminates the most frequent VM exit entirely. But it requires rethinking
how queue processing is triggered — instead of inline in the run loop, it
needs to be event-driven (epoll on the ioeventfd).

This naturally pairs with the async virtiofs change (section 4) and the
epoll-based RX change (section 2B). If we're going to add an epoll loop,
we should add all three event sources at once: ioeventfd for queue kicks,
tap fd for network RX, and completion channel for async FUSE.

---

## 8. Memory Ordering in Virtqueue Access

### Current State

We use `virtio-queue` crate's `Queue` type which handles descriptor chain
traversal. But the interaction between our MMIO transport (`VirtioMmioDevice`)
and the queue operations may be missing required memory barriers.

The virtio spec requires:
- **Device reads available ring**: needs acquire fence after reading
  `avail.idx` (ensures descriptor data is visible)
- **Device writes used ring**: needs release fence before writing
  `used.idx` (ensures response data is visible before guest sees index)

The `virtio-queue` crate handles fences for `pop_descriptor_chain` and
`add_used`, so we're likely correct. But we should verify we're not
reordering around these calls — particularly in the MMIO write handler
where we parse the value, call `mmio.write()`, then `process_queue()`.

Firecracker is explicit about this:
```rust
fn advance_used_ring_idx(&mut self) {
    fence(Ordering::Release);  // all descriptor writes visible before index
    self.used_ring_idx_set(self.next_used.0);
}
```

### Assessment

Low risk since `virtio-queue` handles the critical fences internally, and
we're single-threaded (no cross-thread reordering to worry about). But worth
auditing if we add threading for async FUSE.

---

## Priority Ordering

What to do, in order of effort-to-impact ratio:

| # | Change | Lines | Impact | When |
|---|--------|-------|--------|------|
| 1 | Reusable packet buffer | ~10 | Eliminates 64KB alloc/zero per packet | Now |
| 2 | irqfd for interrupts | ~20 | Eliminates 1 syscall per interrupt | Now |
| 3 | Drain RX loop + check on all exits | ~10 | Fixes RX throughput and latency | Now |
| 4 | Extract device methods from run loop | ~30 | Readability, no perf change | Now |
| 5 | Notification suppression | ~60 | Reduces interrupt/exit rate under load | Soon |
| 6 | ioeventfd for queue notify | ~40 | Eliminates most frequent VM exit | Soon |
| 7 | epoll integration (RX + ioeventfd) | ~80 | Event-driven I/O, no polling | Soon |
| 8 | Async virtiofs (thread pool) | ~100 | Unblocks vCPU during S3 I/O | When needed |

Items 1-4 are independent, low-risk, and can be done in any order. Items 5-7
build on each other and should be planned together. Item 8 is the largest
change and should wait until we measure actual filesystem latency impact.

---

## What We're Deliberately Not Doing

- **Event-manager crate**: Firecracker's subscriber/event-ops pattern adds
  indirection we don't need for 1 vCPU and 2-3 devices. A raw epoll loop
  with 3-4 fds is simpler and faster.

- **Bus abstraction**: BTreeMap device dispatch is slower than a flat match
  for our device count. We'll reconsider at 4+ devices.

- **Metrics framework**: Firecracker's METRICS.vcpu.exit_io_in.inc() is
  great for production observability but we're better served by targeted
  tracing when investigating specific issues.

- **Seccomp filters**: Important for multi-tenant production, not for a
  single-purpose coding agent VM.

- **Multi-vCPU**: Our workload is a single agent process. One vCPU avoids
  all cross-thread synchronization complexity in the VMM. If we need
  parallelism, the guest can use it — KVM handles multi-vCPU scheduling
  internally. But the VMM stays simpler with one.

- **Device hotplug**: We know our device set at boot time. Static
  configuration means no dynamic bus management.
