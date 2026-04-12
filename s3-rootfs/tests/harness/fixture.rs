use std::sync::atomic::{AtomicU64, Ordering};

use fuse_backend_rs::abi::fuse_abi::FsOptions;
use fuse_backend_rs::api::filesystem::FileSystem;

use s3_rootfs::config::FsConfig;
use s3_rootfs::{S3Fs, S3Client, KeyMeta, encode_key};

use super::s3::test_config;

static TEST_ID: AtomicU64 = AtomicU64::new(0);

pub struct TestFixture {
    pub fs: S3Fs,
    pub client: S3Client,
    pub prefix: String,
    /// Whether this fixture is responsible for cleaning up objects under
    /// `prefix` on drop. True for fixtures created via `new()` (which own
    /// a unique prefix); false for fixtures created via `attach_to()`
    /// (which share another fixture's prefix). Without this gate, the
    /// first fixture to drop would delete objects the surviving one still
    /// needs, so tests that simulate a cold session via attach_to would
    /// break intermittently depending on drop order.
    owns_prefix: bool,
}

impl TestFixture {
    pub fn new() -> Self {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let prefix = format!("t_{}_{}", ts, id);

        let config = test_config();
        let verify_config = test_config();

        let fs = S3Fs::new(FsConfig { s3: config, ..Default::default() });
        fs.init(FsOptions::empty()).expect("S3Fs init failed");

        let client = S3Client::new(verify_config);

        Self { fs, client, prefix, owns_prefix: true }
    }

    /// Build a second fixture pointing at the same bucket + prefix as an
    /// existing one, with fresh S3Fs / S3Client instances (empty caches,
    /// fresh handle table). Simulates a cold session against data written
    /// by another process — the test analogue of "unmount, remount".
    ///
    /// The returned fixture does NOT own the prefix: it will not run the
    /// cleanup sweep on drop. The original fixture (owns_prefix=true)
    /// handles cleanup. Keep the original alive at least until the
    /// attached fixture is done — if the original drops first, the
    /// attached fixture sees missing data.
    pub fn attach_to(other: &TestFixture) -> Self {
        let config = test_config();
        let verify_config = test_config();

        let fs = S3Fs::new(FsConfig { s3: config, ..Default::default() });
        fs.init(FsOptions::empty()).expect("S3Fs init failed");

        let client = S3Client::new(verify_config);

        Self {
            fs,
            client,
            prefix: other.prefix.clone(),
            owns_prefix: false,
        }
    }

    /// Build a full path under this fixture's unique prefix.
    pub fn path(&self, rel: &str) -> String {
        if rel.is_empty() {
            self.prefix.clone()
        } else {
            format!("{}/{}", self.prefix, rel)
        }
    }

    /// Seed a regular file into S3 directly.
    pub fn seed_file(&self, rel_path: &str, mode: u32, uid: u32, gid: u32, data: &[u8]) {
        let path = self.path(rel_path);
        let meta = KeyMeta::new(mode, uid, gid);
        let key = encode_key(&path, meta);
        self.client.put(&key, data).expect("seed_file PUT failed");
    }

    /// Seed a directory marker.
    pub fn seed_dir(&self, rel_path: &str) {
        let path = self.path(rel_path);
        let meta = KeyMeta::new(libc::S_IFDIR as u32 | 0o755, 0, 0);
        let key = encode_key(&path, meta);
        self.client.put(&key, &[]).expect("seed_dir PUT failed");
    }

    /// Seed the fixture's root directory marker.
    pub fn seed_root(&self) {
        let meta = KeyMeta::new(libc::S_IFDIR as u32 | 0o755, 0, 0);
        let key = encode_key(&self.prefix, meta);
        self.client.put(&key, &[]).expect("seed_root PUT failed");
    }

    /// Seed a symlink.
    pub fn seed_symlink(&self, rel_path: &str, target: &str) {
        let path = self.path(rel_path);
        let meta = KeyMeta::new(libc::S_IFLNK as u32 | 0o777, 0, 0);
        let key = encode_key(&path, meta);
        self.client.put(&key, target.as_bytes()).expect("seed_symlink PUT failed");
    }

    /// Lookup a name under a parent inode. Convenience wrapper.
    pub fn lookup(&self, parent: u64, name: &str) -> std::io::Result<fuse_backend_rs::api::filesystem::Entry> {
        use super::context::{root_ctx, cname};
        self.fs.lookup(&root_ctx(), parent, &cname(name))
    }

    /// Navigate from root (inode 1) through a slash-separated path.
    /// Returns the final inode's Entry.
    pub fn walk(&self, path: &str) -> std::io::Result<fuse_backend_rs::api::filesystem::Entry> {
        let mut ino = 1u64;
        let mut entry = None;
        for component in path.split('/') {
            if component.is_empty() { continue; }
            let e = self.lookup(ino, component)?;
            ino = e.inode;
            entry = Some(e);
        }
        entry.ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        // Only the owning fixture cleans up — attached fixtures share the
        // prefix and must not delete out from under the owner.
        if !self.owns_prefix {
            return;
        }
        // Clean up all objects under our prefix
        let prefix = s3_rootfs::list_prefix(&self.prefix);
        if let Ok(result) = self.client.list(&prefix, None) {
            for obj in &result.objects {
                let _ = self.client.delete(&obj.key);
            }
        }
        // Also clean children under prefix/
        let child_prefix = format!("{}/", self.prefix);
        if let Ok(result) = self.client.list(&child_prefix, None) {
            for obj in &result.objects {
                let _ = self.client.delete(&obj.key);
            }
        }
    }
}
