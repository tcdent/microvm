// S3-backed filesystem — implements fuse-backend-rs FileSystem trait.
//
// Key format: path\x01<mode_hex><uid_hex><gid_hex>
// Metadata is embedded in the key, so a single LIST tells us type + perms.
// Symlink targets stored as object body.
//
// Write strategy: data buffered per file handle, flushed to S3 on close.
// TODO: periodic flush for long-lived files, fsync support.

use std::collections::HashMap;
use std::ffi::CStr;
use std::io;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::debug;

use fuse_backend_rs::abi::fuse_abi::{stat64, statvfs64, FsOptions, OpenOptions};
use fuse_backend_rs::api::filesystem::{Context, DirEntry, Entry, FileSystem};

use crate::blob_cache::{
    is_golden_ino, load_golden, GoldenBlobCache, GoldenEntry, GoldenInodeCache,
};
use crate::config::FsConfig;
use crate::inode_cache::{CachedInode, InodeCache};
use crate::keys::{self, KeyMeta};
use crate::read_ahead::ReadAheadState;
use crate::read_cache::{self, ReadCache};
use crate::ruleset::{Durability, HandleRules, Ruleset};
use crate::s3::{S3Client, S3ResultExt};
use crate::storage::{self, ObjectBody};
use crate::write_pool::{WritePool, WritePoolConfig};
use crate::log;

/// Cache statistics for observability and testing.
pub struct CacheStats {
    pub inode_count: usize,
    pub negative_count: usize,
    pub hits: u64,
    pub misses: u64,
}
/// Kernel FUSE attribute + entry cache TTL.
///
/// Zero: the kernel must revalidate every lookup and stat by calling
/// back into us. With our own caching layers (inode cache, read cache,
/// inflight map, golden blob cache) a HashMap lookup is cheaper than
/// a stale-window correctness bug. A nonzero TTL (even 1s) breaks
/// write-then-read visibility: release() updates our caches but the
/// kernel serves the old size/attrs from its own cache until TTL
/// expires, so `echo data > file; cat file` returns empty if the
/// sequence completes within the window.
///
/// Cost of TTL=0 on virtio-fs is negligible — the FUSE round-trip is
/// a shared-memory virtqueue dispatch (~µs), not a wire protocol hop.
const TTL: Duration = Duration::ZERO;
/// Number of S3 keys to fetch per lookup LIST probe.
/// Includes the target entry + neighbors for cache pre-fill.
pub const LIST_PROBE_MAX_KEYS: u32 = 12;
/// Maximum write buffer per file handle (512MB).
const WRITE_BUFFER_MAX: usize = 512 * 1024 * 1024;
// Default read cache / read-ahead sizes live in `FsConfig::default` —
// 16MB / 16MB. Benchmarks (research/07): 4MB=123MB/s, 16MB=176MB/s,
// 64MB=172MB/s, 128MB=243MB/s — 16MB is the knee.
//
// Hardlink key/body helpers and the UUID generator live in
// `crate::storage`. This file just calls them.

/// Reported filesystem size (1TB). S3 is effectively unlimited;
/// this just needs to be large enough that tools like apt don't
/// refuse to write.
const FS_TOTAL_BLOCKS: u64 = 1024 * 1024 * 1024; // 1G blocks × 4 KB = 4 TB

/// Write `data[offset..offset+size]` into the FUSE zero-copy
/// writer, clamping to the buffer length. Shared between every
/// cache-hit branch of [`S3Fs::resolve_read`] so offset/size math
/// doesn't get duplicated (or subtly diverged) across four sites.
fn serve_slice(
    w: &mut dyn fuse_backend_rs::api::filesystem::ZeroCopyWriter,
    data: &[u8],
    offset: u64,
    size: u32,
) -> io::Result<usize> {
    let off = offset as usize;
    if off >= data.len() {
        return Ok(0);
    }
    let end = off.saturating_add(size as usize).min(data.len());
    w.write(&data[off..end])
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

/// Where an inode lives after per-VM shadowing is taken into
/// account. Returned by [`S3Fs::resolve_inode`] and consumed by
/// every golden-aware FUSE op. Clones out of the cache rather than
/// holding borrows — the data is small and short-lived.
enum InodeSource {
    /// Per-VM inode: either originally assigned by per-VM
    /// InodeCache, or a golden ino whose path has since been
    /// materialized (per-VM cache shadows the golden entry).
    PerVm { path: String, cached: CachedInode },
    /// Golden inode with no per-VM shadow.
    Golden { path: String, entry: GoldenEntry },
}

pub struct S3Fs {
    /// Per-VM bucket S3 client. `Arc` so the write worker pool can
    /// hold its own reference — the client is `Sync` and every worker
    /// shares the underlying `ureq::Agent` pool.
    client: Arc<S3Client>,
    cache: Mutex<InodeCache>,
    handles: Mutex<HandleTable>,
    /// Cross-handle read cache (research/13 §3). Keyed by visible
    /// path; holds `Arc<Vec<u8>>` values. Replaces the old per-handle
    /// `data`/`read_cached` buffer for small/medium files.
    read_cache: ReadCache,
    /// Deferred-write queue + worker pool (research/13 §2).
    /// `release()` enqueues dirty buffers; workers drain to S3 in
    /// the background. `fsync()` blocks until the named path drains.
    write_pool: Arc<WritePool>,
    /// Handles for the spawned worker threads. Kept so `Drop` can
    /// join them cleanly on shutdown. Wrapped in `Mutex<Option<_>>`
    /// because `Drop` needs to take them out by value; `Option::take`
    /// gives us that without making the field itself `mut`.
    write_threads: Mutex<Option<Vec<JoinHandle<()>>>>,
    /// Shared golden inode cache. When the golden layer is disabled,
    /// this points at [`GoldenInodeCache::empty`] so every lookup
    /// path can consult it unconditionally without a config branch.
    /// `Arc` so downstream orchestrator code can share one cache
    /// across every `S3Fs` on the host (research/12).
    golden_inodes: Arc<GoldenInodeCache>,
    /// Shared golden blob cache. Same sentinel pattern as
    /// `golden_inodes` when the feature is disabled.
    golden_blobs: Arc<GoldenBlobCache>,
    /// Compiled per-workload ruleset (research/14 §1). Resolved
    /// once per `open()`/`create()` and cached on the handle.
    /// Empty ruleset when `FsConfig::ruleset_path` is `None`.
    ruleset: Ruleset,
}

struct HandleTable {
    by_handle: HashMap<u64, HandleEntry>,
    next_handle: u64,
}

struct HandleEntry {
    inode: u64,
    dirty: bool,
    /// Write buffer OR materialize-on-write preload for golden.
    /// For pure read handles, stays `None` — reads go through the
    /// cross-handle [`ReadCache`] and inflight map now.
    data: Option<Vec<u8>>,
    /// Dynamic read-ahead state (research/14 §4). Per-fd streak
    /// detection + prefetch chunk. Replaces the static read_ahead +
    /// read_ahead_offset fields.
    ra: ReadAheadState,
    /// Per-handle rules resolved from the compiled ruleset at
    /// `open()`/`create()` time (research/14 §1). Consulted by
    /// read/write/release without further glob matching.
    rules: HandleRules,
}

impl HandleTable {
    fn new() -> Self {
        Self { by_handle: HashMap::new(), next_handle: 1 }
    }

    fn open(&mut self, inode: u64, writing: bool, rules: HandleRules) -> u64 {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.by_handle.insert(handle, HandleEntry {
            inode,
            dirty: false,
            ra: ReadAheadState::new(rules.readahead),
            rules,
            data: if writing { Some(Vec::new()) } else { None },
        });
        handle
    }

    fn get_mut(&mut self, handle: u64) -> Option<&mut HandleEntry> {
        self.by_handle.get_mut(&handle)
    }

    fn remove(&mut self, handle: u64) -> Option<HandleEntry> {
        self.by_handle.remove(&handle)
    }
}

impl S3Fs {
    /// Construct from an [`FsConfig`].
    ///
    /// This is the only constructor. Callers who want defaults use
    /// `S3Fs::new(FsConfig::default())`; callers who want to tweak a
    /// single knob use struct-update syntax, e.g.
    /// `S3Fs::new(FsConfig { read_cache_max: 8 * MB, ..Default::default() })`.
    pub fn new(cfg: FsConfig) -> Self {
        let client = Arc::new(S3Client::new(cfg.s3.clone()));

        // Load the golden layer synchronously at construction if
        // enabled. A failure here is an orchestrator-level invariant
        // violation (manifest missing, bucket unreachable, schema
        // mismatch) — panicking at init is correct: running with a
        // partially-loaded golden layer would silently serve stale
        // data. The sentinel branch for the disabled case is
        // branchless at every downstream lookup.
        let (golden_inodes, golden_blobs) = match &cfg.golden {
            Some(golden_cfg) => load_golden(golden_cfg, &client).unwrap_or_else(|e| {
                panic!(
                    "S3Fs::new: failed to load golden layer from {:?}: {e}",
                    golden_cfg.bucket
                )
            }),
            None => (
                Arc::new(GoldenInodeCache::empty()),
                Arc::new(GoldenBlobCache::empty()),
            ),
        };

        // Spawn the deferred-write worker pool. Workers hold their
        // own Arc<S3Client> clones of the one above — same pool,
        // concurrent PUTs. Must start BEFORE returning so the
        // `write_threads` field is populated by the time any FUSE
        // op can call `release`.
        let (write_pool, write_threads) = WritePool::start(WritePoolConfig {
            workers: cfg.write_workers,
            budget_bytes: cfg.write_inflight_budget,
            ephemeral_dir: cfg.ephemeral_dir.clone(),
            client: Arc::clone(&client),
        });

        // Load the workload ruleset (research/14 §1). Empty ruleset
        // when no path is configured — every path gets defaults.
        let ruleset = match &cfg.ruleset_path {
            Some(path) => Ruleset::from_toml(path).unwrap_or_else(|e| {
                panic!("S3Fs::new: failed to load ruleset from {}: {e}", path.display())
            }),
            None => Ruleset::empty(),
        };
        if !ruleset.is_empty() {
            crate::log!("S3Fs: loaded {} ruleset rules", ruleset.len());
        }

        S3Fs {
            client,
            cache: Mutex::new(InodeCache::new()),
            handles: Mutex::new(HandleTable::new()),
            read_cache: ReadCache::new(cfg.read_cache_budget),
            write_pool,
            write_threads: Mutex::new(Some(write_threads)),
            golden_inodes,
            golden_blobs,
            ruleset,
        }
    }

    /// Cache statistics for observability and testing.
    pub fn cache_stats(&self) -> CacheStats {
        let cache = self.cache.lock().unwrap();
        CacheStats {
            inode_count: cache.inode_count(),
            negative_count: cache.negative_count(),
            hits: cache.hits,
            misses: cache.misses,
        }
    }

    /// Block until all deferred writes have drained to S3. Call
    /// before dropping to avoid data loss on abandoned writes. Also
    /// usable as a `syncfs()` primitive.
    pub fn sync_all(&self) {
        self.write_pool.wait_for_all();
    }
}

impl Drop for S3Fs {
    /// Drain-then-shutdown: wait for every pending write to flush
    /// to S3 before joining worker threads. This is the clean-exit
    /// path; callers that need a fast abort (unit tests with broken
    /// S3, process receiving SIGKILL) don't reach here at all — the
    /// "process crash = data loss" tradeoff from research/13 §2.4
    /// applies to those cases.
    ///
    /// If S3 is persistently unavailable, `sync_all` blocks until
    /// either S3 recovers or the process is killed externally.
    /// That's correct — Drop's contract is "make durable before
    /// giving up references"; opting out belongs at a higher layer.
    fn drop(&mut self) {
        self.write_pool.wait_for_all();
        self.write_pool.shutdown();
        if let Some(threads) = self.write_threads.lock().unwrap().take() {
            for h in threads {
                let _ = h.join();
            }
        }
    }
}

impl S3Fs {

    fn stat_for(ino: u64, entry: &CachedInode) -> stat64 {
        // SAFETY: stat64 is a C struct of integers + padding. Zeroing is
        // the idiomatic, portable initialization; we explicitly set every
        // field we care about below. The remaining padding/reserved fields
        // stay zero, which is the convention FUSE consumers expect.
        let mut st: stat64 = unsafe { std::mem::zeroed() };
        st.st_ino = ino;
        st.st_mode = entry.meta.mode;
        st.st_nlink = if entry.meta.file_type() == libc::S_IFDIR as u32 { 2 } else { 1 };
        st.st_size = entry.size as i64;
        st.st_uid = entry.meta.uid;
        st.st_gid = entry.meta.gid;
        st.st_blksize = 4096;
        st.st_blocks = ((entry.size + 511) / 512) as i64;
        st
    }

    /// Build a `stat64` from a [`GoldenEntry`]. Parallel to
    /// [`stat_for`] for per-VM inodes; kept separate because the two
    /// entry types have different field shapes and a shared helper
    /// would need dynamic dispatch to pay for.
    fn stat_for_golden(entry: &GoldenEntry) -> stat64 {
        // SAFETY: same as `stat_for`.
        let mut st: stat64 = unsafe { std::mem::zeroed() };
        st.st_ino = entry.ino;
        st.st_mode = entry.mode;
        let is_dir = (entry.mode & libc::S_IFMT as u32) == libc::S_IFDIR as u32;
        st.st_nlink = if is_dir { 2 } else { 1 };
        st.st_size = entry.size as i64;
        st.st_uid = entry.uid;
        st.st_gid = entry.gid;
        st.st_blksize = 4096;
        st.st_blocks = ((entry.size + 511) / 512) as i64;
        st
    }

    fn entry_for_golden(entry: &GoldenEntry) -> Entry {
        Entry {
            inode: entry.ino,
            generation: 0,
            attr: Self::stat_for_golden(entry),
            attr_flags: 0,
            attr_timeout: TTL,
            entry_timeout: TTL,
        }
    }

    /// Resolve a FUSE-supplied inode back to its source of truth,
    /// taking per-VM shadowing into account.
    ///
    /// Crucial invariant (research/12 G1 pushback 2): if a golden
    /// inode has been materialized (write + release populated the
    /// per-VM cache at the same path), subsequent ops on that inode
    /// must serve per-VM content, not stale golden. The kernel's
    /// dentry cache may still hold the golden ino long after
    /// materialization; this function makes that "zombie ino"
    /// transparently correct — golden path → look in per-VM cache
    /// first → fall through to golden only if per-VM doesn't shadow.
    fn resolve_inode(&self, ino: u64) -> Option<InodeSource> {
        if is_golden_ino(ino) {
            // Golden ino: resolve to path via the (immutable) golden
            // cache, then check for per-VM shadow before serving.
            let path = {
                let (path, _) = self.golden_inodes.get_by_ino(ino)?;
                path.to_string()
            };
            let cache = self.cache.lock().unwrap();
            if let Some(shadowed) = cache.get_by_path(&path) {
                // Per-VM cache has a materialized entry at this path.
                // The per-VM inode number is different from the golden
                // one we were handed, but the content source is
                // per-VM — we serve that and the caller's ino is just
                // a handle into the shadowed entry.
                return Some(InodeSource::PerVm {
                    path,
                    cached: shadowed.clone(),
                });
            }
            drop(cache);
            // No shadow — serve from the golden entry. We re-fetch
            // the entry here rather than holding a reference across
            // the lock drop above; GoldenEntry is small (no large
            // owned allocations) and clone is cheap.
            let gentry = self.golden_inodes.get(&path)?.clone();
            Some(InodeSource::Golden { path, entry: gentry })
        } else {
            // Per-VM ino: existing path.
            let cache = self.cache.lock().unwrap();
            let (path, cached) = cache.get_by_ino(ino)?;
            Some(InodeSource::PerVm {
                path: path.to_string(),
                cached: cached.clone(),
            })
        }
    }

    /// Build the Entry response for a cached inode
    fn entry_for(ino: u64, cached: &CachedInode) -> Entry {
        Entry {
            inode: ino,
            generation: 0,
            attr: Self::stat_for(ino, cached),
            attr_flags: 0,
            attr_timeout: TTL,
            entry_timeout: TTL,
        }
    }

    /// Resolve path from parent inode + child name
    fn child_path(&self, parent: u64, name: &str) -> io::Result<String> {
        // Resolve parent → parent_path. Golden-aware: a golden parent
        // ino (bit 63 set) comes from the golden inode cache; all
        // other inos come from the per-VM cache as before.
        let parent_path = if is_golden_ino(parent) {
            let (p, _) = self.golden_inodes.get_by_ino(parent)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            p.to_string()
        } else {
            let cache = self.cache.lock().unwrap();
            let (p, _) = cache.get_by_ino(parent)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            p.to_string()
        };
        if parent_path.is_empty() {
            Ok(name.to_string())
        } else {
            Ok(format!("{}/{}", parent_path, name))
        }
    }

    /// `name.to_str()` + `child_path` in one step. Called from every
    /// entry-creating / entry-mutating op (create, mkdir, unlink, rename,
    /// symlink, link, rmdir, mknod), which otherwise duplicate the same
    /// two-line preamble and diverge subtly in error mapping.
    fn resolve_child(&self, parent: u64, name: &CStr) -> io::Result<String> {
        let name_str = name.to_str()
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        self.child_path(parent, name_str)
    }

    /// Look up an inode and return its `(path, meta)`. ENOENT if missing.
    /// Used by readlink / read / open / release / link / rename etc., all
    /// of which would otherwise lock the cache, call `get_by_ino`, clone
    /// out the path, read `meta`, drop the guard — seven copies of the
    /// same four lines.
    fn path_meta_for_inode(&self, ino: u64) -> io::Result<(String, KeyMeta)> {
        let cache = self.cache.lock().unwrap();
        let (p, e) = cache.get_by_ino(ino)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        Ok((p.to_string(), e.meta))
    }

    /// Variant of `path_meta_for_inode` that also returns the hardlink
    /// target UUID if set. read / open / release need all three to pick
    /// between `keys::encode(path, meta)` and `storage::hardlink_key(uuid, meta)`.
    fn path_meta_hl_for_inode(&self, ino: u64) -> io::Result<(String, KeyMeta, Option<String>)> {
        // Golden-aware: a golden ino whose path is NOT yet
        // shadowed resolves through the golden cache; a zombie
        // golden ino (post-materialize) resolves through per-VM.
        // Hardlink targets are a per-VM concept — golden entries
        // never have them, so the Option<String> is always None
        // on the Golden branch.
        match self.resolve_inode(ino) {
            Some(InodeSource::PerVm { path, cached }) => {
                Ok((path, cached.meta, cached.hardlink_target))
            }
            Some(InodeSource::Golden { path, entry }) => {
                let meta = KeyMeta::new(entry.mode, entry.uid, entry.gid);
                Ok((path, meta, None))
            }
            None => Err(io::Error::from_raw_os_error(libc::ENOENT)),
        }
    }

    /// Single source-branching method for reads.
    ///
    /// All lookup order for a read lives in one place, so every
    /// level of the cache stack is visible at one call site:
    ///
    /// 1. Handle-local buffer (write-your-writes + golden RDWR
    ///    materialize-on-write preload).
    /// 2. Handle-local read-ahead chunk (sequential-read stream
    ///    buffer, large files only).
    /// 3. Inflight write map (cross-handle read-your-writes — the
    ///    guest's `release` buffer still hasn't drained to S3).
    /// 4. Cross-handle [`ReadCache`] — LRU-promoted on hit.
    /// 5. [`GoldenInodeCache`]/[`GoldenBlobCache`] for golden inodes.
    /// 6. S3 GET. Small files populate the cross-handle read cache;
    ///    large files stream via the handle-local read-ahead buffer.
    ///
    /// Returns the byte count written to `w`, FUSE-style. `ENOENT`
    /// if the inode isn't known to any source.
    fn resolve_read(
        &self,
        inode: u64,
        handle: u64,
        offset: u64,
        size: u32,
        w: &mut dyn fuse_backend_rs::api::filesystem::ZeroCopyWriter,
    ) -> io::Result<usize> {
        // ── 1. Handle-local buffer ──────────────────────────────
        //
        // Covers (a) read-your-writes within the same handle
        // (the guest is filling a write buffer and reads it back
        // before release) and (b) golden RDWR materialize-on-write
        // preload (`open()` filled `entry.data` with the golden
        // bytes so subsequent writes can diverge from them).
        //
        // ── 2. Handle-local read-ahead ─────────────────────────
        //
        // Sequential-read streaming buffer for large files. The
        // check is `offset` inside the previously-fetched chunk;
        // satisfies contiguous reads from one GET's worth of bytes.
        {
            let mut handles = self.handles.lock().unwrap();
            if let Some(entry) = handles.by_handle.get_mut(&handle) {
                if let Some(ref data) = entry.data {
                    if !data.is_empty() {
                        return serve_slice(w, data, offset, size);
                    }
                }
                // Step 2: dynamic read-ahead chunk (research/14 §4).
                if let Some(slice) = entry.ra.try_serve(offset, size) {
                    let result = w.write(slice)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                    entry.ra.observe(offset, size);
                    return Ok(result);
                }
            }
        }

        // Resolve inode (shadow-aware: a golden ino post-
        // materialize comes back as PerVm).
        let source = self.resolve_inode(inode)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;

        // ── 5. Golden branch ─────────────────────────────────────
        //
        // Serve entirely from RAM via the blob cache. One HashMap
        // lookup + Arc clone + slice. No S3.
        let (path, meta, file_size, hl_uuid) = match source {
            InodeSource::Golden { path, entry: gentry } => {
                let Some(hash) = gentry.hash.as_ref() else {
                    // Non-regular golden inode (dir / symlink);
                    // POSIX read on these is undefined — 0 bytes.
                    debug!("golden read on non-blob inode ino={:#x} path={}", inode, path);
                    return Ok(0);
                };
                let bytes = self.golden_blobs.fetch(hash).ok_or_else(|| {
                    log!(
                        "S3Fs: golden blob miss for hash={} path={} — manifest/pool out of sync",
                        hash, path
                    );
                    io::Error::from_raw_os_error(libc::EIO)
                })?;
                return serve_slice(w, &bytes, offset, size);
            }
            InodeSource::PerVm { path, cached } => {
                (path, cached.meta, cached.size, cached.hardlink_target.clone())
            }
        };

        // ── 3. Inflight write map (cross-handle RYW) ─────────────
        //
        // A pending deferred write wins over anything cached or on
        // S3 — the bytes haven't drained yet, and a naive S3 GET
        // would return stale data (or ENOENT on fresh creates).
        if let Some(bytes) = self.write_pool.get(&path) {
            return serve_slice(w, &bytes, offset, size);
        }

        // ── 4. Cross-handle read cache ───────────────────────────
        //
        // Populated by prior reads of this path. LRU-promoted on hit.
        if let Some(bytes) = self.read_cache.fetch(&path) {
            return serve_slice(w, &bytes, offset, size);
        }

        // ── 6. S3 fetch ──────────────────────────────────────────
        //
        // Branch on file size: small enough to cache whole → single
        // GET + read_cache populate; too large → read-ahead chunk +
        // per-handle streaming buffer.
        let key = match hl_uuid {
            Some(ref uuid) => storage::hardlink_key(uuid, meta),
            None => keys::encode(&path, meta),
        };
        let cache_threshold =
            read_cache::per_file_threshold(self.read_cache.budget_bytes()) as u64;

        if file_size <= cache_threshold {
            log!("S3 GET read-cache {} ({} bytes, full file)", path, file_size);
            let raw = self.client.get(&key)
                .io_err(&format!("read GET {}", path))?;

            // Hardlink detection on cold reads via the visible key:
            // if the body starts with "hardlink:<uuid>", refetch
            // from `.hardlinks/<uuid>` and record the UUID so future
            // reads skip the double-hop.
            let data = if hl_uuid.is_some() {
                raw
            } else {
                match storage::parse_body(raw, libc::S_IFREG as u32) {
                    ObjectBody::Direct(bytes) => bytes,
                    ObjectBody::Hardlink(uuid) => {
                        log!("  detected hardlink ref: {} -> .hardlinks/{}", path, uuid);
                        let hl_key = storage::hardlink_key(&uuid, meta);
                        let actual = self.client.get(&hl_key)
                            .io_err(&format!("read GET hardlink data for {}", path))?;
                        self.cache.lock().unwrap().set_hardlink_target(&path, uuid);
                        actual
                    }
                    ObjectBody::Symlink(_) => {
                        unreachable!("parse_body(_, S_IFREG) returned Symlink")
                    }
                }
            };

            let shared = Arc::new(data);
            self.read_cache.insert(&path, Arc::clone(&shared));
            return serve_slice(w, &shared, offset, size);
        }

        // Large file: dynamic read-ahead (research/14 §4). Fetch
        // current_window() bytes starting at the requested offset,
        // serve the requested slice, stash the chunk on the handle's
        // ReadAheadState so the next contiguous read hits step (2).
        let window = {
            let handles = self.handles.lock().unwrap();
            handles.by_handle.get(&handle)
                .map(|e| e.ra.current_window())
                .unwrap_or(crate::read_ahead::AUTO_INITIAL_WINDOW)
        };
        // ReadAhead::Off produces window=0 — skip prefetch entirely,
        // fetch only the exact requested range.
        let fetch_size = if window > 0 { window } else { size as usize };
        let chunk_start = offset;
        let chunk_end = std::cmp::min(chunk_start + fetch_size as u64, file_size);
        log!(
            "S3 GET read-ahead {} offset={} size={} window={}",
            path,
            chunk_start,
            chunk_end - chunk_start,
            window
        );
        let ra_data = self.client.get_range(&key, chunk_start, chunk_end - 1)
            .io_err(&format!("read-ahead GET {}", path))?;

        let buf_end = std::cmp::min(size as usize, ra_data.len());
        let result = w.write(&ra_data[..buf_end])
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut handles = self.handles.lock().unwrap();
        if let Some(entry) = handles.get_mut(handle) {
            entry.ra.observe(offset, size);
            entry.ra.chunk = Some(ra_data);
            entry.ra.chunk_offset = chunk_start;
        }
        Ok(result)
    }

    /// Insert `(path, meta, size)` into the cache and build the FUSE
    /// `Entry` for the caller in one shot. Collapses the `insert_from_key`
    /// + `get_by_ino(ino).unwrap()` + `entry_for` dance that appears four
    /// times (create/mkdir/symlink/link). The unwrap is safe: we just
    /// inserted `ino` under the cache lock, and the guard is still held,
    /// so nothing can have removed it.
    fn insert_and_entry(
        cache: &mut InodeCache,
        path: &str,
        meta: KeyMeta,
        size: u64,
    ) -> (u64, Entry) {
        let ino = cache.insert_from_key(path, meta, size);
        let cached = cache.get_by_ino(ino)
            .expect("just-inserted ino missing under held cache lock");
        (ino, Self::entry_for(ino, cached.1))
    }

    /// Lookup a path via LIST probe, populate cache with results.
    ///
    /// Strategy: LIST the parent directory with max-keys=12 to pre-fill
    /// siblings. If the target isn't in the first 12 results (large dir,
    /// target sorts late), fall back to a precise single-key probe.
    fn probe_and_cache(&self, path: &str) -> io::Result<u64> {
        // Phase 1: broad LIST at parent directory level for pre-fill
        let parent_prefix = match path.rfind('/') {
            Some(pos) => &path[..=pos], // "usr/bin/" for "usr/bin/bash"
            None => "",                  // root children have no slash
        };

        debug!("S3 LIST prefill prefix={} max-keys={}", parent_prefix, LIST_PROBE_MAX_KEYS);
        let result = self.client.list_probe(parent_prefix, Some("/"), LIST_PROBE_MAX_KEYS)
            .io_err(&format!("list_probe prefill for {}", path))?;

        let mut cache = self.cache.lock().unwrap();
        let mut found_ino = None;
        let prefill_count = result.objects.len();

        for obj in &result.objects {
            if let Some((decoded_path, meta)) = keys::decode(&obj.key) {
                let ino = cache.insert_from_key(decoded_path, meta, obj.size);
                if decoded_path == path {
                    found_ino = Some(ino);
                }
            }
        }

        if let Some(ino) = found_ino {
            debug!("  lookup HIT in prefill: {} (cached {} siblings)", path, prefill_count - 1);
            return Ok(ino);
        }

        // If the LIST was not truncated, we saw the entire directory.
        // The target doesn't exist — no need for a precise probe.
        if !result.is_truncated {
            debug!("  lookup ENOENT: {} (dir complete, {} entries, negative cached)", path, prefill_count);
            cache.insert_negative(path);
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }

        debug!("  lookup MISS in prefill ({} results), falling back to precise probe", prefill_count);
        drop(cache);

        // Phase 2: precise probe — target wasn't in first 12 results (dir has >12 entries)
        let precise_prefix = keys::list_prefix(path);
        debug!("S3 LIST precise prefix={}\\x01 max-keys=1", path);
        let result = self.client.list_probe(&precise_prefix, None, 1)
            .io_err(&format!("list_probe precise for {}", path))?;

        let mut cache = self.cache.lock().unwrap();
        for obj in &result.objects {
            if let Some((decoded_path, meta)) = keys::decode(&obj.key) {
                let ino = cache.insert_from_key(decoded_path, meta, obj.size);
                if decoded_path == path {
                    found_ino = Some(ino);
                }
            }
        }

        if found_ino.is_none() {
            debug!("  lookup ENOENT: {} (negative cached)", path);
            cache.insert_negative(path);
        } else {
            debug!("  lookup HIT in precise probe: {}", path);
        }

        found_ino.ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
    }
}

impl FileSystem for S3Fs {
    type Inode = u64;
    type Handle = u64;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        // Enable features that improve throughput:
        // - BIG_WRITES: allows max_write up to 1MB (vs ~8KB default)
        // - MAX_PAGES: allows max_pages=256 for 1MB FUSE requests
        // Only enable what the kernel offered (intersection).
        let want = FsOptions::BIG_WRITES
            | FsOptions::MAX_PAGES;
        let enabled = capable & want;
        log!("S3Fs: init capable={:#x} enabled={:#x}", capable.bits(), enabled.bits());
        Ok(enabled)
    }

    fn getattr(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Option<Self::Handle>,
    ) -> io::Result<(stat64, Duration)> {
        match self.resolve_inode(inode) {
            Some(InodeSource::PerVm { cached, .. }) => {
                // For a materialized golden path, `inode` is the
                // original (golden) ino the kernel cached. We still
                // report the per-VM metadata at that ino — the
                // FUSE contract doesn't require inode identity to
                // match st_ino, and preserving the kernel's cached
                // ino avoids invalidating dentries mid-session.
                Ok((Self::stat_for(inode, &cached), TTL))
            }
            Some(InodeSource::Golden { entry, .. }) => {
                Ok((Self::stat_for_golden(&entry), TTL))
            }
            None => Err(io::Error::from_raw_os_error(libc::ENOENT)),
        }
    }

    fn lookup(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<Entry> {
        let path = self.resolve_child(parent, name)?;

        // Tier 1: per-VM cache. Hits first because single-writer
        // ownership of vm-<id> means a per-VM entry at a path
        // shadows any golden entry at the same path. Negative hits
        // end the lookup — they cover both "probed and missed" and
        // "unlinked a golden-inherited path" (research/12 §7).
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(entry) = cache.get(&path) {
                debug!("lookup CACHE HIT: {}", path);
                return Ok(Self::entry_for(entry.ino, entry));
            }
            if cache.is_negative(&path) {
                debug!("lookup NEGATIVE HIT: {}", path);
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
        }

        // Tier 2: golden inode cache. Lock-free read into an
        // Arc<immutable> — empty sentinel when golden is disabled,
        // so the branch is free in that case. We do NOT insert the
        // returned entry into the per-VM cache (research/12 §5):
        // mirroring would grow the per-VM cache unboundedly on
        // scans like `find /`; walking both tiers on every lookup
        // costs a second HashMap probe and keeps the delta-only
        // semantic honest.
        if let Some(gentry) = self.golden_inodes.get(&path) {
            debug!("lookup GOLDEN HIT: {} ino={:#x}", path, gentry.ino);
            return Ok(Self::entry_for_golden(gentry));
        }

        // Tier 3: S3 LIST probe against the per-VM bucket.
        debug!("lookup CACHE MISS: {}", path);
        let ino = self.probe_and_cache(&path)?;

        let cache = self.cache.lock().unwrap();
        let entry = cache.get_by_ino(ino)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?
            .1;
        Ok(Self::entry_for(ino, entry))
    }

    fn readdir(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Self::Handle,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        // Resolve parent_path via source-aware helper: per-VM and
        // golden directories both have readdir children, and both
        // may have siblings in the other tier (e.g. VM wrote a file
        // inside a golden-inherited directory).
        let parent_path = match self.resolve_inode(inode) {
            Some(InodeSource::PerVm { path, .. }) => path,
            Some(InodeSource::Golden { path, .. }) => path,
            None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
        };

        // LIST all children: prefix = "parent_path/" (no \x01, just path prefix)
        // We use the raw path prefix + "/" to get direct children
        let list_prefix = if parent_path.is_empty() {
            String::new()
        } else {
            format!("{}/", parent_path)
        };

        debug!("S3 LIST readdir prefix={} delimiter=/", list_prefix);
        let result = self.client.list(&list_prefix, Some("/"))
            .io_err(&format!("readdir list {}", list_prefix))?;
        debug!("  readdir got {} entries for {}", result.objects.len(), list_prefix);

        let mut idx: u64 = 0;
        // Track which child names we've emitted so golden entries
        // shadowed by per-VM deltas aren't duplicated in the listing.
        let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cache = self.cache.lock().unwrap();

        // Tier 1: per-VM children (existing behavior).
        for obj in &result.objects {
            if let Some((decoded_path, meta)) = keys::decode(&obj.key) {
                let child_name = decoded_path.strip_prefix(&list_prefix)
                    .unwrap_or(decoded_path);
                if child_name.is_empty() || child_name.contains('/') {
                    continue; // skip self or grandchildren
                }
                if storage::is_reserved_name(child_name) {
                    continue;
                }

                // Populate cache
                let ino = cache.insert_from_key(decoded_path, meta, obj.size);
                emitted.insert(child_name.to_string());

                if idx >= offset {
                    let d_type = match meta.file_type() {
                        t if t == libc::S_IFDIR as u32 => libc::DT_DIR,
                        t if t == libc::S_IFLNK as u32 => libc::DT_LNK,
                        _ => libc::DT_REG,
                    };

                    match add_entry(DirEntry {
                        ino,
                        offset: idx + 1,
                        type_: d_type as u32,
                        name: child_name.as_bytes(),
                    }) {
                        Ok(0) => return Ok(()),
                        Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                }
                idx += 1;
            }
        }

        // Tier 2: golden children not shadowed by per-VM delta and
        // not hidden by a sticky negative. Walk the pre-built
        // by_parent index — lock-free read from the Arc.
        for child_name in self.golden_inodes.children(&parent_path) {
            if emitted.contains(child_name) {
                continue; // already emitted from per-VM
            }
            // Build the full path to consult the negative cache.
            let full_path = if parent_path.is_empty() {
                child_name.clone()
            } else {
                format!("{}/{}", parent_path, child_name)
            };
            if cache.is_negative(&full_path) {
                continue; // unlinked golden path (research/12 §7)
            }
            let gentry = match self.golden_inodes.get(&full_path) {
                Some(e) => e,
                None => continue, // by_parent drifted from by_path? skip defensively
            };

            if idx >= offset {
                let d_type = match gentry.mode & libc::S_IFMT as u32 {
                    t if t == libc::S_IFDIR as u32 => libc::DT_DIR,
                    t if t == libc::S_IFLNK as u32 => libc::DT_LNK,
                    _ => libc::DT_REG,
                };
                match add_entry(DirEntry {
                    ino: gentry.ino,
                    offset: idx + 1,
                    type_: d_type as u32,
                    name: child_name.as_bytes(),
                }) {
                    Ok(0) => return Ok(()),
                    Ok(_) => {}
                    Err(e) => return Err(e),
                }
            }
            idx += 1;
        }

        Ok(())
    }

    fn readlink(&self, _ctx: &Context, inode: Self::Inode) -> io::Result<Vec<u8>> {
        // Golden-aware: symlink targets for golden inodes live
        // inline on the manifest entry, no S3 fetch needed. Per-VM
        // shadow (if any) takes precedence via resolve_inode.
        match self.resolve_inode(inode) {
            Some(InodeSource::Golden { path, entry }) => {
                return match entry.symlink_target {
                    Some(target) => {
                        debug!("readlink GOLDEN: {} -> {}", path, target);
                        Ok(target.into_bytes())
                    }
                    None => Err(io::Error::from_raw_os_error(libc::EINVAL)),
                };
            }
            Some(InodeSource::PerVm { path, cached }) => {
                if let Some(target) = cached.symlink_target {
                    debug!("readlink CACHE HIT: {} -> {}", path, target);
                    return Ok(target.into_bytes());
                }
                // Fall through to the S3 fetch below with the
                // resolved per-VM (path, meta).
            }
            None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
        }

        // Cache miss — fetch symlink target from S3 (stored as object body)
        let (path, meta) = self.path_meta_for_inode(inode)?;

        let key = keys::encode(&path, meta);
        log!("S3 GET readlink {}", path);
        let data = self.client.get(&key)
            .io_err(&format!("readlink GET {}", path))?;

        // By symlink convention the body IS the target path — parse_body
        // with S_IFLNK encodes that rule in one place. Pattern-matches
        // only Symlink because our file_type is S_IFLNK; the other
        // variants aren't reachable here but we handle them defensively
        // so future changes to parse_body can't silently break us.
        let target = match storage::parse_body(data, libc::S_IFLNK as u32) {
            ObjectBody::Symlink(t) => t,
            _ => return Err(io::Error::from_raw_os_error(libc::EIO)),
        };

        // Cache it
        {
            let mut cache = self.cache.lock().unwrap();
            cache.set_symlink_target(&path, target.clone());
        }

        Ok(target.into_bytes())
    }

    fn read(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        handle: Self::Handle,
        w: &mut dyn fuse_backend_rs::api::filesystem::ZeroCopyWriter,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        // Thin wrapper — all source branching lives in `resolve_read`
        // so every lookup order (handle buffer → inflight → read_cache
        // → golden → S3) is visible at one call site.
        self.resolve_read(inode, handle, offset, size, w)
    }

    fn open(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions, Option<u32>)> {
        let writing = (flags & libc::O_ACCMODE as u32) != libc::O_RDONLY as u32;

        // Resolve per-path rules from the compiled ruleset (once
        // per open, cached on the handle for the lifetime of the FD).
        let rules = match self.resolve_inode(inode) {
            Some(InodeSource::PerVm { ref path, .. }) |
            Some(InodeSource::Golden { ref path, .. }) => self.ruleset.resolve(path),
            None => HandleRules::default(),
        };

        let mut handles = self.handles.lock().unwrap();
        let handle = handles.open(inode, writing, rules);

        let truncating = (flags & libc::O_TRUNC as u32) != 0;

        // Materialize-on-write: a golden-inherited file opened for
        // write needs its current content in the handle buffer
        // before guest writes can diverge it. We preload from the
        // shared GoldenBlobCache (one HashMap lookup + Arc clone +
        // Vec copy — no S3). `release` later writes the buffer to
        // the per-VM bucket at the visible path; from that moment
        // on, the per-VM cache shadows the golden entry and
        // resolve_inode routes subsequent reads per-VM.
        //
        // A golden ino whose path is already materialized would
        // come back from resolve_inode as PerVm and fall through to
        // the normal per-VM preload path below — same code handles
        // both zombie golden inos and freshly-opened per-VM inos.
        if writing && !truncating && is_golden_ino(inode) {
            if let Some(InodeSource::Golden { path, entry }) = self.resolve_inode(inode) {
                if let Some(ref hash) = entry.hash {
                    if let Some(bytes) = self.golden_blobs.fetch(hash) {
                        log!("golden open-preload {} ({} bytes, materialize-on-write)", path, bytes.len());
                        // Own a Vec — the handle buffer is writable
                        // and will diverge from the Arc-shared bytes
                        // imminently. Cheap one-shot copy.
                        if let Some(h) = handles.get_mut(handle) {
                            h.data = Some((*bytes).clone());
                        }
                    } else {
                        log!(
                            "S3Fs: golden open-preload blob miss hash={} path={} — invariant violation",
                            hash,
                            path
                        );
                    }
                }
                // Golden preload is complete (or deliberately skipped
                // for non-blob entries); don't fall through to the
                // per-VM preload path.
                return Ok((Some(handle), OpenOptions::empty(), None));
            }
        }

        if writing && !truncating {
            // Missing-inode is tolerated here: if the cache doesn't know
            // this inode (it should, but defensive), we just skip the
            // preload. `.ok().unwrap_or_default()` preserves that legacy
            // semantics while sharing the lookup helper with other ops.
            let (path, meta, hl_uuid) = self.path_meta_hl_for_inode(inode)
                .ok()
                .unwrap_or_default();

            if !path.is_empty() {
                let key = match hl_uuid {
                    Some(ref uuid) => storage::hardlink_key(uuid, meta),
                    None => keys::encode(&path, meta),
                };
                log!("S3 GET open-preload {}", path);
                if let Ok(raw) = self.client.get(&key) {
                    // If we already had hl_uuid in cache, `key` targeted
                    // the real data object and `raw` is the file content.
                    // If not, `raw` might be a hardlink ref body — parse
                    // to find out, mirroring the cold-path detection in
                    // read(). Without this, release() would write the
                    // corrupted-ref buffer back to the visible key and
                    // detach it from the shared data; see
                    // tests/test_correctness.rs::hardlink_write_through_append.
                    let data = if hl_uuid.is_some() {
                        Some(raw)
                    } else {
                        match storage::parse_body(raw, libc::S_IFREG as u32) {
                            ObjectBody::Direct(bytes) => Some(bytes),
                            ObjectBody::Hardlink(uuid) => {
                                log!("  open-preload detected hardlink ref: {} -> .hardlinks/{}", path, uuid);
                                // Record the hint so subsequent ops (this
                                // release, future reads) route through
                                // the hardlink_key() path directly.
                                self.cache.lock().unwrap()
                                    .set_hardlink_target(&path, uuid.clone());
                                // Re-fetch the actual data from the shared
                                // blob. If this fails we leave the handle
                                // buffer unset — same behavior as the
                                // outer `if let Ok(raw)` miss: writes
                                // still work, just without a preload.
                                let hl_key = storage::hardlink_key(&uuid, meta);
                                self.client.get(&hl_key).ok()
                            }
                            // Symlinks aren't opened for write; parse_body
                            // only returns Symlink when file_type=S_IFLNK.
                            ObjectBody::Symlink(_) => {
                                log!("S3Fs: open-preload unexpected Symlink body at {}", path);
                                None
                            }
                        }
                    };

                    if let Some(data) = data {
                        if let Some(entry) = handles.get_mut(handle) {
                            entry.data = Some(data);
                        }
                    }
                }
            }
        }

        Ok((Some(handle), OpenOptions::empty(), None))
    }

    fn create(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
        _args: fuse_backend_rs::abi::fuse_abi::CreateIn,
    ) -> io::Result<(Entry, Option<Self::Handle>, OpenOptions, Option<u32>)> {
        let path = self.resolve_child(parent, name)?;
        let meta = KeyMeta::new(libc::S_IFREG as u32 | 0o755, 0, 0);
        let key = keys::encode(&path, meta);

        let rules = self.ruleset.resolve(&path);

        // Ephemeral files never touch S3 — skip the initial empty PUT.
        // The inode cache entry still gets created so lookups work;
        // the data lives only in the inflight map after release.
        if rules.durability != Durability::Ephemeral {
            log!("S3 PUT create {}", path);
            self.client.put(&key, &[])
                .io_err(&format!("create PUT {}", path))?;
        } else {
            debug!("create EPHEMERAL {} (skipping S3 PUT)", path);
        }

        let (ino, fuse_entry) = {
            let mut cache = self.cache.lock().unwrap();
            Self::insert_and_entry(&mut cache, &path, meta, 0)
        };

        let mut handles = self.handles.lock().unwrap();
        let handle = handles.open(ino, true, rules);

        Ok((fuse_entry, Some(handle), OpenOptions::empty(), None))
    }

    fn write(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        handle: Self::Handle,
        r: &mut dyn fuse_backend_rs::api::filesystem::ZeroCopyReader,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<usize> {
        let mut handles = self.handles.lock().unwrap();
        let entry = handles.get_mut(handle)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;

        let data = entry.data.get_or_insert_with(Vec::new);
        let end = offset as usize + size as usize;
        if end > WRITE_BUFFER_MAX {
            return Err(io::Error::from_raw_os_error(libc::EFBIG));
        }
        if end > data.len() {
            data.resize(end, 0);
        }

        let n = r.read(&mut data[offset as usize..end])
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        entry.dirty = true;

        // Update size in cache
        let inode = entry.inode;
        let new_size = data.len() as u64;
        drop(handles);

        let mut cache = self.cache.lock().unwrap();
        // Snapshot path + inode under one borrow so the subsequent
        // cache.insert() doesn't need a second get_by_ino+unwrap. Cloning
        // the CachedInode is cheap (a few fields, no heavy allocs).
        let snapshot = cache.get_by_ino(inode)
            .map(|(p, c)| (p.to_string(), c.clone()));
        if let Some((path, mut cached)) = snapshot {
            cached.size = new_size;
            cache.insert(&path, cached);
        }

        Ok(n)
    }

    fn release(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _flags: u32,
        handle: Self::Handle,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        let entry = {
            let mut handles = self.handles.lock().unwrap();
            handles.remove(handle)
        };

        let Some(mut entry) = entry else { return Ok(()); };
        if !entry.dirty {
            return Ok(());
        }
        // Move the buffer out of the handle entry — we'll hand it to
        // the write pool (or PUT sync) and don't need it here after.
        let Some(data) = entry.data.take() else { return Ok(()); };

        // Missing-inode tolerated — if cache has lost the mapping
        // (shouldn't happen) we silently skip the PUT.
        let (path, meta, hl_uuid) = self
            .path_meta_hl_for_inode(entry.inode)
            .ok()
            .unwrap_or_default();
        if path.is_empty() {
            return Ok(());
        }

        // Hardlink writes go to the shared data object at
        // `.hardlinks/<uuid>`, not the visible key. The deferred-
        // write path doesn't special-case hardlinks — the inflight
        // map is keyed by visible PATH but carries whatever S3 key
        // the worker should PUT to.
        let key = match hl_uuid {
            Some(ref uuid) => storage::hardlink_key(uuid, meta),
            None => keys::encode(&path, meta),
        };
        let data_len = data.len() as u64;
        let shared = Arc::new(data);

        let durability = entry.rules.durability;

        // Try to enqueue for deferred PUT. If the inflight budget
        // would be exceeded, fall back to a synchronous inline PUT
        // — the caller waits, the queue doesn't grow. This is the
        // backpressure path. Ephemeral writes always succeed (they
        // skip S3 entirely — the inflight map IS their storage).
        let deferred = self.write_pool.enqueue(
            path.clone(),
            key.clone(),
            Arc::clone(&shared),
            durability,
            entry.rules.coalesce,
        )?;

        if deferred {
            debug!(
                "release DEFERRED {} ({} bytes, {:?}, inflight total={}B)",
                path,
                data_len,
                durability,
                self.write_pool.current_bytes()
            );
        } else {
            log!(
                "release SYNC-FALLBACK {} ({} bytes, inflight budget full)",
                path,
                data_len
            );
            self.client
                .put(&key, &shared)
                .io_err(&format!("release sync PUT {}", path))?;
        }

        // Per-VM inode cache is updated immediately, whether the
        // write deferred or went sync. Future lookups see the new
        // size without a fresh LIST probe, and materialize-on-write
        // semantics (golden → per-VM shadow) remain correct.
        // Hardlink path uses the ref body's size, unchanged from
        // pre-G1 behavior.
        if hl_uuid.is_none() {
            let mut cache = self.cache.lock().unwrap();
            cache.insert_from_key(&path, meta, data_len);
        }

        // Populate the read cache with the just-written bytes. The
        // freshly-written bytes are likely to be read next
        // (research/13 §3.2); sharing the Arc between inflight and
        // read cache costs nothing. Large files auto-skip per the
        // per-file threshold inside `insert`.
        if hl_uuid.is_none() {
            self.read_cache.insert(&path, shared);
        }

        Ok(())
    }

    fn mkdir(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
        _mode: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        let path = self.resolve_child(parent, name)?;
        let meta = KeyMeta::new(libc::S_IFDIR as u32 | 0o755, 0, 0);
        let key = keys::encode(&path, meta);

        let rules = self.ruleset.resolve(&path);
        if rules.durability != Durability::Ephemeral {
            log!("S3 PUT mkdir {}", path);
            self.client.put(&key, &[])
                .io_err(&format!("mkdir PUT {}", path))?;
        } else {
            debug!("mkdir EPHEMERAL {} (skipping S3 PUT)", path);
        }

        let mut cache = self.cache.lock().unwrap();
        let (_, entry) = Self::insert_and_entry(&mut cache, &path, meta, 0);
        Ok(entry)
    }

    fn unlink(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<()> {
        let path = self.resolve_child(parent, name)?;

        // Need the meta to build the S3 key. Also captures whether
        // per-VM owns this path — if not, we may be unlinking a
        // golden-inherited path (no S3 DELETE needed).
        let per_vm_meta = {
            let mut cache = self.cache.lock().unwrap();
            cache.get(&path).map(|e| e.meta)
        };

        // Invalidate any cached read bytes for this path. Concurrent
        // readers that still hold the Arc keep seeing the old data
        // until their refcount drops — standard Rust lifetime.
        self.read_cache.remove(&path);

        match per_vm_meta {
            Some(meta) => {
                // Per-VM owns this path: DELETE from vm-<id>. Also
                // insert a sticky negative — if the same path also
                // exists in golden, the negative keeps it hidden on
                // future lookups. Pure per-VM files pay one extra
                // HashSet insert; golden-materialized-then-deleted
                // files are covered.
                let key = keys::encode(&path, meta);
                log!("S3 DELETE unlink {}", path);
                self.client.delete(&key)
                    .io_err(&format!("unlink DELETE {}", path))?;

                let mut cache = self.cache.lock().unwrap();
                cache.remove(&path);
                cache.insert_negative(&path);
                Ok(())
            }
            None if self.golden_inodes.get(&path).is_some() => {
                // Golden-inherited path with no per-VM shadow: no
                // S3 DELETE (there's nothing in vm-<id> to remove),
                // just insert a sticky negative so future lookups
                // return ENOENT instead of the golden entry. The
                // negative lives for the VM's lifetime; no TTL.
                debug!("unlink GOLDEN {} (negative-cache only)", path);
                let mut cache = self.cache.lock().unwrap();
                cache.insert_negative(&path);
                Ok(())
            }
            None => Err(io::Error::from_raw_os_error(libc::ENOENT)),
        }
    }

    fn rename(
        &self,
        _ctx: &Context,
        olddir: Self::Inode,
        oldname: &CStr,
        newdir: Self::Inode,
        newname: &CStr,
        _flags: u32,
    ) -> io::Result<()> {
        let old_path = self.resolve_child(olddir, oldname)?;
        let new_path = self.resolve_child(newdir, newname)?;

        log!("S3 rename {} -> {}", old_path, new_path);

        // Get the old entry's metadata
        let (meta, is_dir) = {
            let mut cache = self.cache.lock().unwrap();
            match cache.get(&old_path) {
                Some(e) => (e.meta, e.meta.file_type() == libc::S_IFDIR as u32),
                None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        };

        if is_dir {
            // KNOWN BUG — directory rename is NOT atomic; see
            // tests/test_correctness.rs for the regression anchor and the
            // filed issue for the full failure modes. Summary:
            //
            // (a) The children-copy/delete loop below has no rollback.
            //     A network failure mid-loop leaves some children moved
            //     and others still at the old path, with no record of
            //     which is which.
            // (b) The directory marker itself (the zero-byte object at
            //     `old_path`) is renamed AFTER the children — if the
            //     marker rename fails, children are at `new_path/` but
            //     the marker is orphaned at `old_path`, which is worse
            //     than the reverse failure mode.
            // (c) Hardlink refs inside the directory happen to work
            //     because the ref body (`hardlink:<uuid>`) is just bytes
            //     that get copied verbatim — no special handling needed,
            //     but also no test coverage of that interaction.
            //
            // Known-better design: rename marker FIRST (cheap server-side
            // copy), then walk children with a tracked rollback list,
            // then delete the old marker. Deferred until we have
            // fault-injection test infrastructure to exercise the
            // partial-failure paths — otherwise a "fix" is just moving
            // the bug around.
            //
            // Drain all pending deferred writes before the copy loop.
            // server-side COPY reads whatever S3 has; if a child file
            // was just released and its PUT is still inflight, COPY
            // gets stale (empty) content while the real data sits in
            // the write pool. The deferred PUT then fires against the
            // OLD key, which gets deleted — data loss. wait_for_all
            // is broader than needed (drains ALL inflight, not just
            // children under old_prefix) but rename is already O(n)
            // S3 ops so the marginal cost is acceptable.
            self.write_pool.wait_for_all();

            // Directory rename: LIST all children, copy+delete each
            let old_prefix = format!("{}/", old_path);
            let result = self.client.list(&old_prefix, None)
                .io_err(&format!("rename LIST {}", old_prefix))?;

            // Build the work list: decode each key, compute old/new
            // keys. Errors in decode are skipped (same as before).
            let work: Vec<_> = result.objects.iter().filter_map(|obj| {
                let (decoded_path, child_meta) = keys::decode(&obj.key)?;
                let suffix = &decoded_path[old_prefix.len()..];
                let new_child_path = format!("{}/{}", new_path, suffix);
                let old_key = keys::encode(decoded_path, child_meta);
                let new_key = keys::encode(&new_child_path, child_meta);
                Some((
                    decoded_path.to_string(),
                    new_child_path,
                    old_key,
                    new_key,
                    child_meta,
                    obj.size,
                ))
            }).collect();

            // Parallel COPY+DELETE with bounded concurrency. Uses
            // std::thread::scope so threads borrow &self.client
            // without Arc overhead. 8 threads matches the write
            // worker count.
            const RENAME_CONCURRENCY: usize = 8;
            let first_error: Mutex<Option<io::Error>> = Mutex::new(None);

            std::thread::scope(|s| {
                let sem = Arc::new((Mutex::new(0usize), std::sync::Condvar::new()));
                for (old_decoded, new_child, old_key, new_key, _, _) in &work {
                    // Acquire semaphore slot.
                    let sem = Arc::clone(&sem);
                    {
                        let (lock, cvar) = &*sem;
                        let mut count = lock.lock().unwrap();
                        while *count >= RENAME_CONCURRENCY {
                            count = cvar.wait(count).unwrap();
                        }
                        *count += 1;
                    }

                    let client = &self.client;
                    let err_slot = &first_error;
                    s.spawn(move || {
                        let result = client.copy(old_key, new_key)
                            .and_then(|_| client.delete(old_key));
                        if let Err(e) = result {
                            let mut slot = err_slot.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(io::Error::new(
                                    io::ErrorKind::Other,
                                    format!("rename child {} -> {}: {}", old_decoded, new_child, e),
                                ));
                            }
                        }
                        let (lock, cvar) = &*sem;
                        let mut count = lock.lock().unwrap();
                        *count -= 1;
                        cvar.notify_one();
                    });
                }
            });

            // Propagate the first error if any child COPY failed.
            if let Some(e) = first_error.into_inner().unwrap() {
                return Err(e);
            }

            // Cache updates: sequential (cache is Mutex-protected).
            for (old_decoded, new_child_path, _, _, child_meta, size) in &work {
                self.read_cache.remove(old_decoded);

                let mut cache = self.cache.lock().unwrap();
                if let Some(mut entry) = cache.get(old_decoded).cloned() {
                    entry.size = *size;
                    cache.remove(old_decoded);
                    cache.insert(new_child_path, entry);
                } else {
                    cache.insert_from_key(
                        new_child_path,
                        *child_meta,
                        *size,
                    );
                }
            }
        } else {
            // Single-file rename: drain any pending deferred write for
            // this specific path before the server-side COPY. Same
            // race as the dir case — COPY reads stale S3 content if
            // the PUT hasn't drained.
            self.write_pool.wait_for_path(&old_path);
        }

        // Rename the entry itself.
        let old_key = keys::encode(&old_path, meta);
        let new_key = keys::encode(&new_path, meta);

        // Resolve durability from the RULESET, not inflight state.
        // Ephemeral entries may have been evicted or not yet released.
        let durability = self.ruleset.resolve(&old_path).durability;
        if durability == Durability::Ephemeral {
            // Ephemeral source: no S3 object. Move the inflight entry
            // (if present) to the new path+key. No S3 ops.
            log!("rename EPHEMERAL {} -> {}", old_path, new_path);
            self.write_pool.rename_path(&old_path, &new_path, new_key);
        } else {
            // Durable source: server-side S3 copy, no data through client.
            self.client.copy(&old_key, &new_key)
                .io_err(&format!("rename COPY {} -> {}", old_path, new_path))?;
            self.client.delete(&old_key)
                .io_err(&format!("rename DELETE {}", old_path))?;
        }

        // Read-cache invalidation: drop the old-path entry if any.
        // new_path won't hit the cache until someone reads it, at
        // which point we'll fetch from S3 fresh.
        self.read_cache.remove(&old_path);

        // Update cache
        let mut cache = self.cache.lock().unwrap();
        if let Some(entry) = cache.get(&old_path).cloned() {
            cache.remove(&old_path);
            cache.insert(&new_path, entry);
        }

        Ok(())
    }

    fn setattr(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        attr: stat64,
        _handle: Option<Self::Handle>,
        valid: fuse_backend_rs::abi::fuse_abi::SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        use fuse_backend_rs::abi::fuse_abi::SetattrValid;

        // SIZE: truncate the file (resize handle buffers + update cache).
        if valid.contains(SetattrValid::SIZE) {
            let new_size = attr.st_size as usize;
            log!("setattr truncate ino={} size={}", inode, new_size);

            let mut cache = self.cache.lock().unwrap();
            let snapshot = cache.get_by_ino(inode)
                .map(|(p, c)| (p.to_string(), c.clone()));
            if let Some((path, mut cached)) = snapshot {
                cached.size = new_size as u64;
                cache.insert(&path, cached);
            }
            drop(cache);

            let mut handles = self.handles.lock().unwrap();
            for entry in handles.by_handle.values_mut() {
                if entry.inode == inode {
                    if let Some(ref mut data) = entry.data {
                        if !data.is_empty() {
                            data.resize(new_size, 0);
                            entry.dirty = true;
                        }
                    }
                }
            }
        }

        // MODE / UID / GID: metadata is embedded in the S3 key, so
        // changing it requires re-encoding the key (COPY + DELETE).
        // Same pattern as rename — ephemeral paths just update the
        // inflight entry's key; durable paths do a server-side COPY.
        let meta_changed = valid.intersects(
            SetattrValid::MODE | SetattrValid::UID | SetattrValid::GID,
        );
        if meta_changed {
            let mut cache = self.cache.lock().unwrap();
            let snapshot = cache.get_by_ino(inode)
                .map(|(p, c)| (p.to_string(), c.clone()));
            if let Some((path, mut cached)) = snapshot {
                let old_meta = cached.meta;

                // Apply requested changes to the meta.
                if valid.contains(SetattrValid::MODE) {
                    // Preserve file type bits, take permission bits from attr.
                    let file_type = old_meta.mode & libc::S_IFMT as u32;
                    let perm_bits = attr.st_mode & !libc::S_IFMT as u32;
                    cached.meta = KeyMeta::new(file_type | perm_bits, cached.meta.uid, cached.meta.gid);
                }
                if valid.contains(SetattrValid::UID) {
                    cached.meta = KeyMeta::new(cached.meta.mode, attr.st_uid, cached.meta.gid);
                }
                if valid.contains(SetattrValid::GID) {
                    cached.meta = KeyMeta::new(cached.meta.mode, cached.meta.uid, attr.st_gid);
                }

                let new_meta = cached.meta;
                cache.insert(&path, cached);
                drop(cache);

                // Only re-key if the encoded key actually changed.
                if old_meta != new_meta {
                    let old_key = keys::encode(&path, old_meta);
                    let new_key = keys::encode(&path, new_meta);

                    // Resolve durability from the RULESET, not from
                    // inflight state. Ephemeral entries may have been
                    // evicted (budget pressure) or not yet released —
                    // the ruleset is the authoritative source.
                    let durability = self.ruleset.resolve(&path).durability;
                    if durability == Durability::Ephemeral {
                        // Ephemeral: no S3 object to re-key. Update
                        // the inflight entry's key if present.
                        self.write_pool.rename_path(&path, &path, new_key);
                    } else if self.write_pool.get(&path).is_some() {
                        // Durable inflight: update the key the worker
                        // will PUT to. Don't touch S3 — the old key's
                        // content will be superseded by the inflight PUT.
                        self.write_pool.rename_path(&path, &path, new_key);
                        let _ = self.client.delete(&old_key);
                    } else {
                        // No inflight: re-key via S3 COPY + DELETE.
                        log!("setattr re-key {} (mode {:o} -> {:o})", path,
                             old_meta.mode, new_meta.mode);
                        self.write_pool.wait_for_path(&path);
                        self.client.copy(&old_key, &new_key)
                            .io_err(&format!("setattr COPY {}", path))?;
                        self.client.delete(&old_key)
                            .io_err(&format!("setattr DELETE {}", path))?;
                    }
                    // Invalidate read cache — the old key is gone.
                    self.read_cache.remove(&path);
                }
            }
        }

        // ATIME / MTIME / CTIME: silently ignored. S3 has no mutable
        // timestamp concept. FUSE mount tools (apt, chmod, touch) don't
        // hard-fail when these are dropped.

        let cache = self.cache.lock().unwrap();
        let (_, entry) = cache.get_by_ino(inode)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        Ok((Self::stat_for(inode, entry), TTL))
    }

    fn opendir(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        Ok((Some(0), OpenOptions::empty()))
    }

    fn statfs(&self, _ctx: &Context, _inode: Self::Inode) -> io::Result<statvfs64> {
        // SAFETY: statvfs64 is a C struct of integers + padding; zeroed
        // initialization is idiomatic and every meaningful field is set
        // explicitly below.
        let mut st: statvfs64 = unsafe { std::mem::zeroed() };
        st.f_bsize = 4096;
        st.f_frsize = 4096;
        st.f_blocks = FS_TOTAL_BLOCKS;
        st.f_bfree = FS_TOTAL_BLOCKS;
        st.f_bavail = FS_TOTAL_BLOCKS;
        st.f_files = 1_000_000;
        st.f_ffree = 1_000_000;
        st.f_namemax = 255;
        Ok(st)
    }

    fn symlink(
        &self,
        _ctx: &Context,
        linkname: &CStr,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<Entry> {
        let target = linkname.to_str()
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let path = self.resolve_child(parent, name)?;
        let meta = KeyMeta::new(libc::S_IFLNK as u32 | 0o777, 0, 0);
        let key = keys::encode(&path, meta);

        log!("S3 PUT symlink {} -> {}", path, target);
        self.client.put(&key, target.as_bytes())
            .io_err(&format!("symlink PUT {}", path))?;

        let mut cache = self.cache.lock().unwrap();
        let ino = cache.insert_from_key(&path, meta, target.len() as u64);
        // Cache the symlink target on the just-inserted inode so readlink
        // doesn't need a follow-up GET. Not using `insert_and_entry`
        // because that helper doesn't know about the target field.
        cache.set_symlink_target(&path, target.to_string());
        let entry = cache.get_by_ino(ino)
            .expect("just-inserted ino missing under held cache lock")
            .1;
        Ok(Self::entry_for(ino, entry))
    }

    fn rmdir(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<()> {
        let path = self.resolve_child(parent, name)?;

        // Check it's a directory and get its meta for the S3 key
        let meta = {
            let mut cache = self.cache.lock().unwrap();
            match cache.get(&path) {
                Some(e) if e.meta.file_type() == libc::S_IFDIR as u32 => e.meta,
                Some(_) => return Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
                None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        };

        // Verify directory is empty — LIST with max-keys=1
        let prefix = format!("{}/", path);
        let result = self.client.list_probe(&prefix, None, 1)
            .io_err(&format!("rmdir empty-check {}", prefix))?;
        if !result.objects.is_empty() {
            return Err(io::Error::from_raw_os_error(libc::ENOTEMPTY));
        }

        let key = keys::encode(&path, meta);
        log!("S3 DELETE rmdir {}", path);
        self.client.delete(&key)
            .io_err(&format!("rmdir DELETE {}", path))?;

        let mut cache = self.cache.lock().unwrap();
        cache.remove(&path);
        Ok(())
    }

    fn link(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        newparent: Self::Inode,
        newname: &CStr,
    ) -> io::Result<Entry> {
        // Hardlink emulation via .hardlinks/ directory.
        // File data moves to .hardlinks/<uuid>, both paths become references.
        // TODO: hardlink cleanup — when all references to a UUID are deleted,
        // the .hardlinks/<uuid> object becomes orphaned. Need refcount or GC.
        let new_path = self.resolve_child(newparent, newname)?;

        let (old_path, meta, size, existing_uuid) = {
            let cache = self.cache.lock().unwrap();
            let (p, e) = cache.get_by_ino(inode)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            (p.to_string(), e.meta, e.size, e.hardlink_target.clone())
        };

        let uuid = if let Some(ref uuid) = existing_uuid {
            // Already a hardlink — reuse the same UUID
            uuid.clone()
        } else {
            // First hardlink — move data to .hardlinks/<uuid>.
            // Drain any pending deferred write for this path first:
            // otherwise client.copy() below reads stale S3 bytes and
            // the subsequent deferred PUT would race against the
            // hardlink ref we write at line `client.put(&old_key, ...)`.
            // See tests/test_correctness.rs::hardlink_write_through_append.
            self.write_pool.wait_for_path(&old_path);

            let uuid = storage::new_hardlink_uuid();
            let old_key = keys::encode(&old_path, meta);
            let hl_key = storage::hardlink_key(&uuid, meta);

            log!("S3 hardlink: moving data {} -> .hardlinks/{}", old_path, uuid);
            self.client.copy(&old_key, &hl_key)
                .io_err(&format!("hardlink COPY {} -> .hardlinks/{}", old_path, uuid))?;

            // Overwrite old path with reference
            self.client.put(&old_key, &storage::encode_hardlink_ref(&uuid))
                .io_err(&format!("hardlink ref PUT {}", old_path))?;

            // Update cache for old path
            let mut cache = self.cache.lock().unwrap();
            cache.set_hardlink_target(&old_path, uuid.clone());
            drop(cache);

            uuid
        };

        // Create new path as reference
        let new_key = keys::encode(&new_path, meta);

        log!("S3 PUT hardlink ref {} -> .hardlinks/{}", new_path, uuid);
        self.client.put(&new_key, &storage::encode_hardlink_ref(&uuid))
            .io_err(&format!("hardlink ref PUT {}", new_path))?;

        // Cache new path with hardlink UUID. Can't use insert_and_entry
        // because we need to set the hardlink target on the inode before
        // handing the Entry back — otherwise the first read would pay an
        // extra GET to rediscover it.
        let mut cache = self.cache.lock().unwrap();
        let ino = cache.insert_from_key(&new_path, meta, size);
        cache.set_hardlink_target(&new_path, uuid);
        let entry = cache.get_by_ino(ino)
            .expect("just-inserted ino missing under held cache lock")
            .1;
        Ok(Self::entry_for(ino, entry))
    }

    fn access(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _mask: u32,
    ) -> io::Result<()> {
        // Guest runs as root, S3 doesn't enforce POSIX perms
        Ok(())
    }

    fn flush(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        handle: Self::Handle,
        _lock_owner: u64,
    ) -> io::Result<()> {
        // Flush = push dirty buffer into the write queue (like
        // `release`, but the handle stays open). Doesn't block on the
        // PUT — that's `fsync`'s job. If the handle has no dirty
        // buffer, this is a no-op.
        //
        // We do NOT consume entry.data — the guest's next `read`
        // through this same handle would otherwise see an empty
        // buffer instead of its own writes. Leave the data in the
        // handle; release/subsequent-flush still send the latest
        // version to the inflight queue, which replaces whatever was
        // there.
        let snapshot: Option<(String, KeyMeta, Option<String>, Arc<Vec<u8>>, Durability)> = {
            let handles = self.handles.lock().unwrap();
            match handles.by_handle.get(&handle) {
                Some(entry) if entry.dirty => {
                    if let Some(ref data) = entry.data {
                        let (path, meta, hl_uuid) = self
                            .path_meta_hl_for_inode(entry.inode)
                            .ok()
                            .unwrap_or_default();
                        if path.is_empty() {
                            None
                        } else {
                            Some((path, meta, hl_uuid, Arc::new(data.clone()), entry.rules.durability))
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };

        let Some((path, meta, hl_uuid, shared, durability)) = snapshot else { return Ok(()); };
        let key = match hl_uuid {
            Some(ref uuid) => storage::hardlink_key(uuid, meta),
            None => keys::encode(&path, meta),
        };

        // Same deferred-or-sync path as release. flush is typically
        // called for explicit guest-requested partial writes (e.g.
        // before fsync); we still respect the inflight budget.
        // flush() does NOT coalesce — the guest explicitly requested
        // a sync point. Coalesced debounce timers are flushed by
        // fsync() via wait_for_path(), which calls flush_coalesce().
        let deferred = self.write_pool.enqueue(
            path.clone(),
            key.clone(),
            Arc::clone(&shared),
            durability,
            None, // no coalesce on explicit flush
        )?;
        if !deferred {
            self.client
                .put(&key, &shared)
                .io_err(&format!("flush sync PUT {}", path))?;
        }
        Ok(())
    }

    fn fsync(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _datasync: bool,
        handle: Self::Handle,
    ) -> io::Result<()> {
        // fsync = "make this handle's writes durable before returning."
        // With deferred writes: (1) flush pushes the dirty buffer
        // into the inflight queue, (2) wait for the path to drain.
        //
        // The wait is unbounded. In practice the worker's
        // exponential-backoff retry loop (research/13 §2.4) will
        // keep trying against a sustained S3 outage; a fsync caller
        // under that condition will block indefinitely. That's the
        // correct behavior — fsync is the durability opt-in.

        // Resolve path while the handle is still open so we can
        // wait by path.
        let inode = {
            let handles = self.handles.lock().unwrap();
            handles.by_handle.get(&handle).map(|e| e.inode)
        };
        self.flush(_ctx, 0, handle, 0)?;
        if let Some(inode) = inode {
            if let Ok((path, _, _)) = self.path_meta_hl_for_inode(inode) {
                if !path.is_empty() {
                    self.write_pool.wait_for_path(&path);
                }
            }
        }
        Ok(())
    }

    fn mknod(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
        mode: u32,
        _rdev: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        // Only support regular files — devices/fifos not applicable for S3
        if (mode & libc::S_IFMT as u32) != libc::S_IFREG as u32 && (mode & libc::S_IFMT as u32) != 0 {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }
        let path = self.resolve_child(parent, name)?;
        let file_mode = if (mode & libc::S_IFMT as u32) == 0 {
            libc::S_IFREG as u32 | (mode & 0o7777)
        } else {
            mode
        };
        let meta = KeyMeta::new(file_mode, 0, 0);
        let key = keys::encode(&path, meta);

        log!("S3 PUT mknod {}", path);
        self.client.put(&key, &[])
            .io_err(&format!("mknod PUT {}", path))?;

        let mut cache = self.cache.lock().unwrap();
        let (_, entry) = Self::insert_and_entry(&mut cache, &path, meta, 0);
        Ok(entry)
    }

    fn fallocate(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _handle: Self::Handle,
        _mode: u32,
        _offset: u64,
        _length: u64,
    ) -> io::Result<()> {
        // No-op — S3 doesn't support preallocation
        Ok(())
    }

    fn getxattr(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _name: &CStr,
        _size: u32,
    ) -> io::Result<fuse_backend_rs::api::filesystem::GetxattrReply> {
        // We don't support xattrs at all (unlike ext4 where specific attrs
        // may be missing). Return ENOTSUP (aka EOPNOTSUPP) — the POSIX
        // signal for "filesystem doesn't support this feature".
        //
        // Returning ENODATA here (per-attr miss) causes coreutils `ls -la`
        // to log spurious EIO when probing system.posix_acl_access/default
        // before rendering permission bits.
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }

    fn listxattr(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _size: u32,
    ) -> io::Result<fuse_backend_rs::api::filesystem::ListxattrReply> {
        // Same rationale — ENOTSUP tells callers not to expect xattrs.
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }
}
