# S3-rootfs Refactor Plan — Code Quality Pass

Structural cleanup of `s3-rootfs/` with an eye toward the future
orchestrator split (see `research/10-fs-daemon.md`) but keeping the
current in-process model. Also introduces `launcher.rs` in microvm.

**Hard constraints:**
- No perf regression. The virtio-fs worker loop is heavily optimized
  and those optimizations must survive bit-for-bit (see §3).
- Microvm layout is NOT being reorganized. The only changes to microvm
  are (a) adding `launcher.rs` and (b) modifying `virtio_fs.rs` to
  delegate the worker loop to `s3_rootfs::worker::serve`.
- Every step gated by benchmarks. Criterion baseline set before any
  file moves; each step must hold the numbers flat.

---

## 1. Target layout

```
s3-rootfs/src/
  lib.rs             re-exports + log/debug macros (unchanged)
  config.rs          FsConfig + BlobCacheConfig (plain structs, Default impl)
  keys.rs            (unchanged)
  s3.rs              S3Client + S3Config + error types (split from current s3.rs if grows)
  storage.rs         ObjectBody enum + hardlink/golden body helpers
  inode_cache.rs     current cache.rs, renamed (path → metadata)
  blob_cache.rs      hash → bytes cache (future; scaffold only for now)
  fs.rs              S3Fs + HandleTable + impl FileSystem (ALL ops, one file)
  worker.rs          FsSession + serve() — the virtio-fs worker loop
```

```
microvm/src/
  launcher.rs        NEW — LaunchConfig + prepare_filesystem + launch_vm + run
  virtio_fs.rs       MODIFIED — start_worker delegates to s3_rootfs::worker::serve
  virtio_mmio.rs     MODIFIED — interrupt_status becomes Arc<AtomicU32>
  main.rs            MODIFIED — thin arg parsing + launcher::run()
  everything else    UNCHANGED (cpu.rs, kernel.rs, vm.rs, virtio_net.rs, etc.)
```

---

## 2. Design decisions (already discussed)

**FUSE ops in one file (`fs.rs`).** Splitting by read/write/mutate is
artificial. The real question is whether individual ops can be short
and clear — that comes from good helpers (storage.rs body parsing,
inode_cache lookup, key encoding), not splitting files.

**No FUSE protocol layer.** Don't recreate FUSE with a custom RPC.
The `fuse-backend-rs::FileSystem` trait IS the interface. Reader/Writer,
Context, flags, offsets, lock_owner all exist for reasons we'd rediscover
the hard way.

**Inode cache and blob cache separate files.** Different keys (path vs
hash), different eviction (TTL+neg-cache vs LFU/LRU), different contracts.
Don't force a shared trait.

**Config struct, not builder.** `FsConfig { ..Default::default() }`
reads better than `.builder().build()`. Matches Rust stdlib conventions.

**`storage.rs` contents:**
```rust
pub enum ObjectBody {
    Direct(Vec<u8>),
    Hardlink(String),     // "hardlink:<uuid>"
    GoldenRef(String),    // "ref:<sha>" — future
    Symlink(String),      // body IS target path
}
pub fn parse_body(bytes: Vec<u8>, file_type: u32) -> ObjectBody;
pub fn encode_hardlink_ref(uuid: &str) -> Vec<u8>;
pub fn encode_golden_ref(sha: &str) -> Vec<u8>;  // stub
pub fn new_hardlink_uuid() -> String;
pub fn hardlink_key(uuid: &str, meta: KeyMeta) -> String;
pub fn golden_key(sha: &str) -> String;          // stub
```

All body-format decisions live here. `fs.rs` calls `parse_body()` and
matches the enum. When golden refs ship, `GoldenRef` gets a resolver,
`fs.rs` doesn't change.

---

## 3. Run-loop optimizations — MUST BE PRESERVED

From current `microvm/src/virtio_fs.rs::start_worker`. Each of these
was benchmarked and is load-bearing:

1. **ioeventfd at `mmio_base + 0x050`** — QueueNotify writes skip vCPU
   exit entirely. Registered once via
   `vm.register_ioevent(&kick_fd, IoEventAddress::Mmio(addr), NoDatamatch)`.
2. **Batched descriptor drain** —
   `while let Some(desc_chain) = queue.pop_descriptor_chain(&memory)`
   loops until queue is empty per kick. One kick → many requests processed.
3. **Single irqfd write per batch** — `if processed { irqfd.write(1) }`.
   NOT per-request. Coalesces interrupt for the whole drained batch.
4. **`interrupt_status.fetch_or(0x01, Relaxed)` before irqfd.write** —
   cheapest memory ordering that still works.
5. **Named worker thread** (`"vm-fuse"`) — observability in top/htop/perf.
6. **`unsafe transmute` to `'static` Reader/Writer** — required by
   `fuse-backend-rs::Server::handle_message` API. Safe because memory
   is captured by the thread's closure.
7. **`*const AtomicU32 as usize`** — lifetime workaround. Being replaced
   with `Arc<AtomicU32>` in this refactor (zero-cost — AtomicU32 inside
   Arc is still a u32). This is the ONE visible cleanup.

Items 1–5 must be bitwise preserved. Item 6 stays (API constraint).
Item 7 is replaced with Arc — verify with benchmarks that this is flat.

---

## 4. Worker API

```rust
// s3-rootfs/src/worker.rs

pub struct FsSession {
    pub fs: Arc<crate::S3Fs>,
    pub memory: Arc<GuestMemoryMmap>,
    pub queue: Queue,
    pub kick_fd: EventFd,
    pub irqfd: EventFd,
    pub interrupt_status: Arc<AtomicU32>,
    pub thread_name: String,   // "vm-fuse" today, "vm-fuse-{vm_id}" future
}

pub fn serve(session: FsSession) -> JoinHandle<()>;
```

**Loop body is copy-pasted from current virtio_fs.rs.** No logic changes.
Only the surrounding ceremony (how `fs`/`memory`/`interrupt_status` are
held) changes from raw to Arc.

Inside `serve()`:
```rust
let server = Server::new(/* fs by value — clone once from Arc */);
loop {
    kick_fd.read().ok();
    let mut processed = false;
    while let Some(desc_chain) = queue.pop_descriptor_chain(&*memory) {
        // Reader/Writer construction + unsafe transmute (UNCHANGED)
        // server.handle_message (UNCHANGED)
        // queue.add_used (UNCHANGED)
        processed = true;
    }
    if processed {
        interrupt_status.fetch_or(0x01, Ordering::Relaxed);
        irqfd.write(1).ok();
    }
}
```

**Note on `Server::new`:** It takes the filesystem by value. With
`Arc<S3Fs>`, clone-out-of-Arc once at worker start. This runs once per
worker lifetime, NOT per request. Zero hot-path impact.

---

## 5. microvm/virtio_fs.rs minimal change

Before (~40 lines of inline loop):
```rust
pub fn start_worker(&mut self, vm: &VmFd, memory: GuestMemoryMmap) {
    // register ioeventfd
    // spawn thread with inline loop
}
```

After (~10 lines of session construction):
```rust
pub fn start_worker(&mut self, vm: &VmFd, memory: Arc<GuestMemoryMmap>) {
    if self._worker.is_some() { return; }
    let notify_addr = self.mmio_base + 0x050;
    vm.register_ioevent(&self.kick_fd, &IoEventAddress::Mmio(notify_addr), NoDatamatch)
        .expect("register ioeventfd for virtio-fs");
    let session = s3_rootfs::worker::FsSession {
        fs: self.fs.take().expect("fs"),
        memory,
        queue: self.queue.take().expect("queue"),
        kick_fd: self.kick_fd.try_clone().unwrap(),
        irqfd: self.mmio.irqfd.try_clone().unwrap(),
        interrupt_status: self.mmio.interrupt_status.clone(),
        thread_name: "vm-fuse".into(),
    };
    self._worker = Some(s3_rootfs::worker::serve(session));
}
```

Requires:
- `VirtioFs::new` takes `Arc<S3Fs>` instead of `F: FileSystem`.
  Removes the `FuseServer` trait object — cleaner, no dyn dispatch.
- `VirtioMmioDevice::interrupt_status` becomes `Arc<AtomicU32>`.
  Small API change; the mmio device was already sharing that word
  with the worker, Arc just makes it explicit.

Both perf-neutral.

---

## 6. launcher.rs (new file in microvm)

Seed of the future orchestrator. Today runs in-process with the VM.

```rust
pub struct LaunchConfig {
    pub kernel_path: String,
    pub initrd_path: String,
    pub net_ifname: Option<String>,
    pub mem_mb: usize,
    pub vm_id: String,
    pub fs: s3_rootfs::FsConfig,
}

/// Phase 1 — build the filesystem. Future: moves to orchestrator.
pub fn prepare_filesystem(cfg: &LaunchConfig) -> Arc<S3Fs> {
    Arc::new(S3Fs::new(cfg.fs.clone()))
}

/// Phase 2 — build VM, wire devices, run vCPU loop.
pub fn launch_vm(cfg: LaunchConfig, fs: Arc<S3Fs>) -> Result<(), Error> {
    // All the current main.rs::System::new logic moves here.
    // Takes fs: Arc<S3Fs> — orchestrator can hold its own reference.
}

pub fn run(cfg: LaunchConfig) -> Result<(), Error> {
    let fs = prepare_filesystem(&cfg);
    launch_vm(cfg, fs)
}
```

`main.rs` shrinks to arg parsing → build LaunchConfig → `launcher::run`.

---

## 7. Benchmarks — BASELINE BEFORE ANY REFACTOR

Add `s3-rootfs/benches/` with Criterion. Drive the FUSE trait directly
against RustFS using existing `tests/harness/` fixtures.

| Benchmark | Measures | Gate (must not regress) |
|---|---|---|
| `lookup_cold` | first-time path resolution incl. LIST | baseline p50 / p99 |
| `lookup_cached` | pure inode cache hit | < 5µs |
| `readdir_100` | readdir on 100-child directory | baseline throughput |
| `read_1mb_sequential` | open + sequential read of 1MB file | baseline MB/s |
| `read_16mb_cached` | repeat read of cached file | memcpy-bound |
| `write_release_1mb` | write 1MB + release (S3 PUT) | baseline latency |
| `hit_rate_realistic` | existing test workload | ≥ 66.7% |

**Worker loop perf** (ioeventfd, batch drain, single irqfd):
Hard to bench without a VM. For phase 1, confirm Criterion numbers
stay flat. Validate under real workload (boot time, `wget` throughput,
`cargo build` inside VM) when the refactor lands.

---

## 8. Step-by-step refactor

Each step is small enough to revert. Benchmarks run after each.

| Step | Change | Bench gate |
|---|---|---|
| 0 | Add `benches/`, commit baseline numbers | baseline established |
| 1 | Extract `worker.rs` in s3-rootfs, microvm delegates via `FsSession` | flat |
| 2 | Rename `cache.rs` → `inode_cache.rs`; add `blob_cache.rs` + `storage.rs` + `config.rs` scaffolds (no behavior change) | flat |
| 3 | Add `launcher.rs` in microvm; slim `main.rs` | flat |
| 4 | Thread `FsConfig` through the whole API | flat |
| 5 | Move body parsing (hardlink byte-checks etc.) from fs.rs into `storage::parse_body` | flat |
| 6 (later) | Real `blob_cache.rs` tiers (RAM LFU, SSD LRU) gated behind golden-ref feature | separate effort |
| 7 (later) | Golden-ref / materialize-on-write (see research/10) | separate effort |

Steps 0–5 are this refactor. 6–7 are separate future work.

---

## 9. Code quality bar (for OSS release)

- Every non-obvious line has a comment explaining WHY (not what).
- `unsafe` blocks have a `// SAFETY:` comment with the invariant.
- TODOs for future optimizations are acceptable and expected.
- No dead code. Re-exports are intentional.
- `#[derive(Debug)]` on public types where cheap.
- Docs on every `pub` item.
- No global state beyond what already exists (log fn, hardlink counter).
  Audit these for N-VM compatibility (current state: both are fine).
