# Filesystem Daemon — Shared Cache + Pooled Workers

Two intertwined wins for fleet-scale microVM hosting:

1. **Content-addressed shared storage** (S3 dedup + host SSD cache)
2. **Out-of-process FS daemon** (pooled workers, shared S3 connections,
   shared inode metadata across all VMs on the host)

These are the same architecture: once the cache is shared across VMs,
the FS layer naturally wants to be a single host-wide service rather
than per-VM threads.

## Problem

For a fleet of N VMs running the same golden rootfs:

- **Storage waste**: each VM has its own S3 bucket with a full 504MB
  rootfs copy. N×504MB of S3 storage, N×504MB of cold-read traffic on
  first access from each VM. ~95% of the rootfs is byte-identical
  across all VMs.
- **Connection waste**: N VMMs × 4-16 S3 connections each = hundreds
  of HTTP connections to RustFS, all making redundant requests.
- **Cache waste**: N independent inode caches, read-ahead buffers,
  metadata tables. Same paths cached N times.
- **Worker waste**: N FUSE worker threads, mostly idle, no work-stealing
  between VMs.

## Goal

- Single canonical copy of shared content (dedup at the storage layer).
- Single host-wide FS daemon serving all VMs (pool at the runtime layer).
- Per-VM storage only for modifications.
- Per-VM credentials and quotas enforced server-side.

## Architecture

### Host-wide FS daemon

```
┌─────────────┐  ┌─────────────┐  ┌─────────────┐
│   VMM-1     │  │   VMM-2     │  │   VMM-N     │
│  (microvm)  │  │  (microvm)  │  │  (microvm)  │
└──────┬──────┘  └──────┬──────┘  └──────┬──────┘
       │                │                │
       └────────┬───────┴────────┬───────┘
                │ Unix socket    │
                ▼                ▼
       ┌────────────────────────────────┐
       │      microvm-fs daemon         │
       │  ┌──────────────────────────┐  │
       │  │  worker pool (autoscale) │  │
       │  ├──────────────────────────┤  │
       │  │  hot RAM cache (LFU)     │  │
       │  ├──────────────────────────┤  │
       │  │  warm SSD cache          │  │
       │  ├──────────────────────────┤  │
       │  │  shared inode metadata   │  │
       │  ├──────────────────────────┤  │
       │  │  pooled S3 connections   │  │
       │  └──────────────────────────┘  │
       └─────────────┬──────────────────┘
                     │
                     ▼
                  ┌─────┐
                  │ S3  │
                  └─────┘
```

VMMs become **thin clients**. They keep what's intrinsically per-VM:
- KVM lifecycle, vCPU thread, virtio-mmio devices
- virtio-net, virtio-console
- A virtio-fs *transport*: pulls FUSE requests off the queue, ships
  them to the daemon over a Unix socket, writes responses back
- Per-handle write buffers (guest-private dirty data)

The daemon owns everything that benefits from pooling:
- All cache layers (hot RAM, warm SSD, inode metadata)
- A single S3 client connection pool, shared across all VMs
- Worker pool that processes FUSE requests from any VMM
- Hardlink/symlink/golden-ref resolution
- Per-VM credential and quota enforcement

### Wire protocol

Two reasonable choices, ranked:

1. **Pass FUSE requests directly** — VMM reads the virtio-fs descriptor
   chain, forwards the raw FUSE bytes to the daemon over a Unix socket
   with `SCM_RIGHTS` to share guest memory mappings. Daemon writes the
   response back into the shared memory region. This is what virtiofsd
   already does; we'd be writing a lighter, S3-native virtiofsd.
2. **Higher-level RPC** — VMM parses FUSE locally, sends typed requests
   (`Lookup`, `Read`, `Write`, etc.) to the daemon. Cleaner Rust types,
   more code, slightly higher overhead. Probably not worth it given
   the FUSE ABI is stable.

Go with option 1.

### Worker pool autoscaling

```
Goal: low tail latency, no over-spawning

Metrics:
  - request queue depth (per-priority if we add fairness)
  - p99 request latency
  - active workers, idle workers

Policy:
  - scale up when queue depth > N for T seconds
  - scale down when idle workers > M for T' seconds
  - bounds: min=4, max=2*num_cpus

Workers are stateless threads. All state is in shared caches. Each
worker pulls from the queue, processes, sends response, returns.
```

### Per-VM isolation

Even though the FS state is shared, per-VM concerns must stay enforced
server-side:

- **S3 credentials**: each VMM's connection identifies itself; daemon
  uses scoped credentials per VM (write access only to `vm-{id}/`,
  read access to `golden/`).
- **Bucket prefix**: daemon enforces that VM `id` can only read/write
  its own `vm-{id}/` namespace. Path validation, not trust.
- **Quotas**: track bytes/requests per VM, enforce limits to prevent
  one runaway VM starving the rest.
- **Worker fairness**: per-VM token bucket on worker scheduling so a
  slow VM (one stuck on huge S3 requests) can't monopolize the pool.

Goldens are read-only for everyone (see "Read-only safety" below) so
no per-VM credential is needed for that namespace.

### Resource savings at scale

For 64 VMs on one host:

| Resource | Per-VM today | Pooled |
|----------|-------------|--------|
| FUSE worker threads | 64 | autoscale 4–32 |
| S3 HTTP connections | ~64 × 8 = 512 | shared pool, ~32 |
| Inode caches | 64 copies | 1 shared |
| Read caches | 64 × 16MB handle | 1 × 256MB hot tier |
| Process count | 64 microvms | 64 microvms + 1 daemon |

The S3 connection consolidation is the big one. Hundreds of TCP
connections to a backing store is wasteful and limits horizontal
scaling — pooling drops that to dozens.

### Failure modes

- **Daemon crashes** → all VMs lose their filesystem. Need:
  - systemd `Restart=always` on the daemon
  - VMMs reconnect on broken socket, retry in-flight requests with
    timeout. FUSE protocol is request/response so retries are safe
    for idempotent ops (read, getattr); write/rename need careful
    handling (idempotency tokens or guest-side retry).
- **Slow VM starves workers** → per-VM token bucket on worker dispatch.
- **Daemon OOM** → hard caps on hot tier, per-VM write buffers, and
  inflight request count. Reject with EBUSY rather than OOM.
- **Cache corruption** → content-addressed = self-verifying. Verify
  hash on warm-tier load, drop on mismatch.

### Two-layer S3 model

```
S3 layout:
  golden/<sha256>                     # immutable content-addressed blobs
  vm-{id}/<path>\x01<mode_uid_gid>    # per-VM rootfs (current format)
```

The `golden/` namespace is **content-addressed**: the key is the
SHA-256 of the file content. Same content → same key. Immutable.

The `vm-{id}/` namespace is per-VM, using our existing key format.
Files in this namespace come in two flavors:

1. **Reference**: object body is `ref:<sha256>` pointing at a
   golden blob. Used for unmodified files inherited from the golden
   image.
2. **Direct**: object body is the actual file content. Used for
   files the VM has modified.

### Read path

```rust
fn read(path) -> Bytes {
    let key = lookup(path);                  // existing FUSE lookup
    let body = s3.get(key);                  // existing GET

    if body.starts_with("ref:") {
        let sha = &body[4..];
        // Check host SSD cache first
        if let Some(data) = ssd_cache.get(sha) {
            return data;                     // cache hit, no S3 traffic
        }
        // Cache miss — fetch from golden, populate cache
        let data = s3.get(format!("golden/{}", sha));
        ssd_cache.put(sha, &data);
        return data;
    }
    body  // direct content
}
```

### Write path

When the VM writes to a reference file:

```rust
fn write(handle, offset, data) {
    let entry = handles.get(handle);

    if entry.is_reference {
        // Materialize: fetch golden content, apply write, mark direct
        let original = resolve_reference(&entry);
        entry.buffer = original;
        entry.is_reference = false;
    }

    apply_write(entry, offset, data);
    entry.dirty = true;
}

fn release(handle) {
    if entry.dirty {
        // PUT real content to vm-{id}/<path>, no longer a ref
        s3.put(vm_key(entry.path), entry.buffer);
    }
}
```

The reference becomes a direct copy on first write. From then on
the VM has its own copy of that file, identical reads no longer
benefit from the shared cache.

### Host cache hierarchy

Three tiers between the guest and S3:

| Tier | Storage | Latency | Capacity | Population |
|------|---------|---------|----------|------------|
| **Hot (RAM)** | mmap/Vec in VMM process | ~ns | hundreds of MB | LFU on golden access |
| **Warm (SSD)** | content-addressed blobs on disk | ~µs | tens of GB | Every golden ever fetched |
| **Cold (S3)** | RustFS / object store | ~ms | unbounded | Source of truth |

Reads check hot → warm → cold in order. Writes go straight to S3
(per-VM namespace) with optional warm-tier population.

#### Warm tier — host SSD

```
/var/cache/microvm/golden/
  ab/cd/abcdef...     # 2-level sharding by sha prefix
  12/34/123456...
```

Layout:
- Content-addressed (sha256 as filename)
- 2-level prefix sharding to avoid huge directories
- LRU eviction past configured size limit (e.g., 10GB)
- Shared across all VM processes on the host

Trivially safe: content-addressed means a hit can never be wrong.
If the file got modified upstream, the new sha would be different,
so the lookup misses and we fetch fresh.

#### Hot tier — VMM RAM, LFU

For blobs accessed frequently across many VMs (libc, common shared
libraries, top crates/npm packages), the SSD round trip still adds
latency we can avoid. Pin the hottest blobs in VMM-process memory.

```rust
struct HotCache {
    blobs: HashMap<Sha256, Arc<Vec<u8>>>,
    counts: HashMap<Sha256, AtomicU64>,    // bumped on every read
    capacity_bytes: usize,                   // configurable, e.g. 256MB
}
```

Eviction: periodic (every N reads or T seconds), drop the least-
frequently-accessed blobs to keep total under capacity. Keep a
running count rather than recomputing — cheap.

A 256MB hot tier can fit ~50-100 typical shared libraries plus the
most-accessed crates/packages, covering the bulk of read traffic
in steady state.

#### Future: fleet-wide consensus

Out of scope for the initial implementation. If we get to large
fleets where new hosts pay a warm-up cost on every cold start, the
natural extensions are:
- Per-host LFU stats published to a coordinator (gossip / etcd / S3)
- New hosts pre-warm their tiers from fleet-hot blob list
- Or: NFS/cluster-fs shared SSD across a rack — sidesteps the
  consensus algorithm by making the warm tier physically shared

Document, defer until single-host caching is in production.

### Build pipeline changes

`bootstrap-rootfs.sh` becomes:

1. Run debootstrap as today
2. Walk the chroot, for each file:
   - Compute SHA-256
   - PUT to `golden/<sha>` if not already present (HEAD check)
   - PUT to `vm-{id}/<path>\x01<meta>` with body `ref:<sha>`
3. Files only ever uploaded to `golden/` once across all VM creations
4. Per-VM bucket only contains references (~10K small ref files,
   total ~few MB instead of 504MB)

### Why not S3 server-side copy?

Tested. S3 server-side copy (`x-amz-copy-source`) is a **byte-for-byte
copy at write time**. No link semantics, no reference counting.
The new object consumes its own storage. Identical to GET+PUT in
terms of S3 storage cost; only saves client bandwidth.

So we can't use copy to dedup — we have to actively design the ref
layer ourselves.

### Why not just rely on ETag?

ETag is unreliable as content identity:
- Multipart uploads have non-MD5 ETags (`<md5>-<part_count>`)
- Server-side encryption can change ETag format
- CopyObject may return different ETags for identical content
- Different S3 implementations (RustFS, MinIO, Ceph) have different
  conventions

ETag is fine as a "did this change?" signal but not as a content
identity for cross-VM dedup. Computing our own SHA-256 is cheap
(~3s for the whole rootfs at 500MB/s) and gives us guarantees.

## Storage and bandwidth analysis

### Per-VM at rest

| | Naive | Shared |
|--|-------|--------|
| Per-VM S3 storage | 504MB | ~5MB (refs only) |
| Total S3 storage (64 VMs) | 32GB | 504MB golden + 320MB refs ≈ 824MB |
| Reduction | — | **97%** |

### First read across fleet

| | Naive | Shared |
|--|-------|--------|
| First-VM cold read of `libc.so.6` | 1× S3 GET, 2MB | 1× S3 GET, 2MB → SSD cache |
| Subsequent VMs reading `libc.so.6` | 1× S3 GET each | SSD read, 0 S3 traffic |
| 64 VMs reading libc | 64× S3 GET (128MB) | 1× S3 GET + 63× SSD reads |
| S3 bandwidth saved per file | — | (N-1) × file_size |

### Per-VM after writes

VM that runs apt-install or compiles things:
- Materializes the modified files (write triggers materialization)
- New direct content goes to `vm-{id}/`
- Unmodified files still references → still served from shared cache

Typical agent workload modifies maybe 10-100 files per session. Cache
benefit stays high for the unmodified majority.

## Open questions

1. **Cache invalidation when goldens change.** Building a new rootfs
   = new shas = new keys. Old goldens become orphans. Need GC. Easy
   policy: weekly cleanup of `golden/` keys that no `vm-{id}/`
   reference points to.

2. **Concurrent first-read race.** If two VMs hit the same uncached
   file simultaneously, both fetch from S3 and write to the cache.
   Wasteful but not incorrect (file is content-addressed). Could
   add a per-key inflight map but probably not worth the complexity.

3. **Host SSD cache size limit.** A 10GB cache covers the whole
   golden rootfs with room to spare. LRU eviction handles overflow.
   Configurable per host.

4. **Per-host vs shared (NFS/Ceph) cache.** Per-host is simpler.
   Cross-host shared would let a VM that migrates hosts skip the
   warm-up. Probably overkill for our use case.

5. **Direct file → reference downgrade.** If a VM writes to a file
   then later writes back exactly the original content, we keep the
   direct copy forever. Not worth detecting (rare in practice, the
   tests + extra metadata aren't worth it).

6. **Symlinks and directories.** Symlink targets are tiny — store
   as direct content, no need for golden refs. Dir markers are
   empty — same.

7. **Hardlinks within a VM.** Our existing hardlinks use
   `.hardlinks/<uuid>` for shared data. Goldens override that —
   hardlinked-from-golden files just both reference the same sha.
   Need to think through the interaction with our existing hardlink
   code.

8. **Versioned goldens.** When we update the golden rootfs (security
   patches, new packages), running VMs still reference old goldens.
   They keep working. New VMs reference new goldens. Old goldens
   GC'd when no reference remains.

## When to build this

Not yet. Current 504MB per-VM works for a small fleet. The break-even
is when storage cost or first-boot cold latency becomes painful:

- **Storage**: at $0.023/GB/month S3 standard, 64 VMs × 504MB = 32GB
  = $0.74/month. Trivial. Build this when we hit hundreds of VMs.
- **Cold-read latency**: more pressing. Each VM cold-boot pulls
  ~50MB of shared libraries from S3 sequentially. SSD cache would
  cut that to milliseconds for warm hosts.

The cold-boot win matters first. Storage win matters at scale.

## Implementation order

The daemon split and the content-addressed dedup are independent —
either can ship first. Two phasings depending on priorities:

**Phase 1 option A (storage win first):**
1. SSD cache layer in the existing per-VM VMM (no schema change).
   Wrap `s3.get()` with content-addressed cache. Measurable cold-boot
   speedup immediately.
2. Add the golden/ref S3 layer. Requires bootstrap pipeline change.
3. Daemon split. Move all FS code into a separate process, VMM
   becomes a thin client.
4. Hot RAM tier with LFU.

**Phase 1 option B (pooling win first):**
1. Daemon split with the existing per-VM S3 layout. VMM ↔ daemon
   over Unix socket using virtiofsd-style FUSE forwarding. Workers
   pooled, connections pooled. No dedup yet.
2. SSD cache layer (now naturally shared since daemon is one process).
3. Golden/ref dedup.
4. Hot RAM tier.

**Recommended: option A.** Storage win is more visible and gates on
less infrastructure work. The daemon split is a heavier lift —
multi-process IPC, retry semantics, failure handling — best done
once the in-process design is proven.

5. **Materialize-on-write** semantics for ref files. Touches the
   write path carefully — needs solid tests.
6. **Convention-based dispatch** for package-manager caches (npm,
   cargo, pip, etc).
7. **Fleet-wide consensus** for the hot tier (deferred — see future
   section).

## Extending dedup to package manager caches

The same model works for runtime-installed package caches — the user
storage that dwarfs the rootfs (npm node_modules, cargo registry,
pip wheels, etc). Everything is content-addressable, identical
across VMs running the same project, and pointless to store per-VM.

The challenge is detection: package managers spray caches under
user home directories. Detecting "this is a cache" by inspecting
file contents is unreliable.

### Convention-based dispatch

Instead of detection, document a set of **path conventions**. Files
under those paths get the dedup treatment automatically. Files
outside go through the normal per-VM path.

```
DEDUP_PREFIXES = [
    "root/.cargo",          # Cargo registry + git deps
    "root/.npm",            # npm cache
    "root/.cache/pip",      # pip wheels
    "root/.cache/uv",       # uv (Python)
    "root/.go/pkg",         # Go modules
    "root/.m2",             # Maven
    "root/.gradle",         # Gradle
    "root/.venv",           # Python virtualenvs (canonical location)
    "var/cache/apt",        # APT package cache
    "var/cache/microvm/shared",  # explicit user opt-in
]
```

The deal we offer agents: **use these paths and your installs are
fast and free** (no per-VM storage cost, no per-VM cold-fetch latency).
Use random paths and it works normally — just per-VM storage as today.

### Implementation: in-FUSE dispatch

No additional mounts. Single virtio-fs filesystem, dispatch happens
inside our FUSE layer:

```rust
fn read(path) -> Bytes {
    if matches_dedup_prefix(path) {
        return shared_cache.get(path);  // SSD + content-addressed
    }
    per_vm_read(path)
}

fn release(handle) {  // file closed after writes
    if matches_dedup_prefix(entry.path) && entry.dirty {
        let sha = sha256(&entry.buffer);
        if !s3.head(format!("golden/{}", sha)) {
            s3.put(format!("golden/{}", sha), &entry.buffer);
        }
        s3.put(per_vm_key(entry.path), format!("ref:{}", sha));
        ssd_cache.put(sha, &entry.buffer);
    } else {
        normal_release(handle);
    }
}
```

### Why this works

- **Cargo/npm/pip artifacts are immutable** by version. `serde-1.0.193` is
  always the same bytes everywhere. Content-addressed dedup is exactly
  the right model.
- **Hash on release, not on every write**: package managers tend to
  download → unpack → leave alone. Hash once when the file closes,
  no per-write overhead.
- **Tools auto-use the conventions**: `CARGO_HOME=/root/.cargo` is the
  default. `npm config get cache` is `/root/.npm` by default. Pip
  uses `/root/.cache/pip`. The conventions match what tools already
  do — no env var hacks needed.
- **Cross-VM benefit**: 64 VMs running `cargo build` on the same
  project = each crate downloaded once (across the entire fleet,
  not per VM). Same for `npm install`, `pip install`.

### Read-only safety: enforced host-side

**Critical invariant: golden blobs are immutable, enforced by the VMM,
never trusted to the guest.** Once a blob is uploaded to `golden/<sha>`,
no guest can ever mutate it. Poisoning the well would mean every VM
on the fleet gets corrupted content from then on — catastrophic.

Enforcement layers:

1. **Content-addressed = self-verifying.** The key IS the hash. If
   anyone modified `golden/<sha>`, the content no longer matches the
   key. We can re-verify on every cache load (cheap with blake3 — GBs/sec).
   Any mismatch → drop the blob, refetch, alert.

2. **VMM is the only writer to `golden/`.** S3 credentials given to
   the guest (if any) MUST NOT include write access to the `golden/`
   prefix. Use IAM bucket policies / RustFS access keys scoped to
   the VM's own `vm-{id}/` prefix. Even if the guest is fully
   compromised, it cannot touch goldens.

3. **VMM never overwrites existing goldens.** Write path uses
   conditional PUT (`x-amz-copy-source-if-none-match` or HEAD-then-PUT).
   If `golden/<sha>` already exists, skip the PUT. New content
   produces a new sha → new key, never overwrites.

4. **Materialize-on-write means VM writes always go to per-VM
   namespace.** Write path:
   ```
   if writing to a ref file:
       fetch golden content → buffer
       apply guest write → buffer (now diverged from golden)
       PUT to vm-{id}/<path>  ← per-VM namespace, never to golden/
       update cache: this path is now direct, not a ref
   ```
   The guest cannot influence what gets written under `golden/`.
   Only the bootstrap pipeline (one trusted host process) writes
   there, and only at known paths from a known clean source.

5. **SSD cache is content-verified on insert.** When the VMM fetches
   a golden blob and writes it to the SSD cache, hash the bytes and
   verify they match the requested sha. If not, the upstream is
   compromised — abort, log, refuse to serve.

6. **No guest-side path can map into `golden/`.** The guest sees its
   `vm-{id}/` rootfs through FUSE. Refs are dereferenced internally
   and presented as file content. The guest never sees a path that
   points into `golden/`. There is no syscall they can make that
   modifies a golden blob.

The threat model is "fully compromised guest with root inside the
VM tries to corrupt the shared cache." Everything above prevents it
because:
- Guest has no S3 credentials for `golden/`
- VMM never writes guest data to `golden/` (only bootstrap-time
  uploads)
- Content addressing makes tampering instantly detectable

Even if the guest could somehow trick the VMM into writing arbitrary
bytes to `golden/<some_sha>`, the bytes would have to hash to
`<some_sha>` for any other VM to read them as that ref — which is
a sha256 preimage attack. Not possible.

### Measured impact (estimate)

A typical agent session that does `cargo build` on a medium project:
- Without dedup: ~500MB to ~2GB of crate sources + compiled artifacts
  in `~/.cargo/registry/`, all per-VM
- With dedup: same content, single copy across the fleet, ~milliseconds
  to fetch from local SSD instead of seconds from S3

Same pattern for `npm install` (often >1GB of node_modules), pip
(scientific packages can be hundreds of MB), Go modules.

Total dedup savings at scale could exceed the rootfs savings by 10×.
The user-dir cache is the bigger story than the rootfs itself.
