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

use crate::bucket::S3AdminClient;
use crate::config::DaemonConfig;
use crate::Error;

pub struct SharedContext {
    pub config: DaemonConfig,
    pub s3_admin: Arc<S3AdminClient>,
    // TODO: tap pool handle, log dir state.
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

        Ok(Self { config, s3_admin })
    }
}
