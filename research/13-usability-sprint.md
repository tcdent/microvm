# Usability Sprint

Tracking observations from interactive use of the golden-FS microvm and
the design work to address them. Companion to research/12 (golden FS
implementation); picks up where the golden sprint closed.

---

## 1. Observations from interactive use

Notes accumulated during hands-on sessions with the running system.
Each is a real-world rough edge, not a theoretical concern.

### 1.1 Host kernel `perf: interrupt took too long`

Self-regulating kernel message on the host: `kernel.perf_event_max_sample_rate`
gets auto-lowered when perf interrupts exceed the budget (2500 µs). Not
an error, not fatal, but it clutters `dmesg` during interactive sessions.

Triggers: KVM PMU virtualization creating perf events, profilers on the
host, bench tools that hook perf counters. Not in our code.

Mitigations (host-side sysctl, not a code change):
- `sysctl kernel.perf_event_max_sample_rate=100000` (reset default)
- `sysctl kernel.perf_cpu_time_max_percent=50` (increase budget)

Filed under: console-output cleanup during demos/dev.

### 1.2 Small-file writes kill throughput (`npm install`)

Every `release()` on a dirty file buffer fires one synchronous S3 PUT.
For a 200-byte file that's dominated entirely by the round-trip
(~5–20 ms against local RustFS, more against remote). An npm install
with tens of thousands of tiny files takes literal minutes of serial
PUT latency even though the bytes transferred are trivial.

The fix is the subject of §2 (deferred writes) in this document.

### 1.3 tsx postinstall failed to exec esbuild

npm package `tsx` installed fine — its postinstall hook tried to run
the bundled esbuild binary (`/usr/local/lib/node_modules/tsx/node_modules/esbuild/bin/esbuild --version`)
and the spawn failed. Package itself landed, bin symlink didn't.

Possible causes, unverified:
- File mode on freshly-created files: `create()` hardcodes `0o755`, so
  fresh binaries should be executable, but `chmod` is a no-op (setattr
  deferred per research/07 §10c) so any runtime chmod to adjust perms
  doesn't stick.
- FUSE/execve semantics around mmap: `execve` uses mmap/page-cache to
  load binaries, not read(). If our virtio-fs transport or
  fuse-backend-rs doesn't expose the right mmap capability, execve
  fails on FUSE-backed binaries.
- Missing shared library dependencies in the minbase rootfs. esbuild
  might need libs we didn't install.

Diagnostic path when this is picked up: `strace -f -e execve` of the
failing spawn inside a guest to see the actual syscall errno.

### 1.4 `zip`/`unzip` not in baseline rootfs

bootstrap-rootfs.sh's PACKAGES list covers a minimal working set but
doesn't include archive tools beyond gzip. Missing: `zip`, `unzip`,
probably `tar` as a separate concern, possibly `jq`.

Two ways to address:
- Add to baseline PACKAGES — lands in golden, available everywhere
  with zero config. Cheap (~500 KB for zip/unzip).
- Make `apt install` reliably fast inside a running VM — decouples
  tooling from golden-image versioning. Blocked on the write-path
  weaknesses in §1.2 (tiny files), plus setattr persistence + rename
  atomicity. The real answer, but bigger scope.

Probably both: ship a better baseline AND fix the write path.

---

## 2. Deferred writes

The core fix for §1.2. Replace synchronous `release()` → S3 PUT with
async queueing and a worker pool.

### 2.1 Design

**Memory ownership.** Every dirty file buffer transitions from
`entry.data: Option<Vec<u8>>` (handle-local) to
`Arc<Vec<u8>>` (S3Fs-owned, held in an inflight map) at `release()`
time. The S3Fs instance owns the memory until the PUT worker confirms
the write landed in S3; then the inflight entry is dropped and the
Arc refcount falls.

```rust
struct InflightWrite {
    bytes: Arc<Vec<u8>>,
    key: String,            // already-encoded S3 key
    submitted_at: Instant,  // for diagnostic / stale-write detection
}

struct S3Fs {
    // ... existing fields ...
    inflight: Mutex<HashMap<String, InflightWrite>>,  // path → in-flight write
    write_pool: WriteWorkerPool,                      // background PUT workers
}
```

**Flow.**

1. `release(handle)` on a dirty handle: take `entry.data`, wrap in
   `Arc::new(...)`, insert into `inflight[path]`, signal the worker
   pool via `Condvar` or a channel, return immediately. No S3 call
   on the FUSE-handler thread.
2. A background worker pops the next inflight entry, PUTs to S3 using
   the existing `Arc<ureq::Agent>` pool, removes the entry from the
   inflight map on success.
3. On PUT failure: retry policy TBD. At-least-once semantics via retry
   with backoff; log catastrophically if we exhaust retries (the alternative
   is silently losing data, which is worse).

**Read-your-writes consistency.** The inflight map is consulted by
`read()` *before* the S3 GET path. A guest that writes and immediately
reads back the same file always sees its own data — served from the
`Arc<Vec<u8>>` still sitting in the inflight map or (after PUT
completes) from the read cache (§3).

**Connection pool.** Reuses the existing `Arc<ureq::Agent>` (Phase 1,
commit 2662e9f). No new HTTP infrastructure, just parallel PUTs across
the same pool that already serves reads.

**fsync() semantics.** Block until `inflight[path]` is empty. For
`syncfs`-style whole-filesystem fsync: block until all inflight entries
clear. This is the synchronization point that converts async-by-default
into sync-when-requested.

### 2.2 Worker pool sizing

**One pool for the whole app, 8 workers to start.** The PUT workers
consume from whatever S3Fs instance's inflight map they're attached to,
using the existing `Arc<ureq::Agent>` (the shared HTTP connection
pool). No new HTTP infrastructure; the workers just call through the
existing client.

Today (single-VM): workers live inside the S3Fs instance. When the
orchestrator hoists the S3 client/pool up to host scope, the worker
pool hoists with it — one pool of workers serving all VMs. Same
code, different scope. Pool-ownership location is a variable we
rename when we do the hoist, not a design concern today.

If 8 turns out wrong (bench shows pool contention or underutilization),
tune empirically.

### 2.3 Memory budget and backpressure

**Per-VM inflight budget: 100 MB.** Tracked via `AtomicUsize`
(`current_inflight_bytes`). Increments on `release()` enqueue, decrements
on PUT completion.

**Backpressure via synchronous fallback.** When `current_inflight_bytes +
this_write.len() > 100 MB`, the release path falls back to the existing
synchronous PUT — write completes inline, no enqueue, no budget delta.
Applications writing faster than S3 can drain automatically get slowed
down without any code path hanging. No special-case error, just the
pre-deferred-writes behavior re-engaged for that one call.

**Per-handle `WRITE_BUFFER_MAX = 512 MB` stays** as a guard against a
single runaway file write (catches "one open file somehow buffered 5 GB"
as `EFBIG`). Different defense than the inflight budget — this one is
about bounding a single buffer, the inflight budget is about bounding
aggregate pending writes.

### 2.4 Failure modes and retry

**Retry policy: exponential backoff, unbounded.** Each PUT failure
logs (level=warn or error, includes path + error) and schedules a
retry with growing delay (100 ms → 500 ms → 2 s → 8 s → 30 s → 30 s
steady). No retry-count ceiling. A sustained S3 outage manifests as
the inflight queue growing, which shows up in host RAM consumption —
that's the operator signal for sustained failure, not an in-process
error count.

No "degraded filesystem" state, no special error surface. Production
monitoring on the host RAM watermark is how sustained failure gets
escalated. In-process, the filesystem keeps trying and keeps logging;
applications continue writing (hitting the backpressure path from
§2.3 once the inflight budget fills).

| Failure | Behavior |
|---|---|
| Worker panics mid-PUT | inflight entry stays, next worker picks it up |
| S3 returns 5xx or network failure | log, exponential backoff, retry indefinitely |
| Inflight queue grows faster than it drains | backpressure via sync-fallback (§2.3), host RAM rises, monitoring catches it |
| Process crashes with entries still inflight | DATA LOSS for those writes (fundamental async tradeoff; fsync opts out) |

The crash-loss window is the async-by-default tradeoff. Applications
that need durability call `fsync`; everything else benefits from
pooling. No synthetic retry count to tune, no in-process degraded-state
machinery — production observability catches real problems where they
belong.

---

## 3. Guest-wide read cache

Companion to §2. Replaces the current per-handle `data` / `read_ahead`
buffers with a cross-handle LRU cache.

### 3.1 Motivation

Current state:
- Each open handle carries its own `Option<Vec<u8>>` full-file buffer
  (for files ≤ 16 MB) or read-ahead chunk (for larger files)
- Closing a handle drops its buffer
- Opening the same file twice = two independent fetches from S3

For read-heavy workloads that re-open files (package managers scanning
manifests, shells walking $PATH, editors re-reading files), the
re-fetch cost adds up.

### 3.2 Design

```rust
struct ReadCache {
    entries: Mutex<LruCache<String, Arc<Vec<u8>>>>,
    current_bytes: AtomicUsize,
    budget_bytes: usize,  // default 100 MB
}
```

Values are `Arc<Vec<u8>>`. **Moving an entry to the head of the LRU
list on access is just pointer bookkeeping — the data stays where it
is, only the linked-list node moves.** No byte-copy on promotion.

**Flow.**

- `read()` checks inflight (§2) → checks read_cache → falls through to
  S3 GET; on miss-from-S3, populate read_cache with the fetched bytes
  before returning to the guest.
- On LRU eviction: subtract `entry.bytes.len()` from `current_bytes`,
  drop the Arc. If any caller still holds the Arc (a concurrent read
  in flight), data lingers until their refcount drops — correct
  lifetime without coordination.
- On `release()` (write complete): optionally insert into read_cache
  since the freshly-written bytes are likely to be read next. This
  couples §2 and §3: the Arc that was in the inflight map can move
  directly into the read cache after PUT completes.

### 3.3 Budget

Default 100 MB. Tunable via `FsConfig::read_cache_budget` (replacing
the current per-handle `read_cache_max: u64 = 16 MB` which is a
different concept).

For a 1 GB-memory VM, 100 MB of read cache is 10% of memory — generous
for interactive workloads, reasonable for batch. If a particular
workload needs more, configurable.

### 3.4 Invalidation

Single-writer (guest is only writer to `vm-<id>/`): cache entries stay
valid until the SAME filesystem writes to that path. On `release()`
with dirty data, just replace `read_cache[path]` with the fresh Arc —
subsequent reads see the new bytes, and if the old bytes were still
in the cache, dropping the cache's Arc decrements the refcount. If
anyone else is concurrently reading the old Arc (e.g., a read in
flight), their Arc keeps the old data alive until their read completes
— standard Rust lifetime semantics, no coordination.

No "dirty flag", no "can't evict" rule. The inflight map from §2 and
the read cache here both hold `Arc<Vec<u8>>` independently. If the
read cache evicts its entry LRU-style while the inflight map still
has its Arc (pending PUT), the bytes stay alive via inflight's
refcount. When the PUT completes and inflight removes its Arc, if
the read cache had already evicted, the data frees; if the read cache
still has it, the data stays until LRU evicts it normally.

Arc's refcount IS the state. No separate bookkeeping needed.

Future — external writes (orchestrator-pushed updates): invalidate
via direct cache access, same shape as inode-cache invalidation.
Doesn't exist today.

---

## 4. User's questions answered

- **"Who owns the memory while it's being written?"** The S3Fs
  instance, via the inflight map. `Arc<Vec<u8>>` means the handle can
  release and return to the guest while the worker still has the
  buffer reachable for the PUT.
- **"Guest fs thread could handle that?"** Yes — the FUSE-handler
  threads hand off to the worker pool via the inflight map; they don't
  block on I/O. The PUT itself runs on the worker pool, using the
  shared Agent.
- **"Pop last accessed to the top — that's just a pointer in Rust
  right, not a full copy?"** Correct. `Arc<Vec<u8>>` values; the LRU
  moves the linked-list node (a few pointer writes). The underlying
  `Vec<u8>` stays where it was allocated. Zero byte-copy on promotion.

---

## 5. Open items / deferred

- **Setattr persistence.** Still broken (research/07 §10c). Separate
  fix, not a usability-sprint scope item per se, but interacts with
  small-file write performance (chmod during install becomes a key-
  rewrite which is currently silent-no-op).
- **Rename non-atomicity.** Still filed (research/12 §9 + s3fs.rs inline
  comment). npm installs do many renames; the current non-atomic
  implementation is a correctness concern under failure. Needs
  fault-injection test infrastructure to gate the fix.
  - **Observed symptom** (vm:2.5 2026-04-13, post-12df82d): in-VM
    `git clone` of nvm reliably fails with "invalid config file
    /root/.nvm/.git/config, fatal: could not set core.bare to false".
    Strong smell of the rename gap: git writes `.git/config.lock`
    and atomically renames to `.git/config`; if the rename observes
    a half-written body, git's config parser trips on the partial
    content. Workload exposed by the bench run, not introduced by
    deferred writes (rename was already non-atomic). Route to the
    fault-injection track, not a deferred-writes fix.
- **Convention-based staging.** Research/10 §4 suggested per-path
  dispatch where caches live in RAM-only and never persist to S3 (e.g.,
  `~/.npm/_cacache`, `~/.cache/pip`). Big win for npm/pip if the guest
  is configured to use the conventional paths. Implement after deferred
  writes are in — until that, the performance math changes the picture.
- **Exec semantics debugging.** §1.3 — investigate whether execve works
  on FUSE-served newly-written binaries. Strace-driven diagnosis.
- **Baseline rootfs package audit.** §1.4 — revisit PACKAGES list with
  fresh eyes on what agents actually need.

---

## 6. Implementation ordering (proposed)

1. **Inflight map + worker pool + read-your-writes** (§2.1–2.2) — core of
   the deferred-write feature. Measurable perf win on small-file
   workloads.
2. **Inflight budget + fsync synchronization** (§2.3) — backpressure
   for pathological cases + durability opt-in.
3. **Read cache** (§3) — cross-handle LRU, coupled with the inflight
   map via shared Arcs.
4. **Benchmarks.** `npm install` wall-clock improvement as the headline
   number, same methodology as G3 (N=5, median + range). fsync latency
   as a secondary check.
5. **Deferred items from §5** as separate follow-ups.

Details TBD once the core lands and we see real numbers.
