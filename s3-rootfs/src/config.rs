//! Filesystem configuration.
//!
//! `FsConfig` bundles the tunables a caller might want to set when
//! constructing an `S3Fs`: where to find S3, how much to cache, how
//! far to read ahead. Default values are documented alongside each
//! field.
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

use crate::S3Config;

/// All the knobs for building an `S3Fs`.
///
/// Defaults are chosen for the coding-agent microVM workload:
/// - 16 MB read cache fits nearly all source files, build artifacts,
///   and package manifests in one GET each.
/// - 16 MB read-ahead keeps large sequential reads saturated with
///   one in-flight request at a time.
///
/// The inode cache has no TTL. S3Fs is the single writer to its
/// bucket, so cached entries are authoritative by construction; there
/// is no external writer whose changes we could miss. The memory
/// footprint (a few MB over a VM's lifetime) is well under any
/// threshold where reaping would pay for itself.
///
/// There is no `BlobCacheConfig`. The golden blob cache (research/12)
/// is either populated in full at orchestrator init or not active —
/// nothing to tune at the per-VM `FsConfig` level. When Phase 2 lands,
/// `S3Fs::new` will take an optional `Arc<GoldenBlobCache>` alongside
/// `FsConfig`, not an embedded config struct.
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
}

impl Default for FsConfig {
    fn default() -> Self {
        Self {
            s3: S3Config::default(),
            read_cache_max: 16 * 1024 * 1024,
            read_ahead_size: 16 * 1024 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_sane() {
        let cfg = FsConfig::default();
        assert!(cfg.read_cache_max > 0);
        assert!(cfg.read_ahead_size > 0);
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
