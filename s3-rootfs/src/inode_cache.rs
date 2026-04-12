// Inode cache — maps filesystem paths to inode metadata.
//
// The cache is populated by FUSE operations (readdir, lookup) and
// serves subsequent lookups without hitting S3. Currently a simple
// HashMap; the trait interface allows swapping in LRU, TTL-based,
// or shared-memory implementations later.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::keys::KeyMeta;

/// Default cache TTL — entries older than this are evicted on access.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

/// Cached metadata for a filesystem entry
#[derive(Debug, Clone)]
pub struct CachedInode {
    pub ino: u64,
    pub meta: KeyMeta,
    pub size: u64,
    /// Symlink target — populated on first readlink, cached thereafter
    pub symlink_target: Option<String>,
    /// Hardlink UUID — if set, file data lives at .hardlinks/<uuid>
    /// and this path's S3 object body is just "hardlink:<uuid>"
    pub hardlink_target: Option<String>,
    /// When this entry was last accessed — used for memory reclamation only.
    /// Cache is always correct (single-writer), TTL just frees unused memory.
    pub last_accessed: Instant,
}

/// Cache for inode metadata, keyed by filesystem path.
///
/// All FUSE operations check the cache before hitting S3.
/// The cache is populated by:
/// - lookup (from LIST results, including pre-fill neighbors)
/// - readdir (all directory entries)
/// - create/mkdir (new entries)
pub struct InodeCache {
    by_path: HashMap<String, CachedInode>,
    by_ino: HashMap<u64, String>,             // ino → path (reverse lookup)
    negative: HashMap<String, Instant>,       // paths known to not exist + when cached
    next_ino: u64,
    /// TTL serves two distinct roles:
    /// (a) Eviction timer for positive cache entries — pure memory
    ///     reclamation. The positive cache is authoritative (single-writer
    ///     through this process), so evicting an entry only forces a re-LIST
    ///     on next access; it never changes the answer.
    /// (b) Expiry timer for negative cache entries — correctness-relevant.
    ///     If something outside this process writes the bucket, a negative
    ///     entry would otherwise shadow the new object forever; the TTL
    ///     bounds that staleness window.
    ttl: Duration,
    /// Operation counter for triggering periodic reap
    op_count: u64,
    // Hit/miss counters for observability
    pub hits: u64,
    pub misses: u64,
    pub negative_hits: u64,
}

impl InodeCache {
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_CACHE_TTL)
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        let mut cache = Self {
            by_path: HashMap::new(),
            by_ino: HashMap::new(),
            negative: HashMap::new(),
            next_ino: 2, // 1 reserved for root
            ttl,
            op_count: 0,
            hits: 0,
            misses: 0,
            negative_hits: 0,
        };
        // Root directory
        cache.insert("", CachedInode {
            ino: 1,
            meta: KeyMeta::new(libc::S_IFDIR as u32 | 0o755, 0, 0),
            size: 0,
            symlink_target: None,
            hardlink_target: None,
            last_accessed: Instant::now(),
        });
        cache
    }

    /// Look up by path. Always returns the entry if present — cache is
    /// authoritative (single-writer). Triggers periodic reap for memory.
    pub fn get(&mut self, path: &str) -> Option<&CachedInode> {
        self.op_count += 1;
        if self.op_count % 100 == 0 {
            self.maybe_reap();
        }

        if self.by_path.contains_key(path) {
            self.hits += 1;
            // Two-lookup pattern is intentional: bumping self.hits and
            // updating entry.last_accessed each need a disjoint &mut borrow
            // of self, and the borrow checker can't split them through a
            // single get_mut. Splitting costs one extra HashMap probe on a
            // hot path; the code-reader tax of a hand-rolled unsafe shortcut
            // is worse than one probe.
            if let Some(entry) = self.by_path.get_mut(path) {
                entry.last_accessed = Instant::now();
            }
            self.by_path.get(path)
        } else {
            self.misses += 1;
            None
        }
    }

    /// Look up by inode number. Returns (path, inode) on hit.
    pub fn get_by_ino(&self, ino: u64) -> Option<(&str, &CachedInode)> {
        let path = self.by_ino.get(&ino)?;
        let inode = self.by_path.get(path)?;
        // Note: doesn't update last_accessed to avoid &mut requirement.
        // get() is the primary access path and handles timestamps.
        Some((path, inode))
    }

    /// Insert or update a cache entry. Assigns an inode number if new.
    pub fn insert(&mut self, path: &str, mut entry: CachedInode) -> u64 {
        entry.last_accessed = Instant::now();
        self.negative.remove(path);
        if let Some(existing) = self.by_path.get(path) {
            let ino = existing.ino;
            entry.ino = ino;
            // Preserve symlink/hardlink targets if we had them and new entry doesn't
            if entry.symlink_target.is_none() {
                entry.symlink_target = existing.symlink_target.clone();
            }
            if entry.hardlink_target.is_none() {
                entry.hardlink_target = existing.hardlink_target.clone();
            }
            self.by_path.insert(path.to_string(), entry);
            ino
        } else {
            let ino = if entry.ino > 0 { entry.ino } else { self.next_ino };
            if entry.ino == 0 {
                entry.ino = ino;
                self.next_ino = self.next_ino.max(ino + 1);
            }
            self.by_ino.insert(ino, path.to_string());
            self.by_path.insert(path.to_string(), entry);
            ino
        }
    }

    /// Insert from a decoded S3 key (path + KeyMeta) with a known size.
    /// Convenience for populating from LIST results. The `last_accessed`
    /// value is overwritten inside `insert()` — we pass `Instant::now()`
    /// here only because `CachedInode` requires one, not because callers
    /// should rely on it being preserved.
    pub fn insert_from_key(&mut self, path: &str, meta: KeyMeta, size: u64) -> u64 {
        self.insert(path, CachedInode {
            ino: 0, // will be assigned
            meta,
            size,
            symlink_target: None,
            hardlink_target: None,
            last_accessed: Instant::now(),
        })
    }

    /// Remove an entry by path.
    pub fn remove(&mut self, path: &str) {
        if let Some(entry) = self.by_path.remove(path) {
            self.by_ino.remove(&entry.ino);
        }
    }

    /// Set the symlink target for a cached inode.
    pub fn set_symlink_target(&mut self, path: &str, target: String) {
        if let Some(entry) = self.by_path.get_mut(path) {
            entry.symlink_target = Some(target);
        }
    }

    /// Look up by path without mutating (no hit/miss counting).
    pub fn get_by_path(&self, path: &str) -> Option<&CachedInode> {
        self.by_path.get(path)
    }

    /// Set the hardlink UUID for a cached inode.
    pub fn set_hardlink_target(&mut self, path: &str, uuid: String) {
        if let Some(entry) = self.by_path.get_mut(path) {
            entry.hardlink_target = Some(uuid);
        }
    }

    /// Record a negative cache entry (path known to not exist).
    pub fn insert_negative(&mut self, path: &str) {
        self.negative.insert(path.to_string(), Instant::now());
    }

    /// Check if a path is negatively cached (known miss).
    pub fn is_negative(&mut self, path: &str) -> bool {
        if let Some(ts) = self.negative.get(path) {
            // Expire stale negative entries
            if ts.elapsed() > self.ttl {
                self.negative.remove(path);
                return false;
            }
            self.negative_hits += 1;
            true
        } else {
            false
        }
    }

    /// Redis-style probabilistic reap. Samples up to 20 entries per round,
    /// evicts stale ones; if >25% of the sample was stale, samples again
    /// (max 3 rounds, 60 entries total). Root inode (ino=1) is never reaped.
    ///
    /// Sampling strategy: `HashMap` iteration order is deterministic within
    /// one instance, so a naive `keys().take(20)` would hit the same 20
    /// entries every call — biased against whichever keys happened to land
    /// early in the hash layout. We pick a rotating offset seeded from
    /// `op_count` and the round index, then `skip(offset).take(20)`. Not
    /// cryptographic randomness; just enough to give every key eventual
    /// coverage across invocations without pulling in a PRNG crate.
    fn maybe_reap(&mut self) {
        let now = Instant::now();
        for round in 0..3u64 {
            let len = self.by_path.len();
            if len == 0 { break; }

            // Knuth-multiplicative hash on (op_count, round) gives us a
            // cheap pseudo-random offset. Wrapping arithmetic is fine —
            // we just mod into [0, len) anyway.
            let seed = self.op_count.wrapping_add(round).wrapping_mul(2654435761);
            let offset = (seed as usize) % len;

            let paths: Vec<String> = self.by_path.keys()
                .skip(offset)
                .take(20)
                .cloned()
                .collect();

            if paths.is_empty() { break; }

            let sample_size = paths.len();
            let mut evicted = 0;

            for path in &paths {
                if let Some(entry) = self.by_path.get(path) {
                    if entry.ino == 1 { continue; } // never reap root
                    if now.duration_since(entry.last_accessed) > self.ttl {
                        let ino = entry.ino;
                        self.by_path.remove(path);
                        self.by_ino.remove(&ino);
                        evicted += 1;
                    }
                }
            }

            // Also reap stale negative entries. Same rotating-offset trick
            // so we don't always sweep the same head of the map.
            let neg_len = self.negative.len();
            if neg_len > 0 {
                let neg_offset = (seed as usize) % neg_len;
                let neg_paths: Vec<String> = self.negative.iter()
                    .skip(neg_offset)
                    .filter(|(_, ts)| now.duration_since(**ts) > self.ttl)
                    .map(|(p, _)| p.clone())
                    .take(20)
                    .collect();
                for path in &neg_paths {
                    self.negative.remove(path);
                }
            }

            // If <25% of sample was stale, stop
            if evicted * 4 < sample_size {
                break;
            }
        }
    }

    /// Number of cached inodes (excluding root).
    pub fn inode_count(&self) -> usize {
        self.by_path.len()
    }

    /// Number of negatively cached paths.
    pub fn negative_count(&self) -> usize {
        self.negative.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_root_exists() {
        let mut cache = InodeCache::new();
        let root = cache.get("").unwrap();
        assert_eq!(root.ino, 1);
        assert_eq!(root.meta.file_type(), libc::S_IFDIR as u32);
    }

    #[test]
    fn test_insert_and_get() {
        let mut cache = InodeCache::new();
        let meta = KeyMeta::new(libc::S_IFREG as u32 | 0o644, 1000, 1000);
        let ino = cache.insert_from_key("etc/hosts", meta, 150);
        assert!(ino >= 2);

        let entry = cache.get("etc/hosts").unwrap();
        assert_eq!(entry.ino, ino);
        assert_eq!(entry.size, 150);
        assert_eq!(entry.meta.file_type(), libc::S_IFREG as u32);
    }

    #[test]
    fn test_reverse_lookup() {
        let mut cache = InodeCache::new();
        let meta = KeyMeta::new(libc::S_IFDIR as u32 | 0o755, 0, 0);
        let ino = cache.insert_from_key("usr/bin", meta, 0);

        let (path, entry) = cache.get_by_ino(ino).unwrap();
        assert_eq!(path, "usr/bin");
        assert_eq!(entry.meta.file_type(), libc::S_IFDIR as u32);
    }

    #[test]
    fn test_symlink_target() {
        let mut cache = InodeCache::new();
        let meta = KeyMeta::new(libc::S_IFLNK as u32 | 0o777, 0, 0);
        cache.insert_from_key("sbin", meta, 0);
        cache.set_symlink_target("sbin", "usr/sbin".to_string());

        let entry = cache.get("sbin").unwrap();
        assert_eq!(entry.meta.file_type(), libc::S_IFLNK as u32);
        assert_eq!(entry.symlink_target.as_deref(), Some("usr/sbin"));
    }

    #[test]
    fn test_update_preserves_ino() {
        let mut cache = InodeCache::new();
        let meta = KeyMeta::new(libc::S_IFREG as u32 | 0o644, 0, 0);
        let ino1 = cache.insert_from_key("file.txt", meta, 100);
        let ino2 = cache.insert_from_key("file.txt", meta, 200);
        assert_eq!(ino1, ino2);
        assert_eq!(cache.get("file.txt").unwrap().size, 200);
    }
}
