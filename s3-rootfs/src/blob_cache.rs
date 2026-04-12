//! Content-addressed blob cache.
//!
//! Two-tier cache for hash → bytes, backing the golden-image read path
//! specified in `research/12-golden-fs-implementation.md`. The design
//! split is:
//!
//! - `BlobCache` trait — narrow (`fetch` + `insert`), thread-safe,
//!   owns its own tier logic internally.
//! - `RamBlobCache` — LFU-ish eviction with rotating-offset
//!   probabilistic sampling. Ported from the InodeCache reap
//!   machinery we prototyped in the hygiene pass and then removed
//!   (wrong home — the inode cache is bounded by workload, not memory;
//!   the blob cache has real memory pressure).
//! - `DiskBlobCache` — stub. Fills in at Phase 3.
//! - `TieredBlobCache` — composer. RAM first, disk second. An S3
//!   fallback tier will be plumbed in at Phase 4 alongside the golden
//!   client.
//!
//! Nothing in this module is wired into the FUSE path yet. Phase 0b
//! is pure scaffolding; correctness is gated by unit tests only.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Sample this many entries per eviction round. Mirrors the InodeCache
/// reap we'd prototyped — small enough to be cheap, large enough to
/// give coverage across many invocations with the rotating offset.
const EVICT_SAMPLE: usize = 20;

/// Trigger `maybe_evict` every N `fetch()` + `insert()` calls. Matches
/// the 1-in-100 cadence we'd picked for InodeCache: frequent enough
/// to bound memory promptly, rare enough to keep the hot path clean.
const EVICT_INTERVAL: u64 = 100;

/// Knuth-multiplicative constant used to derive rotating offsets from
/// `op_count`. Not cryptographic randomness; just enough spread to
/// stop sampling hitting the same HashMap head on every call.
const KNUTH: u64 = 2654435761;

/// Trait for a content-addressed blob cache. Narrow on purpose —
/// `fetch` and `insert` are enough to back the golden read path;
/// tiering and eviction happen inside impls, not at callers.
///
/// Implementations are `Send + Sync` because golden blobs are shared
/// across every VM on a host.
pub trait BlobCache: Send + Sync {
    /// Fetch bytes for a content hash. Walks the cache tiers
    /// internally; returns `NotFound` if no tier has the blob (and
    /// in the wired-up world the caller would then S3-GET and
    /// re-insert). Returned `Arc` means the hot path avoids copies —
    /// multiple concurrent readers share one byte buffer.
    fn fetch(&self, hash: &str) -> io::Result<Arc<Vec<u8>>>;

    /// Insert bytes keyed by hash. Caller guarantees `hash` is the
    /// correct content digest — the cache does not re-verify on the
    /// insert path. Verification happens at the golden-S3 boundary
    /// (DiskBlobCache's promotion path, future).
    fn insert(&self, hash: &str, bytes: Arc<Vec<u8>>);
}

// ── RAM tier ───────────────────────────────────────────────────────────

/// Entry stored in the RAM tier. `Arc<Vec<u8>>` lets concurrent
/// `fetch()` calls share the same allocation; eviction only drops the
/// cache's refcount, not any outstanding caller's.
struct RamEntry {
    bytes: Arc<Vec<u8>>,
    /// LFU signal. Bumped on every `fetch()` hit. Not reset — long-
    /// lived hot entries naturally develop a large lead.
    access_count: u64,
    /// Tiebreaker when two sampled entries share `access_count` and
    /// a secondary recency signal is useful. Not load-bearing on its
    /// own — the access counter is the primary eviction metric.
    /// Populated on insert/fetch; not yet read anywhere — reserved
    /// for Phase 3's DiskBlobCache promotion path (where a warm
    /// disk-tier entry with old `last_accessed` is the right
    /// candidate to drop from RAM and keep on disk).
    #[allow(dead_code)]
    last_accessed: Instant,
    /// Cached `bytes.len()` so budget arithmetic doesn't dereference
    /// the Arc on every sweep.
    size: usize,
}

/// In-memory content-addressed cache with LFU-ish eviction.
///
/// Eviction strategy:
/// - Track a byte budget (`budget_bytes`). `insert()` updates the
///   running total; when the cache exceeds budget, the next few
///   ops trigger a sweep.
/// - Sweeps sample ~20 random entries (rotating offset seeded from
///   `op_count`), evict the lowest `access_count` entry. Repeat up
///   to 3 rounds per sweep, or until we're back under budget.
/// - `fetch()` and `insert()` both bump `op_count`; every
///   `EVICT_INTERVAL` bumps triggers a sweep call. Keeps eviction
///   off the hot path in aggregate while still making progress.
pub struct RamBlobCache {
    entries: Mutex<RamState>,
    budget_bytes: usize,
    op_count: AtomicU64,
}

/// Interior state of `RamBlobCache`. Kept as a single struct so the
/// Mutex holds everything eviction needs in one borrow.
struct RamState {
    map: HashMap<String, RamEntry>,
    /// Sum of `entry.size` across all entries; maintained by
    /// `insert` and `evict_one` so we don't rescan the whole map.
    total_bytes: usize,
}

impl RamBlobCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: Mutex::new(RamState {
                map: HashMap::new(),
                total_bytes: 0,
            }),
            budget_bytes,
            op_count: AtomicU64::new(0),
        }
    }

    /// Count of cached blobs. For tests / observability.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().map.len()
    }

    /// Current resident byte count. For tests / observability.
    pub fn total_bytes(&self) -> usize {
        self.entries.lock().unwrap().total_bytes
    }

    /// Bump the op counter and, if we've crossed an interval boundary,
    /// run a sweep. Sweep runs under its own lock — op counter is
    /// atomic-only so we never block fetchers on it.
    fn tick_and_maybe_evict(&self) {
        let n = self.op_count.fetch_add(1, Ordering::Relaxed);
        if n % EVICT_INTERVAL == 0 && n > 0 {
            self.maybe_evict(n);
        }
    }

    /// Probabilistic eviction sweep.
    ///
    /// Only fires when we're over budget. Each round:
    ///
    /// 1. Pick a rotating offset into the HashMap seeded from
    ///    `op_count` (Knuth-multiplicative). Rotating both across
    ///    invocations and between rounds means `keys().skip(k).take(N)`
    ///    eventually reaches every entry, even though iteration order
    ///    within one process is deterministic.
    /// 2. Sample `EVICT_SAMPLE` entries, compute the median of their
    ///    `access_count`.
    /// 3. Evict every sampled entry whose `access_count < median` —
    ///    roughly half the sample per round. This is LFU, not LRU:
    ///    hot blobs (libc, common shared libs) develop a large
    ///    access_count lead and survive sweeps regardless of recency.
    /// 4. If still over budget, go to the next round. At most 3
    ///    rounds per sweep (≤60 entries examined, ≤30 evicted per
    ///    call), which bounds worst-case sweep cost.
    fn maybe_evict(&self, seed: u64) {
        let mut state = self.entries.lock().unwrap();
        if state.total_bytes <= self.budget_bytes {
            return;
        }

        for round in 0..3u64 {
            if state.total_bytes <= self.budget_bytes {
                break;
            }
            let len = state.map.len();
            if len == 0 {
                break;
            }
            let round_seed = seed.wrapping_add(round).wrapping_mul(KNUTH);
            let offset = (round_seed as usize) % len;

            // Snapshot (key, access_count) for the sample window.
            // Keys cloned out so we can `map.remove()` later without
            // carrying an iterator borrow. End-of-map sampling may
            // yield fewer than EVICT_SAMPLE entries when
            // `offset + EVICT_SAMPLE > len`; rotating the offset
            // across rounds and invocations fills coverage over time.
            let mut sample: Vec<(String, u64)> = state
                .map
                .iter()
                .skip(offset)
                .take(EVICT_SAMPLE)
                .map(|(k, v)| (k.clone(), v.access_count))
                .collect();
            if sample.is_empty() {
                break;
            }

            // Sort the sample ascending by access_count, evict the
            // bottom half. This is the LFU interpretation of "below
            // the sample's median" — with variance in the sample, the
            // bottom half is strictly cooler than the top half; with
            // no variance (all fresh entries at access_count=1), we
            // still evict half to make progress rather than stalling
            // on the edge case. Ties within the bottom half are
            // resolved by HashMap iteration order, which is fine:
            // equally-cold entries are equally-evictable.
            sample.sort_unstable_by_key(|(_, ac)| *ac);
            let evict_n = sample.len() / 2;
            for (key, _) in sample.into_iter().take(evict_n) {
                if let Some(removed) = state.map.remove(&key) {
                    state.total_bytes = state.total_bytes.saturating_sub(removed.size);
                }
                if state.total_bytes <= self.budget_bytes {
                    return;
                }
            }
        }
    }
}

impl BlobCache for RamBlobCache {
    fn fetch(&self, hash: &str) -> io::Result<Arc<Vec<u8>>> {
        let result = {
            let mut state = self.entries.lock().unwrap();
            match state.map.get_mut(hash) {
                Some(entry) => {
                    // saturating_add is paranoid — overflow is effectively
                    // impossible on u64 (~10^19 fetches to saturate) but
                    // cheap to be safe and documents intent at the call site.
                    entry.access_count = entry.access_count.saturating_add(1);
                    entry.last_accessed = Instant::now();
                    Ok(Arc::clone(&entry.bytes))
                }
                None => Err(io::Error::from(io::ErrorKind::NotFound)),
            }
        };
        // Bump the op counter regardless of hit/miss. Miss counts
        // toward sweep cadence too — otherwise a cold cache never
        // evicts even as insert()s pile up.
        self.tick_and_maybe_evict();
        result
    }

    fn insert(&self, hash: &str, bytes: Arc<Vec<u8>>) {
        let size = bytes.len();
        let over_budget = {
            let mut state = self.entries.lock().unwrap();
            // If we're replacing an existing key, decrement old size
            // before adding the new one — otherwise total_bytes drifts.
            if let Some(prev) = state.map.remove(hash) {
                state.total_bytes = state.total_bytes.saturating_sub(prev.size);
            }
            state.map.insert(
                hash.to_string(),
                RamEntry {
                    bytes,
                    access_count: 1,
                    last_accessed: Instant::now(),
                    size,
                },
            );
            state.total_bytes = state.total_bytes.saturating_add(size);
            state.total_bytes > self.budget_bytes
        };
        // Bump the op counter for cadence accounting. If the insert
        // just pushed us over budget, don't wait for the next interval
        // tick — sweep now. maybe_evict is an internal no-op when
        // we're already under budget, so this is bounded work even
        // under burst load.
        let n = self.op_count.fetch_add(1, Ordering::Relaxed);
        if over_budget {
            self.maybe_evict(n);
        } else if n % EVICT_INTERVAL == 0 && n > 0 {
            self.maybe_evict(n);
        }
    }
}

// ── Disk tier (stub) ───────────────────────────────────────────────────

/// On-disk content-addressed cache. Stub for Phase 3 implementation
/// — today `fetch()` always returns `NotFound` and `insert()` drops
/// the bytes on the floor (with a debug-log breadcrumb so a test
/// that accidentally routes through here fails loudly if it asserts
/// on retained state).
///
/// Phase 3 will fill in a real 2-level prefix-sharded layout
/// (`<root>/ab/cd/<full-hash>`) with LRU eviction and on-fetch
/// content verification.
pub struct DiskBlobCache {
    /// Directory that would back the tier. Stashed so the wiring PR
    /// doesn't need to touch this file's signature.
    #[allow(dead_code)]
    root: std::path::PathBuf,
}

impl DiskBlobCache {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self { root }
    }
}

impl BlobCache for DiskBlobCache {
    fn fetch(&self, _hash: &str) -> io::Result<Arc<Vec<u8>>> {
        // TODO(phase-3): read <root>/<shard>/<hash>, verify the BLAKE3
        // hash of the bytes matches, return the Arc. Miss returns
        // NotFound.
        Err(io::Error::from(io::ErrorKind::NotFound))
    }

    fn insert(&self, _hash: &str, _bytes: Arc<Vec<u8>>) {
        // TODO(phase-3): atomically write bytes to
        // <root>/<shard>/<hash> (tempfile + rename), update LRU
        // metadata, maybe trigger sweep.
    }
}

// ── Composer ───────────────────────────────────────────────────────────

/// Composes a RAM tier and a disk tier behind the `BlobCache`
/// interface. Fetch walks RAM → disk, promoting disk hits back into
/// RAM. Insert populates both tiers (disk is authoritative storage;
/// RAM is the fast path). An S3 fallback gets added in Phase 4
/// alongside the golden client.
pub struct TieredBlobCache {
    ram: Arc<RamBlobCache>,
    disk: Arc<dyn BlobCache>,
}

impl TieredBlobCache {
    pub fn new(ram: Arc<RamBlobCache>, disk: Arc<dyn BlobCache>) -> Self {
        Self { ram, disk }
    }
}

impl BlobCache for TieredBlobCache {
    fn fetch(&self, hash: &str) -> io::Result<Arc<Vec<u8>>> {
        match self.ram.fetch(hash) {
            Ok(bytes) => Ok(bytes),
            Err(_) => {
                let bytes = self.disk.fetch(hash)?;
                // Promote disk hit back into RAM so the next access
                // is fast. If the disk tier is the stub, this branch
                // is unreachable in practice — but the wiring stays
                // correct for Phase 3's real disk impl.
                self.ram.insert(hash, Arc::clone(&bytes));
                Ok(bytes)
            }
        }
    }

    fn insert(&self, hash: &str, bytes: Arc<Vec<u8>>) {
        self.ram.insert(hash, Arc::clone(&bytes));
        self.disk.insert(hash, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: make a blob of given size filled with a byte pattern.
    fn blob(size: usize, fill: u8) -> Arc<Vec<u8>> {
        Arc::new(vec![fill; size])
    }

    #[test]
    fn ram_hit_and_miss() {
        let cache = RamBlobCache::new(1 << 20);
        assert!(cache.fetch("abc").is_err());

        cache.insert("abc", blob(100, 0xAA));
        let got = cache.fetch("abc").unwrap();
        assert_eq!(got.len(), 100);
        assert_eq!(got[0], 0xAA);
    }

    #[test]
    fn ram_replace_updates_total_bytes() {
        let cache = RamBlobCache::new(1 << 20);
        cache.insert("h", blob(100, 0));
        assert_eq!(cache.total_bytes(), 100);
        cache.insert("h", blob(250, 0));
        assert_eq!(cache.total_bytes(), 250);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn ram_evicts_when_over_budget() {
        // Budget fits ~10 × 1KB blobs; insert 200 to guarantee
        // sweeps fire and push us back under budget.
        let cache = RamBlobCache::new(10 * 1024);
        for i in 0..200 {
            cache.insert(&format!("h{i:03}"), blob(1024, (i & 0xff) as u8));
        }
        // We can't be exact (sampling is probabilistic) but resident
        // bytes should be roughly bounded by budget plus one entry's
        // worth of slack.
        let total = cache.total_bytes();
        assert!(
            total <= 20 * 1024,
            "total_bytes {total} far above 2x budget — eviction sweep not firing"
        );
        assert!(cache.len() < 200, "nothing evicted");
    }

    #[test]
    fn ram_keeps_hot_entries_under_pressure() {
        // Insert many blobs, hammer one of them, then insert more to
        // force eviction. The hot one should survive.
        let cache = RamBlobCache::new(20 * 1024);
        for i in 0..40 {
            cache.insert(&format!("cold{i:03}"), blob(1024, 0));
        }
        cache.insert("hot", blob(1024, 0xCA));
        // Many fetches to build a large access_count lead on "hot".
        for _ in 0..500 {
            let _ = cache.fetch("hot").unwrap();
        }
        // Insert enough more blobs to trigger several sweeps.
        for i in 40..200 {
            cache.insert(&format!("cold{i:03}"), blob(1024, 0));
        }
        assert!(
            cache.fetch("hot").is_ok(),
            "hot entry evicted despite large access_count lead"
        );
    }

    #[test]
    fn disk_stub_is_a_miss() {
        let disk = DiskBlobCache::new(std::path::PathBuf::from("/tmp/unused"));
        assert!(disk.fetch("anything").is_err());
        disk.insert("anything", blob(10, 0));
        assert!(disk.fetch("anything").is_err(), "stub shouldn't retain");
    }

    #[test]
    fn tiered_hits_ram_tier_first() {
        let ram = Arc::new(RamBlobCache::new(1 << 20));
        let disk = Arc::new(DiskBlobCache::new(std::path::PathBuf::from("/tmp/unused")));
        let tier = TieredBlobCache::new(ram.clone(), disk);

        tier.insert("x", blob(50, 0x11));
        let got = tier.fetch("x").unwrap();
        assert_eq!(got.len(), 50);
        assert_eq!(ram.len(), 1);
    }

    #[test]
    fn tiered_fetch_miss_when_both_tiers_empty() {
        let ram = Arc::new(RamBlobCache::new(1 << 20));
        let disk = Arc::new(DiskBlobCache::new(std::path::PathBuf::from("/tmp/unused")));
        let tier = TieredBlobCache::new(ram, disk);
        assert!(tier.fetch("missing").is_err());
    }
}
