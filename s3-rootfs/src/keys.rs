// S3 key format: path\x01<mode_hex><uid_hex><gid_hex>
//
// Embeds POSIX metadata directly in the key so a single LIST tells us
// everything about a file — type, permissions, ownership — without HEAD.
//
// The \x01 separator sorts before '/' in S3's lexicographic ordering,
// which means a directory's key sorts immediately before its children.
// This enables pre-fill: a LIST starting at a key naturally flows into
// the directory's contents.

const META_SEP: u8 = 0x01;
const META_LEN: usize = 20; // 4 (mode) + 8 (uid) + 8 (gid) hex chars

/// Metadata extracted from an S3 key
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyMeta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl KeyMeta {
    pub fn new(mode: u32, uid: u32, gid: u32) -> Self {
        Self { mode, uid, gid }
    }

    /// File type bits from st_mode (S_IFMT mask)
    pub fn file_type(&self) -> u32 {
        self.mode & libc::S_IFMT as u32
    }
}

/// Encode a filesystem path + metadata into an S3 key.
///
/// Example: encode("usr/bin/bash", KeyMeta { mode: 0o100755, uid: 0, gid: 0 })
///   → "usr/bin/bash\x0181ed0000000000000000"
pub fn encode(path: &str, meta: KeyMeta) -> String {
    let mut key = String::with_capacity(path.len() + 1 + META_LEN);
    key.push_str(path);
    key.push(META_SEP as char);
    // mode: 4 hex chars (16-bit value from st_mode)
    key.push_str(&format!("{:04x}", meta.mode & 0xFFFF));
    // uid: 8 hex chars
    key.push_str(&format!("{:08x}", meta.uid));
    // gid: 8 hex chars
    key.push_str(&format!("{:08x}", meta.gid));
    key
}

/// Decode an S3 key into (path, metadata).
///
/// Returns None if the key doesn't contain the metadata separator.
pub fn decode(key: &str) -> Option<(&str, KeyMeta)> {
    let sep_pos = key.rfind(META_SEP as char)?;
    let path = &key[..sep_pos];
    let meta_str = &key[sep_pos + 1..];

    if meta_str.len() != META_LEN {
        return None;
    }

    let mode = u32::from_str_radix(&meta_str[0..4], 16).ok()?;
    let uid = u32::from_str_radix(&meta_str[4..12], 16).ok()?;
    let gid = u32::from_str_radix(&meta_str[12..20], 16).ok()?;

    Some((path, KeyMeta { mode, uid, gid }))
}

/// Build the LIST prefix for looking up a specific path.
/// Appends \x01 so we only match keys for this exact path.
pub fn list_prefix(path: &str) -> String {
    let mut prefix = String::with_capacity(path.len() + 1);
    prefix.push_str(path);
    prefix.push(META_SEP as char);
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_file() {
        let meta = KeyMeta::new(0o100755, 0, 0);
        let key = encode("usr/bin/bash", meta);
        assert!(key.contains('\x01'));

        let (path, decoded) = decode(&key).unwrap();
        assert_eq!(path, "usr/bin/bash");
        assert_eq!(decoded.mode, 0o100755 & 0xFFFF);
        assert_eq!(decoded.file_type(), libc::S_IFREG as u32);
        assert_eq!(decoded.uid, 0);
        assert_eq!(decoded.gid, 0);
    }

    #[test]
    fn test_encode_decode_dir() {
        let meta = KeyMeta::new(0o040755, 0, 0);
        let key = encode("usr/bin", meta);

        let (path, decoded) = decode(&key).unwrap();
        assert_eq!(path, "usr/bin");
        assert_eq!(decoded.file_type(), libc::S_IFDIR as u32);
    }

    #[test]
    fn test_encode_decode_symlink() {
        let meta = KeyMeta::new(0o120777, 0, 0);
        let key = encode("sbin", meta);

        let (path, decoded) = decode(&key).unwrap();
        assert_eq!(path, "sbin");
        assert_eq!(decoded.file_type(), libc::S_IFLNK as u32);
    }

    #[test]
    fn test_sort_order() {
        // Directory entry should sort before its children
        let dir = encode("usr/bin", KeyMeta::new(0o040755, 0, 0));
        let child = encode("usr/bin/bash", KeyMeta::new(0o100755, 0, 0));
        assert!(dir < child, "dir key should sort before child: {} vs {}", dir, child);
    }

    #[test]
    fn test_list_prefix() {
        let prefix = list_prefix("usr/bin/bash");
        assert_eq!(prefix, "usr/bin/bash\x01");
    }

    #[test]
    fn test_decode_invalid() {
        assert!(decode("no-separator").is_none());
        assert!(decode("path\x01short").is_none());
    }
}
