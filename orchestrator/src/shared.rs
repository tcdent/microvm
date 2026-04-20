//! SharedContext: orchestrator-scope state shared across VM threads
//! (research/15 §1).
//!
//! Deliberately does NOT hold s3-rootfs internals (golden caches,
//! write pool, S3 client). Per design directive, s3-rootfs is consumed
//! via its existing `S3Fs::new(cfg)` per-VM; each VM thread builds its
//! own. Hoisting those Arcs is a future optimization that would require
//! an s3-rootfs constructor change.
//!
//! What IS shared: config, log dirs, tap pool, bucket helpers —
//! infrastructure the orchestrator owns end-to-end.

use std::sync::Arc;

use s3_rootfs::{GoldenBlobCache, GoldenInodeCache, S3Client, S3Config};

use crate::bucket::S3AdminClient;
use crate::config::DaemonConfig;
use crate::Error;

pub struct SharedContext {
    pub config: DaemonConfig,
    pub s3_admin: Arc<S3AdminClient>,
    /// Pre-loaded golden caches, shared across all VMs.
    /// None if no default_golden_bucket is configured.
    pub golden_inodes: Option<Arc<GoldenInodeCache>>,
    pub golden_blobs: Option<Arc<GoldenBlobCache>>,
}

impl SharedContext {
    pub fn build(config: DaemonConfig) -> Result<Self, Error> {
        let s3_admin = Arc::new(S3AdminClient::new(
            &config.s3_endpoint,
            &config.s3_admin_access_key,
            &config.s3_admin_secret_key,
        ));

        // Ensure infrastructure dirs exist.
        let _ = std::fs::create_dir_all(&config.log_dir);
        let _ = std::fs::create_dir_all(&config.ephemeral_root);

        // Pre-load golden caches once for sharing across all VMs.
        //
        // Retry with exponential backoff on failure: production
        // deployments commonly have microd start before the S3
        // backend is fully ready (Nomad task ordering, systemd
        // After=, etc.), and a one-shot load that fell through to
        // per-VM mode would silently lose the cache-sharing win for
        // the whole daemon lifetime. Backoff is bounded so a
        // persistently-broken S3 doesn't hang startup forever —
        // eventually we fall through to per-VM as before.
        let (golden_inodes, golden_blobs) = match &config.default_golden_bucket {
            Some(bucket) => {
                let s3_cfg = S3Config {
                    endpoint: config.s3_endpoint.clone(),
                    bucket: bucket.clone(),
                    access_key: config.s3_admin_access_key.clone(),
                    secret_key: config.s3_admin_secret_key.clone(),
                    ..S3Config::default()
                };
                let client = S3Client::new(s3_cfg);
                let golden_cfg = s3_rootfs::GoldenConfig {
                    bucket: bucket.clone(),
                    manifest: s3_rootfs::ManifestSource::S3Key(".manifest".to_string()),
                };

                let backoffs = [1, 2, 4, 8, 15]; // seconds; ~30s total
                let mut result = None;
                for (attempt, delay) in std::iter::once(&0u64)
                    .chain(backoffs.iter())
                    .enumerate()
                {
                    if *delay > 0 {
                        eprintln!(
                            "SharedContext: golden load retry {}/{} in {}s",
                            attempt, backoffs.len(), delay
                        );
                        std::thread::sleep(std::time::Duration::from_secs(*delay));
                    }
                    match s3_rootfs::load_golden(&golden_cfg, &client) {
                        Ok(pair) => {
                            eprintln!("SharedContext: loaded golden caches from {}", bucket);
                            result = Some(pair);
                            break;
                        }
                        Err(e) => {
                            eprintln!(
                                "SharedContext: golden load attempt {} failed: {}",
                                attempt, e
                            );
                        }
                    }
                }

                match result {
                    Some((inodes, blobs)) => (Some(inodes), Some(blobs)),
                    None => {
                        eprintln!(
                            "SharedContext: golden load gave up after retries, per-VM fallback"
                        );
                        (None, None)
                    }
                }
            }
            None => (None, None),
        };

        Ok(Self { config, s3_admin, golden_inodes, golden_blobs })
    }
}
