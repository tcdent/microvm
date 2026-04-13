# Golden Filesystem — Implementation Design

Companion to `research/10-fs-daemon.md` (the architectural motivation) and
`research/07-s3-filesystem-design.md` (the current single-bucket model).
This document specifies the concrete implementation plan for fleet-wide
rootfs deduplication + cross-VM read caching, building on the refactor
foundation landed in `305d729…df0734a`.

Scope: in-process orchestrator (shared memory via `Arc`, no IPC). The
daemon split from `research/10` stays deferred indefinitely — we get the
fleet-scale wins through thread ownership within a single host process.

---

## 1. Goals and non-goals

**Goals**

- Store the golden rootfs once per host, referenced from every VM
- Serve hot rootfs files from host RAM (microsecond latency)
- Serve warm rootfs files from host SSD (tens of microseconds)
- Per-VM buckets contain only the VM's actual modifications
- Zero-cost property preserved: a VM that never writes stores nothing new
- Shared S3 connection pool across all VMs on a host

**Non-goals (for v1)**

- Multi-version golden coexistence (one version per host)
- Persistence of VM state across reboot (separate future work)
- Cross-host golden coordination (per-host caches, no fleet gossip)
- Daemon/IPC split (we stay in-process with `Arc`)
- Refcount or GC of orphaned golden blobs (manual or out-of-band sweep)

---

## 2. Storage layout

Two S3 buckets per host:

```
golden
  <hash>                              # content-addressed blob, immutable
  <hash>
  ...                                 # populated once per golden build
  .manifest                           # path → hash mapping (one object)

vm-<instance-id>
  <path>\x01<mode><uid><gid>          # direct content: created/modified by THIS VM
  .hardlinks/<uuid>\x01<meta>         # hardlink data pool (existing)
```

**Golden bucket** is shared read-only across every VM on every host.
Populated by the bootstrap pipeline when a new rootfs version is built.
Objects are content-addressed by BLAKE3 hash (32 hex chars = 128 bits).
Immutable: new content → new hash → new key; existing keys never
overwritten (conditional PUT on absence, not on match).

**Per-VM bucket** contains only what diverges from golden. A VM that
boots, reads, and shuts down without writing anything stores zero new
objects. The bucket's purpose is to hold the VM's delta, not mirror
the rootfs.

**Key invariant:** guests have write credentials scoped to `vm-<id>` only.
`golden/` is never written by guest code — only by the bootstrap pipeline
(trusted host process, runs offline). Content-addressing makes
tampering self-detecting: if `golden/<hash>` ever contains bytes that
don't hash to `<hash>`, any consumer re-verifying on read will notice.

---

## 3. Cache architecture

Two flat RAM-resident caches, both shared host-wide:

```
Per-VM lookup
    │
    ▼
┌──────────────────────────────────┐
│ Per-VM InodeCache                │  Writable, VM-local
│ - files created/modified by VM   │  Mutex<InodeCache> per S3Fs
│ - negative entries (deletions)   │
└──────────────┬───────────────────┘
               │ miss
               ▼
┌──────────────────────────────────┐
│ GoldenInodeCache          SHARED │  Immutable after init
│ - path → (hash, mode, uid, gid,  │  Arc<GoldenInodeCache>
│         size, symlink_target)    │  Lock-free concurrent reads
└──────────────┬───────────────────┘
               │ miss
               ▼
┌──────────────────────────────────┐
│ vm-<id> S3 LIST probe            │  Only files unique to this VM
└──────────────────────────────────┘

For reads resolved via golden:
┌──────────────────────────────────┐
│ GoldenBlobCache           SHARED │  hash → Arc<Vec<u8>>
│ - entire golden blob pool, in    │  Arc<GoldenBlobCache>
│   RAM, ~120 MB known quantity    │  HashMap, immutable after init
│ - loaded once at orchestrator    │  Lock-free concurrent reads
│   init from S3 golden bucket     │
└──────────────────────────────────┘
```

### Why one flat RAM-resident blob cache, not a tiered structure

The golden rootfs is a known, bounded, controlled artifact — currently
~120 MB, and we (not an upstream vendor) decide its size. A tiered cache
(RAM + SSD + S3 fallback) with eviction policies would solve a problem
we don't have: "what if the working set doesn't fit in RAM?" At 120 MB
on hosts with tens of GB, it always fits. Pinning everything in RAM and
serving from a `HashMap<Hash, Arc<Vec<u8>>>` yields:

- Lock-free reads (immutable after init)
- Microsecond latency (bounded by one hash lookup + Arc clone)
- Zero eviction code, zero disk I/O, zero tier coordination
- Zero cache-miss paths on the hot read path (misses are structural bugs, not capacity events)

This is the payoff of owning the storage layer. We don't generalize for
arbitrary workloads; we tune for the one we have.

**Loading cost is excluded from guest startup benchmarks.** Orchestrator
init fetches the 120 MB in parallel from S3 (~1–2 s against local RustFS),
populates both golden caches, then signals ready. VMs launched after
that incur zero load-from-S3 cost for any golden-inherited read. A
restart that re-fetches is a rare, non-benchmarked event.

**Per-VM InodeCache** stays close to today's design minus TTL (see §9).
Authoritative for files this VM has touched. Negative cache handles
deletions of golden-inherited paths (no tombstones in S3).

**GoldenInodeCache** is immutable after construction, `Arc`-shareable,
no `Mutex`, no TTL. Populated once per host at orchestrator startup
from the `.manifest` object. Concurrent reads across N VMs are free.

**GoldenBlobCache** is the same pattern applied to blob content:
`HashMap<String, Arc<Vec<u8>>>` behind an `Arc`, populated once at
orchestrator startup by bulk-fetching every object in the golden
bucket. No LFU, no LRU, no eviction — the working set fits, so
nothing is ever evicted. Lost on orchestrator restart, re-fetched on
orchestrator startup. No persistence layer.

---

## 4. Connection pooling

One `Arc<ureq::Agent>` per host, shared across every `S3Client` (per-VM
buckets and the golden bucket). `ureq::Agent` is internally `Send + Sync`
— concurrent requests through a shared agent use the same pool without
external locking.

**Code shape:**

```rust
// In s3-rootfs/src/s3.rs
impl S3Client {
    pub fn new(config: S3Config) -> Self { ... }  // builds its own agent

    /// Construct with a pre-built shared agent. Orchestrator-era usage:
    /// one agent for the whole host, every per-VM and golden client
    /// shares the TCP/TLS pool.
    pub fn with_shared_agent(config: S3Config, agent: Arc<ureq::Agent>) -> Self { ... }
}
```

Today's single-VM build uses `S3Client::new`. When orchestrator lands,
it constructs one `Arc<ureq::Agent>` and passes clones into each VM's
`S3Fs` construction. Same code, different caller.

Connection pool budgets (currently per-VM: `max_idle_connections = 32`,
per-host `= 16`, age `= 30s`) become per-host under sharing. A 64-VM
host no longer has 64 × 8 = 512 HTTP connections; it has ~32 shared.

---

## 5. Lookup flow

FUSE `lookup(parent_ino, name)` with the golden layer active:

```rust
fn lookup(parent, name) -> Entry {
    let path = resolve_child(parent, name)?;

    // Tier 1: per-VM inode cache (hot: 1-5µs typical hit)
    {
        let mut cache = self.cache.lock().unwrap();
        if let Some(entry) = cache.get(&path) {
            return Ok(entry_for(entry));
        }
        if cache.is_negative(&path) {
            return Err(ENOENT);   // covers both probe-miss and guest-delete
        }
    }

    // Tier 2: shared golden inode cache (lock-free read, 100-500ns)
    if let Some(ref golden) = self.golden_inodes {
        if let Some(gentry) = golden.get(&path) {
            // Found in golden. Return the entry; blob lookups go
            // through blob_cache keyed by gentry.hash on read.
            // Per-VM cache stays clean — no duplication of golden
            // metadata. Every future lookup re-traverses the 2 tiers.
            return Ok(entry_for_golden(gentry));
        }
    }

    // Tier 3: LIST probe against vm-<id> bucket (5-15ms S3 round-trip)
    probe_and_cache(&path)
}
```

**Design choice: do NOT mirror golden entries into per-VM cache on hit.**
Every lookup walks both tiers. Cost is ~500ns extra per lookup; saves
the memory duplication and keeps the delta-only semantic honest. With
single-writer correctness and no TTL, the "memory grows bounded by
workload" guarantee gets weaker if we mirror — a `find /` would pull
the entire golden manifest into per-VM cache. Staying pure avoids that.

---

## 6. Read flow

```rust
fn read(ino, offset, size) -> Bytes {
    let (path, meta, hl_uuid) = path_meta_hl_for_inode(ino)?;

    // For files in the per-VM bucket (direct, hardlink, or not golden)
    let key = match hl_uuid {
        Some(uuid) => storage::hardlink_key(&uuid, meta),
        None => keys::encode(&path, meta),
    };

    // If this inode came from golden, serve from the RAM-resident
    // blob cache. One HashMap lookup, one Arc clone, done.
    if let Some(hash) = golden_hash_for_inode(ino) {
        let bytes = self.golden_blobs
            .fetch(&hash)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "golden cache miss"))?;
        return serve_slice(bytes, offset, size);
    }

    // Otherwise normal S3 GET against vm-<id>
    let raw = self.client.get(&key)?;
    match storage::parse_body(raw, meta.file_type()) {
        ObjectBody::Direct(bytes) => serve_slice(bytes, offset, size),
        ObjectBody::Hardlink(uuid) => { /* existing logic */ }
        ObjectBody::Symlink(_) => unreachable!(),
    }
}
```

Note: earlier drafts of this doc carried an `ObjectBody::GoldenRef`
variant for "per-VM bucket contains a ref to a golden blob." Under the
delta-only model that variant has no producer — guest writes are
always direct content, and golden lives entirely in the shared RAM
caches. The variant was removed in a post-G2 hygiene pass (see the
commit series ending in G3). `parse_body` returns three variants today:
`Direct`, `Hardlink`, `Symlink`.

**Golden serves from RAM, period.** No tier walk, no cache miss, no
S3 round-trip. The blob is there (because we pinned every golden blob
at orchestrator init) or it doesn't exist (which would be a structural
bug, not a capacity event). Treating a golden-blob miss as `unreachable!`
or as a loud error is correct — it means the manifest and the blob
pool are out of sync, which is an orchestrator-level invariant
violation, not a cache-layer concern.

---

## 7. Write flow and materialize-on-write

Writes always land in `vm-<id>/` at the visible path. The guest never
has a write path that touches `golden/`. This is enforced by:

- `S3Fs` holds one write-capable client pointed at `vm-<id>`
- Golden client is constructed with read-only credentials (future: IAM scoping)
- No code path in `release()`, `create()`, `mkdir()`, `unlink()`,
  `rename()`, or `link()` writes anywhere but the per-VM bucket

**Materialize-on-write for golden-inherited files:**

1. Guest `open(f, O_WRONLY)` on a golden-inherited path
2. `open()` sees the inode is golden-backed (hash hint on `Entry`)
3. `open()` calls `blob_cache.fetch(&hash)` to get the golden content
4. Loads bytes into the handle's `data` buffer — handle now owns its
   own copy, conceptually divorced from the golden blob
5. Guest writes → buffer diverges
6. `release()` does what it does today: PUT the buffer to
   `keys::encode(&path, meta)` in `vm-<id>`. Body is direct bytes.
7. Subsequent lookups on this path hit per-VM cache first (VM now has
   its own entry for this path), golden cache is not consulted.

The critical property: `release()` doesn't need to know the handle
came from golden. It just writes the buffer to the visible key. The
fact that the buffer originated in the blob cache is irrelevant by the
time release fires — it's just bytes.

**Deletion:**

1. Guest `unlink(f)` where `f` is a golden-inherited path
2. Nothing in `vm-<id>` to DELETE
3. Insert sticky negative into per-VM cache for `f`
4. Future lookups: per-VM negative hit → ENOENT before golden is consulted
5. Future readdir on parent: filter out paths matching negative cache
6. No S3 write at all (deletion of golden-inherited path is free of S3 ops)

If the guest deletes its own direct file, DELETE as today — the S3
object exists in `vm-<id>` and needs to be removed.

---

## 8. Configuration

```rust
// s3-rootfs/src/config.rs

pub struct FsConfig {
    pub s3: S3Config,                      // per-VM bucket
    pub read_cache_max: u64,
    pub read_ahead_size: usize,
    pub golden: Option<GoldenConfig>,      // None = goldenref disabled
    pub blob_cache: BlobCacheConfig,
}

pub struct GoldenConfig {
    /// Name of the golden bucket on the same S3 endpoint.
    pub bucket: String,

    /// Optional read-only credentials scoped to the golden bucket.
    /// None = reuse the main `s3.credentials`.
    pub credentials: Option<rusty_s3::Credentials>,

    /// Path to a local manifest file OR S3 key within the golden
    /// bucket. `.manifest` is the default convention.
    pub manifest: ManifestSource,
}

pub enum ManifestSource {
    S3Key(String),          // e.g. ".manifest"
    LocalPath(PathBuf),     // for offline/dev scenarios
}

// No BlobCacheConfig — the blob cache is either populated in full
// from S3 at orchestrator init, or it's not active at all. There's
// nothing to tune.
```

Default `FsConfig::default()` keeps `golden = None` — feature disabled
by default, current single-bucket behavior unchanged.

---

## 9. History: InodeCache TTL removal and the blob cache detour

**InodeCache simplification (Phase 0a, landed as 50385c7).** Removed
`DEFAULT_CACHE_TTL`, `ttl`/`op_count`/`last_accessed` fields,
`maybe_reap()`, `with_ttl()` constructor, and TTL check in
`is_negative`. Collapsed `negative` from `HashMap<String, Instant>` to
`HashSet<String>`. Removed `cache_ttl` from `FsConfig` and
`VM_CACHE_TTL` from `main.rs::config_from_env`. Rationale: single-writer
invariant means cached entries are always correct; memory is bounded by
the VM's actual workload (tens of MB typical), which is small enough
that eviction machinery isn't worth the code. The module-level docstring
spells out what would bring TTL back if assumptions ever change.

**Blob cache detour (Phase 0b, landed as 586c815, being removed).** When
the TTL/reap code was deleted from `InodeCache`, the rotating-offset
probabilistic sampling pattern was moved into `blob_cache.rs` as a new
`RamBlobCache` + `DiskBlobCache` stub + `TieredBlobCache` composer —
on the reasoning that the sampling approach was well-built and would
eventually serve the blob cache's real memory pressure.

**Reason for removal: the blob cache doesn't have memory pressure.** The
golden blob pool is a bounded, known-size artifact (~120 MB) that we
control. Pinning it all in RAM at orchestrator init is cheaper (in both
code and cycles) than any eviction scheme. A single flat
`HashMap<Hash, Arc<Vec<u8>>>` with no locks and no eviction is strictly
simpler than a tiered cache with sampling, and the simpler design
matches the actual workload. The sampling logic is preserved in git
history (commit `586c815`) for any future workload that needs it.

The replacement is a single `GoldenBlobCache` type, described in §3 and
§10 below. ~30 lines instead of ~250.

---

## 10. Implementation phases

Each phase is small enough to review in isolation, has a clear bench gate
(vs the previous commit per the new methodology), and can be reverted
cleanly if issues surface.

| Phase | Scope | Status | Bench gate |
|---|---|---|---|
| 0a | InodeCache TTL removal | ✓ landed (50385c7) | Flat vs df0734a |
| 0b | Blob cache scaffolds (rotating-offset LFU port) | ✓ landed (586c815), later removed in 3′ | Flat (no wiring) |
| 1 | `S3Client::with_shared_agent` | ✓ landed (2662e9f) | Flat |
| 3′ | Remove 0b scaffolds (premature abstraction superseded by single flat RAM cache) | ✓ landed (653b55b) | Flat (no callers) |
| 2 | `GoldenInodeCache` + `GoldenBlobCache` + manifest loader + bulk parallel S3 fetch at init + multi-bucket `S3Client::get_from` | ✓ landed (f921cbf) | Flat (not wired); init-time measured separately |
| **G1** | **Golden FUSE integration** — `FsConfig::golden` plumbing, S3Fs holds the Arcs, `lookup()` tier-2 check, `read()`/`open()` serve from `GoldenBlobCache`, `unlink()`/`readdir()` filter via negative cache | ✓ landed (72c64ba) | golden_lookup 179 ns, golden_read_1mb 44.9 µs (~22 GiB/s), golden_open_materialize 45.2 µs |
| — | Rename `dist/s3-upload` → `dist/golden-upload` (pure rename) | ✓ landed (67c66a5) | — |
| **G2** | **Bootstrap pipeline** — BLAKE3 hashing in `golden-upload`, golden PUTs, manifest emission in `bootstrap-rootfs.sh`, schema_version dropped | ✓ landed (7ddf02f) | Offline tool, bench not applicable |
| — | Ino partition fix (1<<63 → 1<<40; kernel VFS rejected i64-negative inos) | ✓ landed (5661f90) | Boot regression fix surfaced during G3 diagnosis |
| **G3** | **End-to-end VM boot on golden rootfs** — `bench-boot.sh`, T0/T1/T2 instrumentation, N=5 both modes | ✓ landed (59a50fd) | **nogolden 23.011s, golden 15.759s, -31.5%; variance 2.21s → 0.75s** |
| — | Post-G2 hygiene — drop `ObjectBody::GoldenRef` variant + `negative_hits` counter, restore `is_negative(&self)`, bootstrap.sh split into golden + per-VM buckets | ✓ landed (88601e3) | Flat (pure code removal + config plumbing) |

### Phase compression

The original plan split the golden integration across four phases (4/5/6/7)
for "scaffold-first" discipline. In practice they form one atomic conceptual
change — shipping them separately produces intermediate commits with broken
read paths (a lookup that resolves to golden but a read that can't serve
from the cache). Merged into **G1**.

**G1** is the biggest diff of the three, probably 200-300 lines across
`s3fs.rs`, `config.rs`, and helpers. If it gets unwieldy the natural seam
is `G1a` (wire + lookup + read/open) / `G1b` (unlink/readdir delete
semantics). Start combined; split only if review feedback says to.

**G2** is independent of G1 at compile time; can land in parallel. Different
code area (shell + `s3-upload` Rust), different review concerns, different
bench methodology (offline tool).

**G3** depends on G1 + G2 being done — it's the integration measurement.

### Bench methodology reminders

- Baseline is always the previous commit, never a weeks-old reference
- N=3 minimum on S3-bound reads; ±20% noise budget for <1GB cells
- Orchestrator init (the ~1-2s bulk fetch of the golden pool) is
  infrastructure startup, not per-VM startup — excluded from guest
  boot-time benches. It's a one-time cost amortized across every VM
  that subsequently boots against the ready caches.
- G3's boot-time number is what validates the whole effort. Don't
  stash it, don't apologize for warm-up — just run vs the existing
  in-VM baseline and report the delta.

### Sprint outcome (canonical reference)

G3 delivered **-31.5% boot time** with **3× tighter variance** vs the
pre-golden baseline, on the same host, same rootfs, same microvm binary
with only the `VM_GOLDEN_BUCKET` env var differing between the A and B
arms. Golden-mode boot reaches `login:` in 15.759s (median, N=5, range
15.440–16.192s); nogolden in 23.011s (range 21.573–23.783s). The
orchestrator-init cost of loading the 120 MB golden pool into RAM is
5.72s, excluded from boot_time per methodology and amortized across
every VM that subsequently boots on the host. Net break-even on the
first VM already (5.72s init − 7.25s per-VM saved = +1.53s); at 10 VMs
per orchestrator lifetime, total time saved is ~66.8s.

The per-op wins that compose into boot-time savings (measured via
criterion `golden_*` microbenches):

| Operation | Golden | Pre-golden S3 | Delta |
|---|---|---|---|
| Lookup hit | 179 ns | ~15.7 ms | ~88,000× |
| 1 MB read (cached) | 44.9 µs (~22 GiB/s) | ~30 ms | ~670× |
| Open + materialize 1 MB | 45.2 µs | ~30 ms + write prep | — |

---

## 11. Bootstrap pipeline

The existing `dist/bootstrap-rootfs.sh` produces a direct-content rootfs
in a single bucket. For the golden model it grows into two outputs:

1. Populate `golden/<hash>` for every file above a size threshold
   (probably ~1KB; files smaller than that have ref indirection cost
   > content dedup benefit)
2. Write `.manifest` object containing every file's `path → hash` +
   metadata (mode/uid/gid/size)
3. Files below the threshold are emitted as direct content in a
   "base vm-<id>" template that future VM creations clone from

**Format: `.manifest` is protobuf or MessagePack**, not JSON. The
manifest for a 10k-file rootfs is ~500KB in JSON, ~200KB in a binary
format. Small enough that loading at orchestrator startup is fine; just
don't waste time on JSON parsing at scale.

In-process today: bootstrap runs per-VM, produces golden content once
per build version, and does the ref-or-direct decision at upload time.

Orchestrator-era: bootstrap is a one-time per-version offline job.
Orchestrator reads `.manifest` once per host per version, populates
`GoldenInodeCache`, caches the parsed structure.

---

## 12. Observability

Extend `CacheStats` to cover the new cache tiers:

```rust
pub struct CacheStats {
    // Per-VM inode cache (existing)
    pub inode_count: usize,
    pub negative_count: usize,
    pub hits: u64,
    pub misses: u64,
    pub negative_hits: u64,

    // Shared golden inode cache (new)
    pub golden_hits: u64,
    pub golden_misses: u64,

    // Shared blob cache (new)
    pub blob_ram_hits: u64,
    pub blob_disk_hits: u64,
    pub blob_s3_fallbacks: u64,
    pub blob_evictions: u64,
}
```

Useful answers to questions the fleet will eventually ask:
- "What's the cross-VM dedup ratio?" (golden hit count vs per-VM write count)
- "Is the hot tier warm enough?" (ram_hits / (ram_hits + disk_hits + s3_fallbacks))
- "Is our disk budget right?" (evictions per unit time)

---

## 13. Open questions / deferred work

- **Versioning.** Single global golden version per host today. Multi-
  version coexistence (running VMs pinned to old version, new VMs on
  new version) needs `Arc<GoldenInodeCache>` indexed by version,
  manifest-version metadata on `vm-<id>` buckets, and GC of unreferenced
  versions. Out of scope until fleet operations demand it.
- **Persistence of VM inode cache across reboot.** Currently the
  per-VM cache is in-memory; a VM reboot drops it. Sticky negatives for
  deletions also drop, meaning a deleted-golden-file would reappear on
  reboot. Either acceptable (VM reboot = fresh slate) or future work to
  snapshot the cache alongside other VM state. Deferred pending
  persistence work.
- **GC of orphaned golden blobs.** When golden versions roll, old
  hashes become unreferenced. Manual or scheduled sweep; not critical
  until storage becomes a concern.
- **Concurrent first-read deduplication.** Two VMs on the same host
  hit a cold golden blob simultaneously — both fetch from S3 and insert
  into the cache. Wasteful but correct (content-addressed, idempotent
  insert). An inflight-map with `oneshot` channels would dedupe but
  adds complexity. Skip until measurement demands it.
- **Scoped IAM credentials for golden.** Today's `GoldenConfig` has an
  optional credentials override; when fleet identity matters, this
  becomes per-host or per-VM STS-issued scoped creds. Design hook is
  already there.
- **Host-wide orchestrator config for S3 tuning knobs.** `S3Config`
  currently exposes `max_idle_connections`, `max_idle_connections_per_host`,
  `max_idle_age`, `timeout_global`, `timeout_connect`, `tcp_nodelay` as
  public fields. These values are empirically tuned defaults; no caller
  overrides them today. When the orchestrator grows a real config
  surface (likely a filesystem-based config file at
  `/etc/microvm-orchestrator.toml` or similar), these tuning knobs
  migrate there — one host-wide set, not per-S3Config. Keeping them
  exposed until that lands so a one-off bench discovery can tune
  without a code change.

---

## 14. Relationship to other research

- `research/07`: defines the current key format (`path\x01<meta>`) and
  the metadata-in-keys rationale. Goldenref reuses the same key format
  on the `vm-<id>` bucket; `golden/<hash>` is a separate namespace with
  its own flat structure.
- `research/10`: originally proposed a daemon split with Unix sockets +
  SCM_RIGHTS. This doc replaces that approach with in-process
  orchestration (shared memory via `Arc`). The cross-VM dedup +
  tiered cache goals are identical; the implementation approach is
  simpler because we don't need IPC.
- `research/11`: the refactor that landed the scaffolding for this work
  (`blob_cache.rs` trait, `storage.rs` helpers, `BlobCacheConfig`). Some
  of that scaffolding (the `BlobCache` trait with tiered-cache semantics,
  `ObjectBody::GoldenRef`, `BlobCacheConfig`) was removed during this
  sprint once the architecture settled on a single flat RAM-resident
  cache — see §9 for the full trail.
