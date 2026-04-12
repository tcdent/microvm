//! Content-addressed blob cache (scaffold).
//!
//! Future home for the hash → bytes cache that backs golden-image
//! sharing across VMs (research/10-fs-daemon.md). Not wired up today.
//!
//! # Why separate from `inode_cache`?
//!
//! Different contract, different keys, different eviction:
//! - `InodeCache` maps **paths** to metadata and is keyed by string.
//!   Entries live a while, expire on TTL, and are always correct
//!   (single-writer).
//! - `BlobCache` maps **content hashes** to bytes. Hashes are stable,
//!   content is immutable, so entries never go stale — they only get
//!   evicted under memory pressure (LFU/LRU, tiered RAM+SSD).
//!
//! Trying to share a common trait across those two buys nothing — the
//! contracts are different enough that delegation would just add
//! noise.
//!
//! # Scaffold
//!
//! This is step 2 scaffold; the real implementation is gated behind
//! the golden-ref feature (plan §8 step 6+). The `Blob` / `BlobKey`
//! types exist now so downstream code can reference them without
//! waiting for the full implementation.

/// Content hash identifying a blob. Today we intend to use SHA-256
/// of the blob bytes, stored as a lowercase hex string. Kept as
/// `String` rather than `[u8; 32]` so it matches the S3 key form
/// directly (`.golden/<hash>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobKey(pub String);

/// Immutable content-addressed blob. Today just an alias; may grow
/// metadata (refcount, last-access time, tier origin) when the cache
/// gains tiers.
#[derive(Debug, Clone)]
pub struct Blob(pub Vec<u8>);

/// Trait for a content-addressed blob cache.
///
/// Kept narrow on purpose — `get` and `insert` are enough to support
/// the golden-ref read path. Eviction happens internally; callers
/// don't drive it. Implementations are expected to be thread-safe
/// (behind locks or lock-free datastructures).
///
/// `#[allow(dead_code)]` on the trait because no implementation
/// exists yet and nothing calls this. Removed once we wire it up.
#[allow(dead_code)]
pub trait BlobCache: Send + Sync {
    /// Look up a blob by hash. Returns `None` on miss.
    fn get(&self, key: &BlobKey) -> Option<Blob>;

    /// Insert a blob. Caller guarantees `key` is the hash of the
    /// bytes — the cache doesn't re-verify.
    fn insert(&self, key: BlobKey, blob: Blob);
}
