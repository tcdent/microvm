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
//! use s3_rootfs::{S3Fs, S3Config};
//!
//! let fs = S3Fs::new(S3Config::default());
//! // Pass `fs` to fuse-backend-rs Server or virtiofs device
//! ```

pub mod cache;
pub mod keys;
mod s3;
mod s3fs;

pub use cache::{CachedInode, InodeCache};
pub use keys::{KeyMeta, encode as encode_key, decode as decode_key, list_prefix};
pub use s3::{S3Client, S3Config, S3Error, ObjectMeta, ListResult, ListObject};
pub use s3fs::S3Fs;

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
