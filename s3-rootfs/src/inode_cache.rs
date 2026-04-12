// Inode cache — maps filesystem paths to inode metadata.
//
// The cache is authoritative by construction: S3Fs is the sole writer
// to its bucket (single-writer model), so every successful mutation
// goes through this cache first. That means a cached entry is always
// correct — there is no external writer whose changes we could miss,
// so no expiry / staleness handling is needed. A running VM accumulates
// at most a few megabytes of CachedInode (a few hundred bytes each × a
// few thousand files touched over a session), which is not worth
// reaping.
//
// If that assumption ever changes — e.g. a shared golden pool with
// out-of-band writes, or a long-running orphan daemon — this module
// would grow back a TTL and a reap path. Not today.

use std::collections::{HashMap, HashSet};

use crate::keys::KeyMeta;

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
    by_ino: HashMap<u64, String>,   // ino → path (reverse lookup)
    /// Paths known not to exist. Single-writer means a negative entry
    /// stays valid until this process creates something at that path;
    /// `insert()` removes from this set on every positive mutation.
    negative: HashSet<String>,
    next_ino: u64,
    // Hit/miss counters for observability
    pub hits: u64,
    pub misses: u64,
    pub negative_hits: u64,
}

impl InodeCache {
    pub fn new() -> Self {
        let mut cache = Self {
            by_path: HashMap::new(),
            by_ino: HashMap::new(),
            negative: HashSet::new(),
            next_ino: 2, // 1 reserved for root
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
        });
        cache
    }

    /// Look up by path. Always returns the entry if present — the cache
    /// is authoritative (single-writer model).
    pub fn get(&mut self, path: &str) -> Option<&CachedInode> {
        if self.by_path.contains_key(path) {
            self.hits += 1;
            // Two-lookup pattern is intentional: bumping self.hits and
            // returning the entry each need a disjoint borrow of self,
            // and the borrow checker can't split them through a single
            // get_mut. Splitting costs one extra HashMap probe on a hot
            // path; the code-reader tax of a hand-rolled unsafe shortcut
            // is worse than one probe.
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
        Some((path, inode))
    }

    /// Insert or update a cache entry. Assigns an inode number if new.
    pub fn insert(&mut self, path: &str, mut entry: CachedInode) -> u64 {
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
    /// Convenience for populating from LIST results.
    pub fn insert_from_key(&mut self, path: &str, meta: KeyMeta, size: u64) -> u64 {
        self.insert(path, CachedInode {
            ino: 0, // will be assigned
            meta,
            size,
            symlink_target: None,
            hardlink_target: None,
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
    /// Cleared by `insert()` whenever a positive entry lands at `path`.
    pub fn insert_negative(&mut self, path: &str) {
        self.negative.insert(path.to_string());
    }

    /// Check if a path is negatively cached (known miss).
    pub fn is_negative(&mut self, path: &str) -> bool {
        if self.negative.contains(path) {
            self.negative_hits += 1;
            true
        } else {
            false
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

    #[test]
    fn test_negative_cleared_on_insert() {
        let mut cache = InodeCache::new();
        cache.insert_negative("etc/missing");
        assert!(cache.is_negative("etc/missing"));

        let meta = KeyMeta::new(libc::S_IFREG as u32 | 0o644, 0, 0);
        cache.insert_from_key("etc/missing", meta, 0);
        assert!(!cache.is_negative("etc/missing"));
    }
}
