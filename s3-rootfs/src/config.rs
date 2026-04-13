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

use std::path::PathBuf;

use crate::S3Config;

/// Configuration for the golden filesystem layer (research/12).
///
/// Opt-in: the only supported construction is explicit — there's no
/// `Default` impl, because enabling the golden layer is a deliberate
/// infrastructure decision, not a sensible silent default. An `S3Fs`
/// with `golden = None` behaves exactly as it does today: single per-VM
/// bucket, no shared caches.
///
/// Credentials are deliberately **not** a field here. Fleet identity
/// is one set of credentials per orchestrator, shared across every
/// bucket it touches (per-VM and golden). Per-bucket credentials are
/// out of scope — the `S3Client` bucket-override methods (`get_from`)
/// reuse the caller's global credentials.
#[derive(Debug, Clone)]
pub struct GoldenConfig {
    /// Name of the golden bucket on the same S3 endpoint as the per-VM
    /// bucket. Separate namespace: content-addressed blobs plus a
    /// `.manifest` describing the filesystem shape.
    pub bucket: String,

    /// Where to find the manifest describing the golden rootfs.
    pub manifest: ManifestSource,
}

/// Where `GoldenConfig::manifest` can live. `S3Key` is the production
/// shape (orchestrator reads the manifest from the same bucket it
/// GETs blobs from); `LocalPath` exists so dev/test/offline scenarios
/// can work without a running S3 endpoint.
#[derive(Debug, Clone)]
pub enum ManifestSource {
    /// Object key within the golden bucket. Convention: `.manifest`.
    S3Key(String),
    /// Absolute or workspace-relative path on the host filesystem.
    LocalPath(PathBuf),
}

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
/// nothing to tune at the per-VM `FsConfig` level.
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

    /// Golden filesystem layer (research/12). `None` means single-
    /// bucket behavior: every lookup/read goes through the per-VM
    /// bucket. `Some` means `S3Fs::new` will load the golden inode
    /// and blob caches synchronously at construction time (blocking
    /// until both are populated), and every FUSE op will consult
    /// them alongside the per-VM state.
    pub golden: Option<GoldenConfig>,
}

impl Default for FsConfig {
    fn default() -> Self {
        Self {
            s3: S3Config::default(),
            read_cache_max: 16 * 1024 * 1024,
            read_ahead_size: 16 * 1024 * 1024,
            golden: None,
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
