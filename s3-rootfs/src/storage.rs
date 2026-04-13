//! S3 object body interpretation and key construction.
//!
//! All decisions about "what's in an S3 object's body" live here:
//! whether it's direct file data, a hardlink reference, a future
//! golden-ref pointer, or a symlink target. The rest of the codebase
//! (`s3fs.rs`) calls [`parse_body`] and matches on the enum — it
//! doesn't parse body bytes directly.
//!
//! Key helpers for objects stored outside the primary filesystem
//! tree (hardlink data pool, golden-image pool) also live here so
//! there's one place that knows about the `.hardlinks/` and future
//! golden-ref layouts.
//!
//! # Scaffold
//!
//! This file is a scaffold for step 2 of the refactor
//! (research/11-s3-rootfs-refactor-plan.md §8). Behavior currently
//! lives inline in `s3fs.rs`; step 5 moves it here. The enum and
//! encode/decode helpers are defined now so downstream code can
//! start using them.

use crate::keys::{encode, KeyMeta};

/// Prefix of the hidden directory that stores hardlink-shared data.
/// Keys under this prefix are the "real" data; visible filesystem
/// paths become references pointing at them via body `hardlink:<uuid>`.
pub const HARDLINK_PREFIX: &str = ".hardlinks/";

/// Magic prefix placed at the start of an object body to mark it as
/// a hardlink reference rather than direct file data. The rest of
/// the body is the UUID naming a file under [`HARDLINK_PREFIX`].
pub const HARDLINK_REF_PREFIX: &str = "hardlink:";

/// Is `name` a top-level entry reserved by the storage layer and hidden
/// from guest-visible readdir? Currently just the `.hardlinks` directory
/// that stores shared hardlink data.
pub fn is_reserved_name(name: &str) -> bool {
    name == HARDLINK_PREFIX.trim_end_matches('/')
}

/// Decoded interpretation of an S3 object body.
///
/// Which variant applies depends on both the key metadata (file type
/// via `S_IFMT` bits) and the body bytes. Symlinks always have the
/// target path as their body; regular files are `Direct` unless the
/// body starts with a known reference magic prefix.
#[derive(Debug, Clone)]
pub enum ObjectBody {
    /// Direct file content.
    Direct(Vec<u8>),

    /// A hardlink reference. The string is the UUID naming the real
    /// data object under `.hardlinks/<uuid>`.
    Hardlink(String),

    /// Symlink target path. For `S_IFLNK` objects, the body *is* the
    /// target — no prefix, no framing.
    Symlink(String),
}

/// Interpret a raw object body given the file type from the key.
///
/// `file_type` is `mode & S_IFMT` (values like `libc::S_IFREG`,
/// `libc::S_IFLNK`). We look at the type first to decide whether to
/// treat the body as a symlink target, then fall back to checking
/// regular-file magic prefixes.
pub fn parse_body(bytes: Vec<u8>, file_type: u32) -> ObjectBody {
    // Symlink target: body is just the path, no framing.
    if file_type == libc::S_IFLNK as u32 {
        return ObjectBody::Symlink(String::from_utf8_lossy(&bytes).into_owned());
    }

    // Regular files may be references rather than direct data.
    if let Some(uuid) = strip_prefix_str(&bytes, HARDLINK_REF_PREFIX) {
        return ObjectBody::Hardlink(uuid.trim().to_string());
    }

    ObjectBody::Direct(bytes)
}

/// Encode a hardlink reference body.
pub fn encode_hardlink_ref(uuid: &str) -> Vec<u8> {
    format!("{HARDLINK_REF_PREFIX}{uuid}").into_bytes()
}

/// Generate a new hardlink UUID.
///
/// Combines a monotonic timestamp and a process-wide counter so that
/// no two calls produce the same value even in the same nanosecond.
/// Not a real UUID (no RFC4122 format) — just a collision-free
/// identifier that's cheap to generate. Callers only care about
/// uniqueness, not parseability.
pub fn new_hardlink_uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{ts:016x}{seq:016x}")
}

/// Build the S3 key for a hardlink data object.
///
/// Data lives at `.hardlinks/<uuid>` with the same metadata encoding
/// as any other key. Multiple visible paths can reference the same
/// UUID and thus share the data.
pub fn hardlink_key(uuid: &str, meta: KeyMeta) -> String {
    let path = format!("{HARDLINK_PREFIX}{uuid}");
    encode(&path, meta)
}


/// Helper: if `bytes` starts with `prefix` (as UTF-8), return the
/// suffix as `&str`. Returns `None` if the bytes aren't valid UTF-8
/// or don't start with the prefix.
fn strip_prefix_str<'a>(bytes: &'a [u8], prefix: &str) -> Option<&'a str> {
    let s = std::str::from_utf8(bytes).ok()?;
    s.strip_prefix(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_direct_file() {
        let body = parse_body(b"hello".to_vec(), libc::S_IFREG as u32);
        match body {
            ObjectBody::Direct(b) => assert_eq!(b, b"hello"),
            _ => panic!("expected Direct"),
        }
    }

    #[test]
    fn parse_symlink() {
        let body = parse_body(b"../target".to_vec(), libc::S_IFLNK as u32);
        match body {
            ObjectBody::Symlink(s) => assert_eq!(s, "../target"),
            _ => panic!("expected Symlink"),
        }
    }

    #[test]
    fn parse_hardlink_ref() {
        let body = parse_body(b"hardlink:abc123".to_vec(), libc::S_IFREG as u32);
        match body {
            ObjectBody::Hardlink(uuid) => assert_eq!(uuid, "abc123"),
            _ => panic!("expected Hardlink"),
        }
    }

    #[test]
    fn encode_hardlink_ref_roundtrip() {
        let encoded = encode_hardlink_ref("abc123");
        assert_eq!(encoded, b"hardlink:abc123");
        let parsed = parse_body(encoded, libc::S_IFREG as u32);
        match parsed {
            ObjectBody::Hardlink(uuid) => assert_eq!(uuid, "abc123"),
            _ => panic!("expected Hardlink"),
        }
    }

    #[test]
    fn uuid_is_unique_enough() {
        let a = new_hardlink_uuid();
        let b = new_hardlink_uuid();
        assert_ne!(a, b);
        // 32 hex chars = 128 bits of identifier space in the string form.
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn hardlink_key_contains_prefix() {
        let key = hardlink_key("abc", KeyMeta::new(0o100644, 0, 0));
        assert!(key.starts_with(HARDLINK_PREFIX));
        assert!(key.contains("abc"));
    }
}
