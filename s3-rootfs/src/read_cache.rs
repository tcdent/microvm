//! Guest-wide read cache (research/13 §3).
//!
//! Cross-handle LRU keyed by S3 path, storing `Arc<Vec<u8>>` values.
//! Replaces per-handle whole-file caching — closing a handle no
//! longer drops the buffer, and re-opening the same file hits the
//! cache instead of re-fetching from S3.
//!
//! The companion to the deferred-write inflight map in
//! [`write_pool`]: both hold `Arc<Vec<u8>>` independently, so the
//! freshly-written bytes can move directly from inflight into the
//! read cache on PUT completion.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use lru::LruCache;

/// Cross-handle read cache. `Mutex<LruCache>` because every mutating
/// op (including `get`, which bumps LRU position) needs exclusive
/// access. Contention is bounded — each FUSE read does one short
/// critical section.
pub struct ReadCache {
    entries: Mutex<LruCache<String, Arc<Vec<u8>>>>,
    /// Total bytes across all cached entries. Maintained by `insert`
    /// and `evict_while_over_budget` so we don't rescan on every op.
    current_bytes: AtomicUsize,
    /// Target cap. Exceeded temporarily during insert (entry goes in,
    /// then LRU evicts back under budget).
    budget_bytes: usize,
}

/// Per-file cache-eligibility threshold: files larger than this bypass
/// the cache entirely and stream via the existing read-ahead path.
/// Prevents a single huge read from evicting an otherwise-productive
/// working set.
///
/// Default is `budget_bytes / 4` — a single file can use up to a
/// quarter of the cache, leaving room for three more.
#[inline]
pub fn per_file_threshold(budget: usize) -> usize {
    budget / 4
}

impl ReadCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::unbounded()),
            current_bytes: AtomicUsize::new(0),
            budget_bytes,
        }
    }

    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    /// LRU-bumping fetch. Returns `Some(Arc<Vec<u8>>)` on hit (caller
    /// gets a cheap refcount increment, no byte copy). Bumping to
    /// front-of-LRU is pointer work; the underlying data stays put.
    pub fn fetch(&self, path: &str) -> Option<Arc<Vec<u8>>> {
        let mut entries = self.entries.lock().unwrap();
        entries.get(path).map(Arc::clone)
    }

    /// Insert or replace a cached entry. Evicts LRU entries until
    /// total bytes is back under budget.
    ///
    /// Files larger than [`per_file_threshold`] are silently skipped
    /// — caching a single huge file would immediately evict everything
    /// else and leave only the huge file, which is strictly worse
    /// than just streaming it.
    pub fn insert(&self, path: &str, bytes: Arc<Vec<u8>>) {
        let size = bytes.len();
        if size > per_file_threshold(self.budget_bytes) {
            return;
        }

        let mut entries = self.entries.lock().unwrap();

        // Replace path: remove the old entry first so total_bytes
        // stays accurate across the replacement.
        if let Some(old) = entries.pop(path) {
            self.current_bytes.fetch_sub(old.len(), Ordering::Relaxed);
        }

        entries.put(path.to_string(), bytes);
        self.current_bytes.fetch_add(size, Ordering::Relaxed);

        // Evict LRU until under budget. Loop because `pop_lru` gives
        // us the oldest entry; repeat until back under the cap.
        while self.current_bytes.load(Ordering::Relaxed) > self.budget_bytes {
            match entries.pop_lru() {
                Some((_, dropped)) => {
                    self.current_bytes.fetch_sub(dropped.len(), Ordering::Relaxed);
                }
                None => break, // shouldn't happen — we just inserted
            }
        }
    }

    /// Remove an entry. Called by FUSE ops that invalidate the path
    /// (unlink, rename of old path). Concurrent readers that still
    /// hold the Arc continue reading via Rust's refcount; their data
    /// lingers until their refcount drops.
    pub fn remove(&self, path: &str) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(old) = entries.pop(path) {
            self.current_bytes.fetch_sub(old.len(), Ordering::Relaxed);
        }
    }

    /// Total bytes resident. For observability.
    pub fn bytes(&self) -> usize {
        self.current_bytes.load(Ordering::Relaxed)
    }

    /// Number of cached files. For observability.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(n: usize, fill: u8) -> Arc<Vec<u8>> {
        Arc::new(vec![fill; n])
    }

    #[test]
    fn insert_and_fetch() {
        let cache = ReadCache::new(1 << 20);
        assert!(cache.fetch("etc/hosts").is_none());

        cache.insert("etc/hosts", bytes(100, 0xAA));
        let got = cache.fetch("etc/hosts").unwrap();
        assert_eq!(got.len(), 100);
        assert_eq!(got[0], 0xAA);
        assert_eq!(cache.bytes(), 100);
    }

    #[test]
    fn replace_updates_total_bytes() {
        let cache = ReadCache::new(1 << 20);
        cache.insert("a", bytes(100, 0));
        cache.insert("a", bytes(250, 0));
        assert_eq!(cache.bytes(), 250);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn fetch_bumps_lru_position() {
        // Budget 1200 → per-file threshold = 300. Use 100-byte files
        // so each fits under the threshold, and the budget holds
        // exactly three before eviction kicks in (3 × 100 = 300,
        // +1 more = 400 > 300? no, budget is 1200…).
        //
        // Rewrite: budget 400 → threshold 100. Use 100-byte files
        // (exactly at threshold, cached), budget holds 4. Insert 5 →
        // LRU-oldest gets evicted.
        let cache = ReadCache::new(400);
        cache.insert("a", bytes(100, 1));
        cache.insert("b", bytes(100, 2));
        cache.insert("c", bytes(100, 3));
        cache.insert("d", bytes(100, 4));
        let _ = cache.fetch("a"); // bump 'a' to head; LRU order now: b, c, d, a

        cache.insert("e", bytes(100, 5)); // evicts LRU = 'b'
        assert!(cache.fetch("a").is_some(), "'a' should survive — just bumped");
        assert!(cache.fetch("b").is_none(), "'b' should be evicted");
        assert!(cache.fetch("c").is_some());
        assert!(cache.fetch("d").is_some());
        assert!(cache.fetch("e").is_some());
    }

    #[test]
    fn oversized_file_skips_cache() {
        // budget 1000 → threshold 250. A 400-byte insert must be
        // skipped; the cache's total_bytes must stay 0.
        let cache = ReadCache::new(1000);
        cache.insert("big", bytes(400, 0));
        assert_eq!(cache.bytes(), 0);
        assert!(cache.fetch("big").is_none());
    }

    #[test]
    fn remove_drops_entry() {
        let cache = ReadCache::new(1 << 20);
        cache.insert("p", bytes(50, 0));
        assert_eq!(cache.bytes(), 50);
        cache.remove("p");
        assert_eq!(cache.bytes(), 0);
        assert!(cache.fetch("p").is_none());
    }

    #[test]
    fn eviction_under_budget_pressure() {
        // Budget 1000, threshold 250. Insert 5 × 200-byte files.
        // After all inserts: total should be ≤ 1000 + slack. Since
        // the cache is a strict LRU with byte accounting, we should
        // see at most 5 entries (if all fit) or fewer.
        let cache = ReadCache::new(1000);
        for i in 0..5 {
            cache.insert(&format!("f{i}"), bytes(200, i as u8));
        }
        assert!(
            cache.bytes() <= 1000,
            "total bytes {} exceeds budget 1000",
            cache.bytes()
        );
        // LRU semantics: most recent ("f4") survived.
        assert!(cache.fetch("f4").is_some());
    }
}
