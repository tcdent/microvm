//! Filesystem configuration.
//!
//! `FsConfig` bundles the tunables a caller might want to set when
//! constructing an `S3Fs`: where to find S3, how much to cache, how
//! far to read ahead, how long to hold cache entries before reaping.
//! Default values are documented alongside each field.
//!
//! The struct is plain — no builder, no setters. Construct with
//! `FsConfig { endpoint: ..., ..Default::default() }` which reads
//! well and matches stdlib conventions.
//!
//! # Usage
//!
//! `S3Fs::new(FsConfig::default())` for defaults, or
//! `S3Fs::new(FsConfig { read_cache_max: 8 << 20, ..Default::default() })`
//! to tweak a single knob. This is the only supported construction
//! path — the older `with_options` / `with_read_cache` helpers are
//! gone.

use std::time::Duration;

use crate::S3Config;

/// All the knobs for building an `S3Fs`.
///
/// Defaults are chosen for the coding-agent microVM workload:
/// - 16 MB read cache fits nearly all source files, build artifacts,
///   and package manifests in one GET each.
/// - 16 MB read-ahead keeps large sequential reads saturated with
///   one in-flight request at a time.
/// - 5 min cache TTL reaps entries nobody's accessed; the cache is
///   always correct (single-writer) so this is memory management,
///   not consistency.
#[derive(Debug, Clone)]
pub struct FsConfig {
    /// S3 endpoint, bucket, credentials, timeouts.
    pub s3: S3Config,

    /// File-size threshold for whole-file caching in the handle.
    /// Reads of files at or below this size fetch everything in one
    /// GET and cache it in the `FileHandle`. Above this, we range-GET
    /// via the read-ahead window.
    pub read_cache_max: u64,

    /// Window size for sequential read-ahead on files above
    /// `read_cache_max`. A single GET fetches up to this many bytes;
    /// subsequent reads within the window hit memory.
    pub read_ahead_size: usize,

    /// How long an inode cache entry stays resident before becoming
    /// eligible for reap. The cache is always consistent with S3
    /// (single-writer), so this is purely memory reclamation — not
    /// a correctness/staleness control.
    pub cache_ttl: Duration,

    /// Blob-cache (content-addressed, future) configuration. Empty
    /// by default since the feature isn't wired up yet.
    pub blob_cache: BlobCacheConfig,
}

impl Default for FsConfig {
    fn default() -> Self {
        Self {
            s3: S3Config::default(),
            read_cache_max: 16 * 1024 * 1024,
            read_ahead_size: 16 * 1024 * 1024,
            cache_ttl: Duration::from_secs(300),
            blob_cache: BlobCacheConfig::default(),
        }
    }
}

/// Content-addressed blob cache configuration.
///
/// Scaffold for the future golden-image path (research/10-fs-daemon.md):
/// shared host-side cache of blob hashes → bytes, populated once and
/// read by many VMs. Not wired up today — the struct exists so
/// callers can pre-plumb config without a breaking change later.
#[derive(Debug, Clone, Default)]
pub struct BlobCacheConfig {
    /// When set, points at the host directory backing the on-disk
    /// tier of the blob cache. `None` disables blob caching entirely.
    /// TODO: define semantics when we actually implement it.
    pub cache_dir: Option<std::path::PathBuf>,

    /// Approximate cap for the in-memory (RAM) tier. `0` means no
    /// RAM cache; bytes come straight from disk/S3.
    pub ram_budget_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_sane() {
        let cfg = FsConfig::default();
        assert!(cfg.read_cache_max > 0);
        assert!(cfg.read_ahead_size > 0);
        assert!(cfg.cache_ttl.as_secs() >= 60);
        assert!(cfg.blob_cache.cache_dir.is_none());
    }

    #[test]
    fn struct_update_works() {
        let cfg = FsConfig {
            read_cache_max: 32 * 1024 * 1024,
            ..Default::default()
        };
        assert_eq!(cfg.read_cache_max, 32 * 1024 * 1024);
        // Other fields keep defaults.
        assert_eq!(cfg.read_ahead_size, FsConfig::default().read_ahead_size);
    }
}
