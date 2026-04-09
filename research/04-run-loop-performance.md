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

### Resolution — RESOLVED

All three devices use `irqfd`. Each `VirtioMmioDevice` owns an `EventFd`
created at construction (`virtio_mmio.rs:68`), registered with KVM at
system init (`main.rs:62,69`). The serial device does the same
(`serial.rs:28-29`). Interrupt injection is a single `inject_irq()` method
(`virtio_mmio.rs:188-191`) that does `fetch_or` on an `AtomicU32` and
`irqfd.write(1)`.

The `interrupt_status` field is now `AtomicU32` instead of plain `u32`,
which is correct — the RX worker thread sets it from a different thread
(`virtio_net.rs:194`). The `InterruptACK` handler uses `fetch_and`
(`virtio_mmio.rs:141`).

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

### Resolution — RESOLVED (exceeded recommendation)

Went directly to option C (dedicated thread). The worker
(`virtio_net.rs:121-201`):
- Blocks on `poll(rx_fd, POLLIN, -1)` — no busy-waiting
- Drains all available packets in a loop until `WouldBlock`
- Uses `recv_iovec` for zero-copy delivery directly into guest memory
- Signals via `irqfd.write(1)` — KVM delivers the interrupt without
  involving the vCPU thread at all
- HLT handler is now empty (`main.rs:167`) — correct, nothing to do

This is the right call — the thread model is actually simpler in practice
because the vCPU run loop doesn't need to think about RX at all.

**Ownership model** is clean: RX queue and backend are moved into the worker
thread via `Option::take()` (`virtio_net.rs:123-129`). No `Arc<Mutex<>>` in
the packet path. TX uses a dup'd fd (`virtio_net.rs:54`) so both threads have
independent file descriptors to the same macvtap device.

One subtlety: `interrupt_status` is shared between the RX worker and the vCPU
thread (the worker sets it, the guest reads it via MMIO). Handled correctly
via `AtomicU32` with `Relaxed` ordering — sufficient because the irqfd write
provides the necessary synchronization (the guest won't read the status
register until after the interrupt is delivered).

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

### Resolution — RESOLVED (differently)

Both TX and RX use `iovec`-based I/O instead of copying through a buffer.
TX builds iovecs pointing directly into guest memory and calls `writev`
(`virtio_net.rs:209-224`). RX does the same with `recv_iovec`
(`virtio_net.rs:158-167,174`).

The per-packet `Vec` allocation is gone entirely. The iovec `Vec` that
collects descriptor pointers is still allocated per packet, but it's a
small Vec (typically 1-3 entries, ~48 bytes) vs. 65KB. For TX this could
be eliminated with a stack array, but it's a minor concern.

Zero-copy is better than reusable buffer.

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

### Resolution — RESOLVED (combined with ioeventfd)

The worker thread now owns the entire FUSE data path. ioeventfd delivers
queue notifications directly to the worker without a vCPU exit (see §7).
The worker (`virtio_fs.rs:143-179`) blocks on `kick_fd.read()`, pops all
pending descriptors, calls `server.handle()` for each, updates the used
ring, and injects an interrupt via irqfd. The vCPU thread is never
involved in steady-state FUSE I/O.

The run loop's virtio-fs MmioWrite handler (`main.rs:127-140`) now only
handles device setup (feature negotiation, queue configuration). The
QueueNotify address (0x050) is intercepted by KVM's ioeventfd and never
reaches userspace.

Clean ownership: queue and server are moved into the worker via
`Option::take()` (`virtio_fs.rs:137-138`). The `FuseServer` trait object
erases the filesystem generic so `VirtioFs` doesn't need to be
parameterized on `F` — simplifies the `System` struct.

Previous intermediate step (channel + eventfd + wait_for_completion) was
replaced entirely. The ioeventfd approach is simpler: one thread, one fd
to block on, no channels, no completion signaling back to the main thread.

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

### Resolution — NO CHANGE NEEDED

Still flat match, still 2 devices, still fine. Correctly deferred.

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

### Resolution — INTENTIONALLY SKIPPED

We're not implementing `VIRTIO_F_EVENT_IDX`. The reasoning:

Notification suppression exists to reduce the cost of notifications. But
with ioeventfd, the per-notification cost drops from "VM exit" (expensive)
to "eventfd write" (cheap). And eventfd coalesces naturally — if the guest
writes to QueueNotify 10 times before the VMM reads the eventfd, the VMM
wakes once and drains all 10. We get batching for free from the kernel.

On the device→guest side, our interrupt pattern is already batched:
- virtio-fs: one IRQ per completion (1:1, can't batch further)
- net TX: drain entire queue, one IRQ after
- net RX: worker drains all packets, one irqfd write after the batch

There's no case where we inject N interrupts for N completions. So
EVENT_IDX would save us some eventfd writes on a pattern that's already
efficient — a layer of abstraction with no meaningful trade-off.

If profiling later shows eventfd write volume is a bottleneck (unlikely),
we can revisit. Until then, fewer feature bits and simpler queue logic.

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

### Resolution — RESOLVED (virtio-fs)

ioeventfd is implemented for virtio-fs (`virtio_fs.rs:132-135`):

```rust
let notify_addr = self.mmio_base + 0x050;
vm.register_ioevent(&self.kick_fd, &IoEventAddress::Mmio(notify_addr), NoDatamatch)
```

The worker thread blocks on `kick_fd.read()` and processes the queue
entirely in the background. Zero vCPU exits on the FUSE data path.

**Net TX** still uses the MMIO exit path (`main.rs:151-153`). This is a
reasonable asymmetry: FUSE requests block on S3 for milliseconds, making
the exit elimination critical. TX is a fast `writev` to a local tap fd —
the VM exit cost is real but small compared to the syscall itself. Adding
ioeventfd for net TX is straightforward if it becomes a bottleneck.

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

### Resolution — NOW RELEVANT, CORRECTLY HANDLED

Now that we have the RX worker thread writing to the used ring and the
vCPU thread reading MMIO registers, memory ordering matters. The current
approach is correct:

- `interrupt_status`: `AtomicU32` with `Relaxed` ordering. Sufficient
  because the irqfd write serializes the interrupt delivery.
- `add_used`: handled by `virtio-queue` crate internally.
- Raw pointer to `AtomicU32` passed to the RX worker
  (`virtio_net.rs:133`): valid because the `VirtioNet` struct outlives
  the worker thread. The `usize` cast is ugly but sound.

The `*const AtomicU32 as usize` pattern in `virtio_net.rs:133` could use
a comment explaining the lifetime guarantee.

---

## Status Summary

| # | Change | Impact | Status |
|---|--------|--------|--------|
| 1 | irqfd for interrupts | Eliminates 1 syscall per interrupt | Done |
| 2 | Network RX rework | Fixes RX throughput and latency | Done (RX worker thread) |
| 3 | Hot-path allocations | Eliminates 64KB alloc/zero per packet | Done (iovec zero-copy) |
| 4 | Async virtiofs | Unblocks vCPU during S3 I/O | Done (worker + ioeventfd) |
| 5 | MMIO dispatch | Readability | Deferred (2 devices, flat match is fine) |
| 6 | Notification suppression | Reduces interrupt/exit rate under load | Skipped (no trade-off) |
| 7 | ioeventfd for queue notify | Eliminates most frequent VM exit | Done (virtio-fs); net TX deferred |
| 8 | Memory ordering | Correctness under threading | Handled |
| 9 | S3 request budget | Reduces HTTP round-trips per FS op | Done (metadata-in-keys, cache, pre-fill) |

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

- **Notification suppression**: ioeventfd + irqfd coalesce in the kernel.
  Our interrupt patterns are already batched. No trade-off to justify the
  complexity.

---

## 9. S3 Request Budget per Filesystem Operation

Moved to [07-s3-filesystem-design.md](07-s3-filesystem-design.md) — covers
the key format (`path\x01<meta>`), caching architecture, pre-fill strategy,
request budget tables, and remaining implementation gaps (read caching,
rename, setattr persistence).

**Summary:** metadata-in-keys eliminates HEAD entirely. Every lookup is
1 LIST (cold) or 0 (cached). Pre-fill with max-keys=12 exploits sort
order to cache neighbors for free. Directory lookups went from 3→1→0
requests. readdir→lookup storm: N×3→0.

---

## Remaining Nitpicks

Small items that don't affect correctness but are worth cleaning up.

### 1. EventPoll allocation per poll

`EventPoll::poll()` (`event_loop.rs:33-42`) returns `Vec<u64>`,
allocating on every call — which happens after every vCPU exit. The
stack-allocated `events` array is fine, but the `Vec` return is
unnecessary. Could use a callback pattern:

```rust
pub fn poll(&self, mut f: impl FnMut(u64)) {
    // ...
    for i in 0..n { f(events[i].u64); }
}
```

Minor, but it's in the hottest loop in the system.

### 2. TX iovec Vec allocation per packet

`process_tx` (`virtio_net.rs:209-218`) collects iovecs into a `Vec` per
descriptor chain. Since typical chains are 1-3 descriptors, a
`[iovec; 4]` stack array with a length counter would eliminate this.
Not critical but easy.

### 3. `transmute` for lifetime erasure in virtio-fs worker

`virtio_fs.rs:163-164` transmutes `Reader` and `Writer` to `'static`:

```rust
let reader: Reader<'static> = unsafe { std::mem::transmute(reader) };
let writer: Writer<'static> = unsafe { std::mem::transmute(writer) };
```

Sound in practice because the `GuestMemoryMmap` is cloned into the worker
thread and owned by the loop that creates the readers/writers — it outlives
every request. But the safety argument isn't documented at the transmute
site. The existing comment (`// SAFETY: guest memory is Arc-wrapped, valid
for VM lifetime`) is close but should say the memory is owned by this
thread and outlives all readers/writers created from it.

### 4. RX worker `yield_now()` after irqfd write — RESOLVED

Removed. Was causing forced context switches after every batch.

### 5. `interrupt_status` pointer cast in both workers

Both `virtio_net.rs:133` and `virtio_fs.rs:141` use the same pattern:

```rust
let interrupt_status = &self.mmio.interrupt_status as *const AtomicU32 as usize;
```

Then in the worker:
```rust
let status = &*(interrupt_status as *const AtomicU32);
```

This is sound because the `VirtioMmioDevice` (and its `AtomicU32` field)
outlives the worker thread in both cases. But passing a raw pointer as a
`usize` bypasses Rust's ability to reason about the reference. A cleaner
pattern would be wrapping the `AtomicU32` in an `Arc` — it's one extra
allocation at device construction (not a hot path) and makes the lifetime
relationship explicit to both the compiler and the reader.

### 6. Net TX still uses MMIO exit path

Net TX queue notifications cause a vCPU exit to dispatch `process_tx()`.
Fine for now — `writev` to a tap fd is fast. Low priority.

---

## Network Throughput Resolution (2026-04-08)

### Journey: 8 KB/s → 2.4 Gbps

The network performance work went through multiple iterations. Summary
of what mattered and what didn't:

**What didn't move the needle:**
- Spin-drain after EAGAIN (50 spin_loop iterations): marginal
- Interrupt coalescing (50μs delay before IRQ): REGRESSED (halved batch
  rate without proportional batch size increase)
- Skip-poll-after-batch: no measurable improvement
- TUNSETSNDBUF (1MB send buffer on macvtap): no measurable improvement
- Zero-copy readv vs host-buffer copy: ~40% improvement (2.63→3.72 MB/s)
- Stack-allocated iovecs vs Vec: included in the readv change

**What fixed bugs:**
- Non-blocking `kick_fd` in virtio-fs worker: was spinning at 100% CPU,
  starving other threads. Fix: create eventfd without `EFD_NONBLOCK`.
- Wasted descriptors on WouldBlock: every drain cycle consumed a guest
  RX buffer with 0 bytes. Fix: read into host buffer first (later: readv
  with go_to_previous_position on EAGAIN).
- `yield_now()` after irqfd write: forced context switch for no reason.
  Fix: removed.
- Worker starting before guest posts buffers: 197MB of "no descriptors"
  log spam, zero throughput. Fix: wait for first pop_descriptor_chain to
  succeed before entering drain loop.

**What actually mattered (40x improvement):**
- `VIRTIO_NET_F_GUEST_TSO4 | VIRTIO_NET_F_GUEST_TSO6`: enables GRO on
  the macvtap path. The kernel aggregates TCP segments into 60KB packets
  before delivering to our readv. One readv call returns 60KB instead of
  1.5KB. Same batch rate, 40x more data per batch.
- `VIRTIO_NET_F_HOST_TSO4 | VIRTIO_NET_F_HOST_TSO6`: enables TSO on the
  TX path. Guest sends large segments, kernel splits them.
- `VIRTIO_NET_F_CSUM | VIRTIO_NET_F_GUEST_CSUM`: guest accepts partial
  checksums, avoiding software checksum computation.

**Key lesson:** the bottleneck was never the drain loop — it was the
packet size. At 1.5KB/packet, per-packet overhead (poll + readv +
add_used + irqfd + KVM interrupt + guest NAPI) created a ~2000 batch/sec
ceiling = ~3 MB/s max. No loop optimization could break through. At
60KB/packet, the same overhead is amortized over 40x more data.

### Final architecture

```
RX: worker thread
  poll(tap_fd, -1)          ← block until data
  loop:
    pop_descriptor_chain    ← get guest buffer (16 iovecs, 60KB)
    readv(tap_fd, iovecs)   ← zero-copy into guest memory
    add_used                ← mark buffer consumed
  irqfd.write(1)            ← one interrupt per batch

TX: inline on vCPU thread
  pop_descriptor_chain      ← get guest packet
  writev(dup_fd, iovecs)    ← zero-copy from guest memory
  add_used                  ← mark buffer consumed
  inject_irq                ← signal completion
```

### Measured throughput

| Test | Throughput | Notes |
|------|-----------|-------|
| Ping (localhost) | 0.5ms | |
| 10MB local HTTP | 182 MB/s (1.5 Gbps) | python http.server |
| 100MB local HTTP | 188 MB/s (1.5 Gbps) | python http.server |
| 1GB local HTTP | 285 MB/s sustained (2.3 Gbps) | peak 304 MB/s |
| iperf3 download (host→guest) | 2.70 Gbps | 0 retransmits, GRO 60KB pkts |
| iperf3 upload (guest→host) | 1.97 Gbps | 0 retransmits |

### Core pinning with TSO/GRO

With 60KB GRO packets, core pinning results reversed vs small packets:

| Configuration | Download | Upload | Notes |
|---------------|----------|--------|-------|
| Unpinned | **2.70 Gbps** | **1.97 Gbps** | Best — cores have real work |
| Pinned to 1 core | 2.63 Gbps | 1.69 Gbps | vCPU + RX worker contend |

With small (1.5KB) packets, single-core won by 42% due to cache locality
on virtqueue descriptors. With large (60KB) GRO packets, the per-packet
overhead is amortized and the vCPU and RX worker have enough work to
benefit from separate cores. The scheduler naturally keeps them close
enough for cache sharing without explicit pinning.

Multi-stream (iperf3 -P 4) was worse: 2.17 Gbps down, 1.64 Gbps up.
Not flow-limited — multiple streams add contention on our single RX
drain loop and TX writev path. Single stream is optimal.

### TX/upload gap

Upload (1.97 Gbps) trails download (2.70 Gbps). We advertise
`HOST_TSO4/6` so the guest should send large TCP segments. The gap may
be due to: (1) macvtap not supporting TSO on the TX path (kernel splits
segments before they reach our writev), (2) the guest driver not using
TSO despite negotiation, or (3) TX runs inline on the vCPU thread (MMIO
exit per QueueNotify) while RX has a dedicated worker thread.

Remaining TX optimizations if needed: ioeventfd for TX QueueNotify
(eliminate MMIO exit per ACK), or a TX worker thread mirroring the RX
pattern.
