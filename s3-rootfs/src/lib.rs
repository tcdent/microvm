//! S3-backed FUSE filesystem for VM root filesystems.
//!
//! Implements the `fuse-backend-rs` `FileSystem` trait with S3 as the
//! backing store. Each file is an S3 object, directories are zero-byte
//! objects with trailing `/`. Designed to be mounted as a VM's root
//! filesystem via virtiofs, but works with any FUSE transport.
//!
//! # Example
//!
//! ```no_run
//! use s3_rootfs::S3Fs;
//! use s3_rootfs::config::FsConfig;
//!
//! let fs = S3Fs::new(FsConfig::default());
//! // Pass `fs` to fuse-backend-rs Server or virtiofs device.
//! ```

pub mod blob_cache;
pub mod config;
pub mod inode_cache;
pub mod keys;
pub mod read_ahead;
pub mod read_cache;
pub mod ruleset;
mod s3;
mod s3fs;
pub mod storage;
pub mod write_pool;

pub use blob_cache::{
    GoldenBlobCache, GoldenEntry, GoldenInodeCache, GoldenManifest, GoldenManifestEntry,
    load_golden,
};
pub use config::{GoldenConfig, ManifestSource};
pub use inode_cache::{CachedInode, InodeCache};
pub use keys::{KeyMeta, encode as encode_key, decode as decode_key, list_prefix};
pub use s3::{S3Client, S3Config, S3Error, ListResult, ListObject};
pub use s3fs::{S3Fs, CacheStats, LIST_PROBE_MAX_KEYS};

use std::sync::Mutex;

// Simple logging hook — consumers can set this to route log messages
// to their own logging system.
static LOG_FN: Mutex<Option<Box<dyn Fn(&str) + Send>>> = Mutex::new(None);

/// Set a logging function for the crate. If not set, logs are discarded.
pub fn set_log_fn(f: impl Fn(&str) + Send + 'static) {
    *LOG_FN.lock().unwrap() = Some(Box::new(f));
}

#[doc(hidden)]
pub fn _log(msg: &str) {
    if let Ok(guard) = LOG_FN.lock() {
        if let Some(f) = guard.as_ref() {
            f(msg);
        }
    }
}

#[macro_export]
#[doc(hidden)]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::_log(&format!($($arg)*))
    };
}

/// Debug log — compiles to nothing in release builds.
/// Use for hot-path tracing (lookup, readdir, read, cache hits/misses).
/// Keep `log!` for S3 requests, errors, and lifecycle events.
#[cfg(debug_assertions)]
#[macro_export]
#[doc(hidden)]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::_log(&format!($($arg)*))
    };
}

#[cfg(not(debug_assertions))]
#[macro_export]
#[doc(hidden)]
macro_rules! debug {
    ($($arg:tt)*) => {};
}
