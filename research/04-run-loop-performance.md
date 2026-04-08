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

Every S3 request is an HTTP round-trip — TCP, TLS (if not local), request
signing, serialization, response parsing. Even against local MinIO, each
request is on the order of 1-5ms. The guest experiences this as filesystem
latency, so every redundant request is directly felt.

### Audit of FUSE → S3 request counts (current)

| FUSE op | S3 requests | Calls | Notes |
|---------|-------------|-------|-------|
| `lookup` (file hit) | 1 | HEAD | Best case — file exists |
| `lookup` (dir hit, has children) | 3 | HEAD + LIST + HEAD | **Redundant** |
| `lookup` (dir hit, empty dir) | 3 | HEAD + LIST + HEAD | |
| `lookup` (miss) | 2 | HEAD + LIST | File HEAD 404, LIST empty |
| `getattr` | 0 | — | Serves from inode cache |
| `readdir` | 1 | LIST | Single list with delimiter |
| `read` | 1 | GET (Range) | Single range request |
| `open` (read-only) | 0 | — | Just creates handle |
| `open` (write) | 1 | GET | Preloads existing content |
| `write` | 0 | — | Buffered in memory |
| `release` (dirty) | 1 | PUT | Flushes buffer to S3 |
| `release` (clean) | 0 | — | No-op |
| `create` | 1 | PUT | Empty object |
| `mkdir` | 1 | PUT | Empty marker object |
| `unlink` | 1 | DELETE | Single delete |
| `setattr` | 0 | — | Stub, inode cache only |
| `readlink` | 0 | — | Serves from inode cache |

### The lookup problem

`lookup` is the hot path. The kernel calls it for every path component —
`ls /usr/bin/bash` is three lookups (`usr`, `bin`, `bash`). A directory
with 100 entries triggers 100 lookups when you `ls` it. With the current
code, each directory lookup costs 3 S3 requests:

```
s3fs.rs:241  HEAD child_key           → 404 (not a file)
s3fs.rs:269  LIST child_key/          → returns children
s3fs.rs:272  HEAD child_key/          → checks for dir marker
```

The fundamental issue: S3 has no concept of directories. We don't know if
`usr` is a file or a directory because they're different keys (`usr` vs
`usr/`). So we try one, fail, try the other.

### Proposed: single LIST with trailing-slash discrimination

The trailing `/` in S3 keys is our primary signal. We can lean on it
exclusively. A single LIST without the trailing slash, using delimiter,
tells us everything:

```
LIST prefix=child_key&delimiter=/&max-keys=2
```

S3's response separates results into `Contents` (exact key matches) and
`CommonPrefixes` (prefix matches up to the delimiter). So:

- `child_key` appears in Contents → **it's a file** (with size)
- `child_key/` appears in CommonPrefixes → **it's a directory**
- Both appear → file and directory coexist (legal in S3)
- Neither → **ENOENT**

One request. No HEAD. The trailing slash does all the work.

**Why `max-keys=2`?** We only need to distinguish the cases above. The
exact key and the prefix-with-slash are the only two results we care about.
`max-keys=2` keeps the query cheap — S3 doesn't need to enumerate children,
it just checks if the prefix exists.

### What we lose without HEAD

HEAD gives us `x-amz-meta-*` headers (mode, uid, gid, symlink_target).
LIST Contents includes key and size but not user metadata. So for files,
a LIST-only lookup gets us the inode type and size but not POSIX metadata.

Options for metadata:

**A. Defer HEAD to first access.** `lookup` only needs type and size for
the kernel's dentry cache. The POSIX metadata (mode, uid, gid) matters at
`open`, `read`, `getattr` time. We could do the HEAD lazily — mark the
inode as "metadata not yet loaded" and fetch on first access that needs it.

Trade-off: `stat()` calls (which trigger `getattr`) are extremely common.
If `getattr` currently serves from cache and we start requiring a HEAD for
metadata, we'd need to ensure the first getattr after lookup triggers the
fetch and subsequent ones serve from cache.

**B. Embed metadata in key conventions.** Not practical — keys shouldn't
encode metadata.

**C. Accept defaults until write.** Files start with default mode (0755),
uid/gid (0). When `setattr` or `release` writes to S3, it persists the
real metadata. On next lookup, the HEAD-on-access fills it in. For a
coding agent VM where most files are root-owned 0755, the defaults are
often correct anyway.

**D. Store metadata in a sidecar.** A separate S3 object (e.g.,
`.meta/child_key`) holds the metadata. This doubles write requests but
gives us metadata without HEAD. Not worth it.

**Recommendation: A+C combined.** Lookup via LIST, inode gets type + size
+ defaults. First `getattr` or `open` that needs real metadata does a
single HEAD and caches it. For a rootfs where everything is root:root 0755,
the defaults are correct and the HEAD may never be needed.

### What about the readdir→lookup storm?

When the guest runs `ls -l /usr/bin/`, the kernel:
1. Calls `readdir` on `/usr/bin/` → we do 1 LIST, return all entries
2. Calls `lookup` on every entry name → N lookups, currently N×(1-3) requests

With the LIST-based lookup, step 2 is N×1 requests. But `readdir` already
told us every entry's key and size. If we populated the inode cache during
`readdir`, the subsequent lookups would all hit cache:

```rust
fn readdir(...) {
    let result = self.client.list(&prefix, Some("/"))?;
    for obj in &result.objects {
        // Cache this entry — lookup will find it later
        inodes.get_or_create(&obj.key, is_dir, obj.size);
    }
    ...
}
```

We already do this (`s3fs.rs:336`). But `lookup` doesn't check the cache
before hitting S3. Adding the cache check means the readdir→lookup storm
goes from N requests to 0, because readdir already warmed the cache.

### The cache consistency question

Serving from cache means we might return stale data. Key scenarios:

1. **We're the only writer** (coding agent VM): cache is always correct.
   Files only change when the guest writes them, which updates our cache.
   Safe to cache aggressively.

2. **External writer** (someone pushes to S3 outside the VM): our cache
   is stale. The guest sees old files until cache expires or VM restarts.

For our use case (1), aggressive caching is safe. The `entry_timeout` TTL
(currently 1 second) controls how often the kernel re-lookups. We could
increase this significantly, but even at 1 second, if our inode cache
serves the re-lookup without hitting S3, the TTL just controls kernel-side
dentry expiry — not S3 traffic.

**One caveat**: if we ever support a workflow where the rootfs S3 bucket
is modified externally while the VM is running, we'd need either:
- A cache invalidation mechanism (e.g., S3 event notifications)
- A way to force a cache flush (e.g., guest-initiated `sync` or remount)
- Accept staleness within a bounded window

For now, single-writer assumption holds. Document it and move on.

### Proposed request budget (after all changes)

| FUSE op | Cold (first time) | Warm (cached) | Notes |
|---------|--------------------|---------------|-------|
| `lookup` | 1 LIST | 0 | LIST discriminates file vs dir |
| `lookup` (after readdir) | 0 | 0 | readdir warms cache |
| `getattr` | 0-1 | 0 | HEAD on first access if metadata needed |
| `readdir` | 1 LIST | 1 LIST | Always live (dir contents change) |
| `read` | 1 GET | 1 GET | Always live (data) |
| `open` (read) | 0 | 0 | Just creates handle |
| `open` (write, no trunc) | 1 GET | 1 GET | Preload for read-modify-write |
| `open` (write, O_TRUNC) | 0 | 0 | Skip preload |
| `release` (dirty) | 1 PUT | 1 PUT | Always writes |
| `create` | 1 PUT | 1 PUT | Always writes |
| `mkdir` | 1 PUT | 1 PUT | Always writes |
| `unlink` | 1 DELETE | 1 DELETE | Always writes |

The big wins: directory lookups go from 3 → 1 (cold) or 0 (warm).
Repeat lookups of any kind go from 1-3 → 0. The readdir→lookup storm
goes from N×3 → 0.

### Implementation approach

The LIST-based lookup needs a lightweight list method that doesn't
paginate. The current `S3Client::list()` always uses `max-keys=1000`
and follows continuation tokens. A `list_probe` or similar:

```rust
pub fn list_probe(&self, prefix: &str) -> Result<ListResult, S3Error> {
    // Single LIST, max-keys=2, delimiter=/, no pagination
}
```

### Open questions (LIST-only approach)

1. **LIST vs HEAD latency on MinIO.** The whole strategy assumes LIST
   isn't dramatically slower than HEAD for small result sets. Needs a
   benchmark before committing.

2. **Symlinks** — LIST can't identify them without metadata in keys.
   This is what drives the key format redesign below.

---

### 9b. Metadata-in-Keys: Full Design

The LIST-only lookup from §9 can't identify symlinks. The kernel needs
`S_IFLNK` in `st_mode` at lookup time — if we return `S_IFREG`, it
never calls `readlink`. A typical rootfs has hundreds of symlinks.
We can't defer this; we need the type at lookup time.

The fix: embed metadata directly in S3 keys where LIST can see it.

#### Key format

```
<path>\x01<mode_hex><uid_hex><gid_hex>
```

- `\x01` (SOH, 0x01) separates the filesystem path from metadata
- All fields are fixed-width lowercase hex, no delimiters between them
- Total metadata: 20 bytes (4 mode + 8 uid + 8 gid), always

| Field | Size | Bits | Range |
|-------|------|------|-------|
| mode | 4 hex chars | 16-bit | Full st_mode (type + perms) |
| uid | 8 hex chars | 32-bit | 0 – 4294967295 |
| gid | 8 hex chars | 32-bit | 0 – 4294967295 |

The mode encodes the file type in its high bits per POSIX:
- `0x81ed` = `0o100755` = S_IFREG | 0755 (regular file)
- `0xa1ff` = `0o120777` = S_IFLNK | 0777 (symlink)
- `0x41ed` = `0o040755` = S_IFDIR | 0755 (directory)

#### No trailing slash on directories

Directories do NOT use a trailing `/` in their key. The mode field
already encodes the type. A directory named `usr/bin` is stored as:

```
usr/bin\x0141ed00000000000000000000
```

Not `usr/bin/\x01...`. This is critical — it means files and directories
share the same key pattern (`path\x01<meta>`), which enables single-LIST
lookup regardless of type.

#### Example keyspace

```
etc\x0141ed00000000000000000000          ← directory
etc/hosts\x0181a000000000000000000000    ← regular file
etc/motd\x0181a400000000000000000000     ← regular file
etc/shadow\x0181a0000000000000002a       ← root:shadow
usr\x0141ed00000000000000000000          ← directory
usr/bin\x0141ed00000000000000000000      ← directory
usr/bin/bash\x0181ed00000000000000000000 ← regular file
usr/bin/ls\x0181ed00000000000000000000   ← regular file
usr/lib\x0141ed00000000000000000000      ← directory
usr/lib/libfoo.so\x01a1ff00000000000000000000  ← symlink
```

#### Symlink targets

Symlink targets are stored as the **object body** (the object's content),
not in the key or in headers. This mirrors real filesystems — on ext4, a
symlink's data content is the target path.

- Keeps the key format uniform: always `path\x01<20 hex chars>`
- No variable-length data in keys, no secondary delimiters to parse
- No 1024-byte key length concerns from long targets
- `readlink` does a GET to fetch the target (one request, then cached)

#### Why \x01

`\x01` (SOH) is the separator between path and metadata because:
- Valid in S3 keys
- Technically valid in Linux filenames but never occurs in practice
  (no tool, editor, or build system generates it in file names)
- Lowest non-null byte value, which gives clean sort behavior

#### How lookup works: one LIST, always

Since both files and directories use `path\x01<meta>`, every lookup
is the same query:

```
LIST prefix=usr/bin/bash\x01&max-keys=12
```

The prefix `usr/bin/bash\x01` precisely matches only keys for that
exact path — no false matches from `usr/bin/bashrc` (which would be
`usr/bin/bashrc\x01...`, different bytes after `bash`).

The mode field in the result tells us the type: regular file, symlink,
or directory. One request, one code path, no type guessing.

**Why `max-keys=12` instead of 1?** The round-trip latency dominates
the cost of a LIST — returning 12 results vs 1 adds negligible overhead
(a few hundred extra bytes of XML on the wire). By asking for 12, we
get the entry we're looking for plus up to 11 subsequent entries from
the sorted keyspace. Those extras are free passengers that pre-fill
the cache.

#### Opportunistic pre-fill

After `usr/bin/bash\x01<meta>` in the sorted keyspace, the next keys
are its lexicographic neighbors:

```
usr/bin/bash\x0181ed...   ← the entry we asked for
usr/bin/cat\x0181ed...    ← free
usr/bin/chmod\x0181ed...  ← free
usr/bin/cp\x0181ed...     ← free
usr/bin/dash\x0181ed...   ← free
...up to 11 more
```

These are siblings in the same directory — exactly the entries the
kernel is likely to ask about next (think `$PATH` resolution, `ls`,
tab completion). We parse all of them and populate our inode cache.

For a directory lookup like `usr\x01`:

```
usr\x0141ed...             ← the entry: it's a directory
usr/bin\x0141ed...         ← free: child directory
usr/bin/bash\x0181ed...    ← free: grandchild file
usr/bin/cat\x0181ed...     ← free: grandchild file
...
```

We get the directory itself plus the first few children (and possibly
grandchildren), warming the cache for the next several path lookups
the kernel will make as it walks deeper.

The depth of pre-fill depends on the fan-out. A narrow directory like
`/usr` (8-10 children) gets entirely pre-filled. A wide directory
like `/usr/bin` (hundreds of entries) gets the first ~11 entries,
which still helps because it covers the alphabetically early commands
the kernel tends to probe first.

#### Why this works with sort order

S3's lexicographic sort on our key format produces a natural ordering:

```
\x01 = 0x01  (metadata separator)
/    = 0x2F  (path separator)
0-9  = 0x30-0x39
a-z  = 0x61-0x7A
```

`\x01` sorts before `/`, which sorts before all alphanumeric characters.
This means for any path prefix like `usr/bin`:

```
usr/bin\x01<meta>           ← the directory entry itself
usr/bin/bash\x01<meta>      ← child (/ comes after \x01)
usr/bin/cat\x01<meta>       ← child
usr/bin/subdir\x01<meta>    ← child directory
usr/bin/subdir/tool\x01...  ← grandchild
```

The directory entry sorts immediately before its children. A LIST
starting at `usr/bin\x01` naturally flows into the directory's
contents. This is a consequence of `\x01 < /` — the metadata suffix
on the directory entry sorts before the path separator that begins
child paths.

If a sibling file exists (rare but possible):

```
usr/bi\x01<meta>            ← hypothetical file "usr/bi"
usr/bin\x01<meta>           ← directory "usr/bin"
usr/bin/bash\x01<meta>      ← child of usr/bin
usr/binder\x01<meta>        ← hypothetical file "usr/binder"
```

The targeted prefix `usr/bin\x01` skips straight to our entry and its
children. `usr/bi` and `usr/binder` don't match because the bytes
after `bin` differ. No ambiguity.

#### Directory listing (readdir)

```
LIST prefix=usr/bin/&delimiter=/
```

S3 returns all keys starting with `usr/bin/`. The `/` delimiter groups
grandchildren into CommonPrefixes. Direct children appear in Contents
with full metadata:

```
Contents:
  usr/bin/bash\x0181ed00000000000000000000
  usr/bin/ls\x0181ed00000000000000000000
  usr/bin/python3\x01a1ff00000000000000000000
  usr/bin/subdir\x0141ed00000000000000000000
CommonPrefixes:
  usr/bin/subdir/   ← because subdir has children under it
```

Subdirectories like `subdir` appear in both Contents (with metadata,
because their key `usr/bin/subdir\x01<meta>` starts with the prefix)
and CommonPrefixes (because keys like `usr/bin/subdir/tool\x01...`
exist). We read metadata from Contents and ignore the CommonPrefixes
duplicate.

`readdir` parses every result and populates the inode cache. Subsequent
`lookup` calls for any child of this directory are served from cache
with zero S3 traffic.

#### FUSE and the serial lookup problem

The kernel's FUSE interface resolves paths one component at a time.
To access `/usr/bin/bash`, it calls `lookup("usr")`, waits for the
result, then `lookup("bin")`, waits, then `lookup("bash")`. Each
call is serial — the kernel needs the parent inode before it can ask
about the child. We can't change this; it's how Linux VFS works.

But with `max-keys=12`, each lookup pre-fills the next few steps:

1. `lookup("usr")` → `LIST prefix=usr\x01&max-keys=12`
   Returns: `usr` (dir), `usr/bin` (dir), `usr/bin/bash` (file), ...
   Cache now has: usr, usr/bin, usr/bin/bash, usr/bin/cat, ...

2. `lookup("bin")` → **cache hit** (pre-filled in step 1). 0 requests.

3. `lookup("bash")` → **cache hit** (pre-filled in step 1). 0 requests.

A 3-component path traversal costs 1 LIST instead of 3. The pre-fill
naturally covers the serial lookup pattern because the sorted keyspace
mirrors the directory hierarchy.

This works especially well for the shallow, narrow directories at the
top of the tree (`/usr`, `/etc`, `/var`) which typically have fewer than
12 direct children. The entire first level or two gets cached on the
first lookup.

For deeper or wider directories, pre-fill covers the first ~11 entries
alphabetically. The rest require their own lookup when accessed, which
is the expected cost — we're not trying to cache the entire filesystem,
just exploit locality for free.

#### Cache architecture

Our inode table is the primary cache, not the kernel's dentry cache.

**Kernel TTL (`entry_timeout`)**: kept short. The kernel re-lookups
frequently, but our cache serves instantly with no S3 traffic. This
gives us control over cache lifetime rather than relying on the
kernel's timer.

**Our cache is authoritative for local writes.** When the guest creates,
modifies, or deletes a file, we update the inode table directly. No
staleness between write and subsequent read.

**Host-side writes invalidate the cache directly.** The VMM owns both
the S3 client and the FUSE server in the same process. When the host
pushes a file to S3 (e.g., injecting a config file into the running
VM), it can update the inode table in the same process. The guest sees
the change on the next lookup — no TTL expiry needed, no staleness
window. This is a unique advantage of owning the full stack.

**Negative caching.** Lookup misses (ENOENT) are cached too. The kernel
probes many non-existent paths (library search paths, locale variants,
optional config files). Without negative caching, each probe is a
wasted LIST. With it, the first miss is cached and subsequent probes
are free.

**Future: kernel FUSE patching.** We control the guest kernel. A custom
FUSE extension that passes the full remaining path (or batches multiple
components) would let us collapse the serial lookup chain into a single
S3 call. Not needed today, but architecturally we should keep the FUSE
layer clean enough that swapping in a patched protocol is straightforward.

#### Data flow: lookup always precedes data access

A critical property of the VFS interface: the kernel never asks us to
`read`, `open`, or `readlink` a file it hasn't already resolved via
`lookup`. Every data access is preceded by a lookup that established the
inode. This means:

1. `lookup` discovers the S3 key (via LIST) and caches it in the inode
   table — including the full key with metadata suffix.
2. `open` / `read` / `readlink` use the **already-known key** from cache
   to issue a direct GET. No additional LIST is needed.
3. `getattr` / `stat` serve entirely from cache — the metadata was
   parsed from the key during lookup. Zero S3 traffic.

The only FUSE operations that hit S3 without a prior lookup are writes
(`create`, `mkdir`) — these construct new keys (we choose the metadata)
and PUT directly. But writes generate keys; they don't need to discover
them.

This means **LIST is purely a discovery mechanism** and **GET is purely
a data fetch**. They never overlap. Every file interaction follows:

```
LIST (discovery, once) → cache → GET (data, when needed)
```

#### S3 request map by FUSE operation

Every FUSE call and exactly which S3 calls it makes:

| FUSE op | S3 calls | What hits S3 | What's cached |
|---------|----------|--------------|---------------|
| `lookup` (cold) | 1 LIST | key discovery | type, mode, uid, gid, full key |
| `lookup` (warm) | 0 | — | served from inode table |
| `lookup` (pre-filled) | 0 | — | populated by prior lookup's max-keys=12 |
| `lookup` (miss) | 1 LIST | confirms non-existence | negative result cached |
| `getattr` | 0 | — | metadata parsed from key at lookup time |
| `readdir` | 1 LIST | enumerate children | all children + metadata cached |
| `open` (read) | 0 | — | just allocates a handle |
| `open` (write, no trunc) | 1 GET | preload content for read-modify-write | — |
| `open` (write, O_TRUNC) | 0 | — | skip preload, start empty |
| `read` | 1 GET | fetch data using cached key | — |
| `readlink` (cold) | 1 GET | symlink target from object body | target cached |
| `readlink` (warm) | 0 | — | target served from cache |
| `write` | 0 | — | buffered in memory |
| `release` (dirty) | 1 PUT | flush buffer to S3 | updates inode table |
| `release` (clean) | 0 | — | — |
| `create` | 1 PUT | new key (we choose metadata) | updates inode table |
| `mkdir` | 1 PUT | new key (we choose metadata) | updates inode table |
| `unlink` | 1 DELETE | remove key | evicts from inode table |
| `setattr` | 0 | — | updates inode table (persisted on next write) |

**HEAD is never used.** All metadata comes from key parsing at LIST time.

**GET is only used for data**: file content (`read`, `open` preload) and
symlink targets (`readlink`). Never for metadata discovery.

**LIST is only used for discovery**: lookup and readdir. Never for data.

**PUT/DELETE are only used for writes.** Always exactly 1 request.

#### Summary: request budget

| FUSE op | Current | Proposed (cold) | Proposed (warm) |
|---------|---------|-----------------|-----------------|
| lookup (any type) | 1-3 | 1 LIST (max-keys=12) | 0 (cache) |
| lookup (pre-filled by prior) | 1-3 | 0 | 0 (cache) |
| lookup (miss) | 2 | 1 LIST | 0 (neg. cache) |
| readdir | 1 LIST | 1 LIST | 1 LIST |
| getattr | 0 | 0 | 0 |
| read | 1 GET | 1 GET | 1 GET |
| readlink | 0 | 1 GET (first) | 0 (cache) |
| open (write, no trunc) | 1 GET | 1 GET | 1 GET |
| open (write, O_TRUNC) | 1 GET | 0 | 0 |
| release (dirty) | 1 PUT | 1 PUT | 1 PUT |
| create | 1 PUT | 1 PUT | 1 PUT |
| mkdir | 1 PUT | 1 PUT | 1 PUT |
| unlink | 1 DELETE | 1 DELETE | 1 DELETE |

Key wins:
- **Every lookup: 1 LIST, always** — no type guessing, no HEAD
- **Directory lookup: 3 → 1**
- **Pre-fill: next ~11 entries cached for free** per lookup
- **Path traversal `/usr/bin/bash`: 1 LIST** instead of 3 (pre-fill)
- **Symlink identification: free from key metadata**
- **readdir→lookup storm: N×3 → 0** (readdir warms entire directory)
- **Host-side writes: instant visibility** via direct cache update
- **Data access (read/readlink) never needs LIST** — key already known

#### Rootfs packing

The upload tool encodes keys in the new format:

1. Walk the source filesystem
2. For each entry, construct: `path\x01<mode_hex><uid_hex><gid_hex>`
   - No trailing `/` on directories
3. Upload with content:
   - Regular files: file content as body
   - Symlinks: target path as body
   - Directories: empty body (0 bytes)

The packer replaces `x-amz-meta-*` headers entirely — all metadata
is in the key. PUT requests become simpler (no custom headers).

`readdir` strips the `\x01...` suffix when returning names to the
kernel. All other operations parse the fixed 20-byte hex suffix.

#### Open questions

1. **MinIO key handling.** Do `\x01` bytes in keys work correctly with
   MinIO's S3 API, web console, and `mc` CLI? Valid per S3 spec but
   might confuse tooling. Need to test.

2. **Key readability.** Keys become opaque: `usr/bin/bash\x0181ed...`
   instead of clean paths. Acceptable for machine-managed rootfs. For
   debugging, the packer could include a `list-decoded` command that
   prints paths with human-readable metadata.

3. **LIST vs HEAD latency on MinIO.** If `max-keys=12` LIST is
   significantly slower than HEAD, we'd want to benchmark. Expectation:
   the round-trip dominates and payload size is negligible.

4. **max-keys tuning.** 12 is a starting point. Could be tuned based
   on typical directory fan-out. Too low wastes the pre-fill opportunity.
   Too high wastes bandwidth on entries that are never accessed. The
   sweet spot is roughly the width of a typical shallow system directory
   (`/usr` has ~8-10 entries, `/etc` has ~20-30). 12 covers the common
   narrow directories fully and gives partial coverage of wider ones.

### Resolution — IMPLEMENTED

The metadata-in-keys design is fully implemented across three new modules:

**`keys.rs`**: `encode`/`decode`/`list_prefix` for the `path\x01<20 hex>`
format. Includes test for sort-order property (directory key < child key).

**`cache.rs`**: Bidirectional `InodeCache` (path↔inode). Populated by
LIST results via `insert_from_key()`. Preserves inode numbers and symlink
targets on update. Root inode pre-initialized.

**`s3.rs`**: `list_probe()` — single-page LIST with configurable max-keys,
no pagination. URL-decodes keys from S3's XML response (handles the
percent-encoded `\x01` byte correctly).

**`s3fs.rs`**: Full rewrite of the FUSE implementation:

- `lookup`: checks cache first, falls back to `probe_and_cache()` which
  does `list_probe(prefix=path\x01, max_keys=12)`. All results (target +
  neighbors) are parsed and cached. One LIST, one code path.
- `readdir`: LISTs children, decodes every key, populates cache. Filters
  to direct children. Subsequent lookups are cache hits.
- `readlink`: cache check → GET on miss → caches target. Target is object
  body.
- `read`: reconstructs S3 key from cached path + meta. Direct GET, no
  LIST.
- `create`/`mkdir`: construct keys via `keys::encode`, PUT, update cache.
- `unlink`: reconstruct key from cache, DELETE, evict from cache.
- `getattr`: purely from cache, zero S3 calls.

**HEAD is eliminated.** Not called anywhere. All metadata from keys.

**Confirmed S3 hit patterns match the design:**

| FUSE op | S3 calls | Verified |
|---------|----------|----------|
| lookup (cold) | 1 LIST | `probe_and_cache` with max-keys=12 |
| lookup (warm) | 0 | cache check at top of `lookup` |
| readdir | 1 LIST | populates cache for all children |
| getattr | 0 | served from cache |
| read | 1 GET | key from cache, no LIST |
| readlink (cold) | 1 GET | target from object body |
| readlink (warm) | 0 | `symlink_target` field in cache |
| open (read) | 0 | handle allocation only |
| open (write) | 1 GET | preload existing content |
| release (dirty) | 1 PUT | flush buffer |
| create | 1 PUT | new key, update cache |
| mkdir | 1 PUT | new key, update cache |
| unlink | 1 DELETE | evict from cache |

### Remaining items from §9

**`readdir` without delimiter.** The current implementation does
`list(prefix, None)` — no delimiter — which returns the entire subtree,
then filters to direct children in Rust (`child_name.contains('/')`).
This works correctly but wastes transfer on deep trees. Adding
`delimiter='/'` would let S3 do the filtering. Direct children still
appear in Contents with full encoded keys. Subdirectory grouping in
CommonPrefixes is harmless (we already have the dir entry in Contents
since directories don't use trailing slashes).

**`open` O_TRUNC optimization.** Not yet implemented. `open` for write
still unconditionally GETs existing content (`s3fs.rs:344-357`). The
fix is a one-liner:

```rust
let needs_preload = writing && (flags & libc::O_TRUNC as u32) == 0;
```

**Negative caching.** Not yet implemented. Lookup misses return ENOENT
but aren't recorded in the cache. The kernel re-lookups on TTL expiry
and we re-LIST. Adding a sentinel entry for misses would eliminate this.

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

### 4. RX worker `yield_now()` after irqfd write

`virtio_net.rs:197`: `std::thread::yield_now()` after delivering packets.
`yield_now` on Linux calls `sched_yield()` which doesn't guarantee the
vCPU thread runs next — it yields to any runnable thread. Since the
interrupt is delivered via irqfd (in-kernel), the guest sees it regardless
of whether the worker yields. This call can be removed.

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

Net TX queue notifications cause a vCPU exit to dispatch `process_tx()`
(`main.rs:151-153`). This is fine for now — `writev` to the tap fd is
fast and the exit overhead is small relative to the syscall. If net TX
throughput becomes a concern, adding ioeventfd + a TX worker (mirroring
the virtio-fs pattern) would eliminate the exit. Low priority.

---

## 10. S3Fs Implementation Gaps

Code review of `s3-rootfs/src/s3fs.rs` and `s3-rootfs/src/s3.rs` against
the §9 design. The metadata-in-keys, probe-and-cache, and write-buffering
strategies are implemented correctly. These are the remaining gaps.

### 10a. No read caching — every `read()` is an HTTP round-trip

`read()` (`s3fs.rs:312-338`) calls `get_range()` on every invocation.
The guest kernel issues FUSE reads in chunks (typically 128KB). Sequential
`cat bigfile.txt` on a 1MB file is ~8 HTTP GETs, each paying the full
S3 latency floor (~5-20ms against local MinIO, much more against remote
S3). That's 40-160ms for 1MB of sequential read.

This is the most impactful data-path gap. Options, in order of complexity:

**A. Cache last GET response.** If the guest reads the same file
sequentially (common: `read()`, `read()`, `read()` at increasing
offsets), cache the previous GET response and serve from it if the
requested range overlaps. One `Vec<u8>` per open file handle, evicted
on `release`. Covers the dominant access pattern with ~20 lines.

**B. Read-ahead with larger GET ranges.** When a 128KB read arrives,
fetch 1MB from S3 and cache the excess. Subsequent reads within the
same 1MB window are served from cache. Trade-off: wastes bandwidth if
the guest seeks randomly, but sequential access is the common case for
a coding agent reading source files.

**C. Per-file block cache.** Cache fixed-size blocks (e.g., 256KB) keyed
by inode + offset. LRU eviction with a configurable memory budget. This
handles both sequential and random access patterns. More complex but
would also benefit repeated reads of the same file regions (e.g., shared
library headers, ELF segments).

**Recommendation:** Start with **B** — fetch 512KB-1MB on first read,
cache per handle, serve subsequent reads from buffer. This is a natural
extension of the write buffering pattern already in `HandleEntry.data`.
The same `data: Option<Vec<u8>>` field could serve double duty: populated
lazily on first read, used for write buffering if the handle is writable.

### 10b. No cache invalidation or TTL

The `InodeCache` has no eviction. Once an inode is inserted, it lives
forever with whatever size and metadata it had at insertion time. If
something modifies the S3 bucket externally (e.g., pushing a new rootfs
snapshot while the VM is running), the guest sees stale data indefinitely.

The §9 design noted this is acceptable under the single-writer assumption
(the VM is the only writer). That still holds. But two scenarios need
consideration:

1. **Host-side file injection.** The VMM may push files into S3 for the
   guest to pick up (config files, agent instructions). Since the VMM
   owns both the S3 client and the FUSE server, it can update the inode
   cache directly — no TTL needed. This path doesn't exist yet but is
   architecturally straightforward.

2. **Long-running VMs.** If the VM runs for hours/days, memory grows
   monotonically as new inodes are cached. A coding agent that `find`s
   the entire filesystem would cache every inode. An LRU or size-bounded
   cache would cap this, but it's not urgent for 256MB guest RAM VMs
   where the inode table is small relative to guest memory.

### 10c. `setattr` doesn't persist

`setattr` (`s3fs.rs:543-555`) returns success but only updates the cache.
The metadata change is never written to S3. This means `chmod`, `chown`,
`utimes` appear to succeed but are lost on next cold lookup.

For a coding agent VM where everything is root:root 0755, this may not
matter in practice. But package managers (`apt`, `pip`) and build systems
(`make`) do check and set permissions. If `setattr` silently lies, subtle
breakage accumulates.

**Fix:** On `setattr`, if mode/uid/gid changed, we need to rename the S3
key (since metadata is encoded in the key). This is a copy + delete in
S3 — no atomic rename. The operation sequence:

```
1. PUT new key (path\x01<new_meta>) with existing object body
2. DELETE old key (path\x01<old_meta>)
3. Update cache
```

This is expensive (GET + PUT + DELETE = 3 requests) so it should be
deferred or batched. One option: mark the inode as "meta-dirty" and
persist on `release` or `fsync`, similar to how data writes are deferred.

### 10d. Write buffer is unbounded

`write()` (`s3fs.rs:403-444`) grows `HandleEntry.data` without limit.
The design doc mentions a 100MB file limit but it's not enforced. A guest
writing a large file (or a malicious process) will grow the VMM's heap
until OOM.

**Fix:** Check `data.len()` against a configured max (e.g., 100MB) and
return `EFBIG` if exceeded. One-liner in `write()`.

### 10e. No `rename`

`rename` is not implemented. This is the most impactful missing FUSE
operation for a coding agent workload:

- `mv file.txt new.txt` — fails
- `sed -i 's/foo/bar/' file.txt` — fails (writes tempfile, renames)
- Every text editor (vim, nano, emacs) — atomic save via write-tmp + rename
- `git checkout` — renames files during branch switch
- `pip install` — renames during package installation

S3 has no rename primitive. Implementation requires copy + delete:

```
1. GET old key (or skip if we have cached data)
2. PUT new key (path\x01<meta>) with same body
3. DELETE old key
4. Update cache: remove old path, insert new path with same inode
```

For directories, this requires recursively renaming all children (LIST
prefix, copy+delete each). This is expensive but necessary for
correctness.

**Priority:** High. Without rename, the filesystem is effectively
read-only for most real-world tools.

### 10f. `readdir` loads entire directory before yielding

`readdir` (`s3fs.rs:206-273`) calls `self.client.list()` which paginates
internally and returns all results, then iterates to yield entries. For
a directory with 10K files, this fetches and parses all 10K keys before
returning the first `DirEntry` to the FUSE layer.

The FUSE protocol supports incremental readdir via the `offset` parameter
and the `add_entry` return value (returns 0 when the buffer is full). The
current code respects the offset for skipping, but all data is already
in memory.

For typical system directories (10-100 entries) this is fine. For large
directories (`node_modules/`, `.git/objects/`) it could be a latency
spike on first `ls`. Not urgent but worth noting.

### Summary: S3Fs implementation vs design

| Feature | Design (§9) | Implementation | Gap |
|---------|-------------|----------------|-----|
| Metadata-in-keys | Full | Full | None |
| Probe-and-cache (LIST) | max-keys=12 | max-keys=12 | None |
| Negative caching | Designed | Implemented | None |
| readdir→lookup pre-fill | Designed | Implemented | None |
| Read caching | Not designed | Not implemented | **High impact** |
| Cache eviction/TTL | Deferred | Not implemented | Low (single-writer) |
| setattr persistence | Not designed | Not implemented | Medium |
| Write buffer bounds | 100MB limit noted | Unbounded | Low risk, easy fix |
| rename | Not designed | Not implemented | **High impact** |
| readdir streaming | Not designed | Loads all first | Low |
