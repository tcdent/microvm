use std::sync::atomic::{AtomicU64, Ordering};

use fuse_backend_rs::abi::fuse_abi::FsOptions;
use fuse_backend_rs::api::filesystem::FileSystem;

use s3_rootfs::config::FsConfig;
use s3_rootfs::{
    GoldenConfig, GoldenManifest, GoldenManifestEntry, KeyMeta, ManifestSource, S3Client, S3Fs,
    encode_key,
};

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

/// Plain description of one manifest entry, used by `with_golden`.
/// Mirrors `GoldenManifestEntry` but lets tests construct entries
/// without depending on the internal hash/ino wiring.
pub struct GoldenSeed {
    pub path: String,
    /// Raw bytes to seed as `golden/<hash>`. Empty slice for dirs
    /// and symlinks (they don't have blob content).
    pub content: Vec<u8>,
    pub mode: u32,
    pub symlink_target: Option<String>,
}

impl GoldenSeed {
    pub fn file(path: &str, mode: u32, content: &[u8]) -> Self {
        Self {
            path: path.to_string(),
            content: content.to_vec(),
            mode,
            symlink_target: None,
        }
    }
    pub fn dir(path: &str) -> Self {
        Self {
            path: path.to_string(),
            content: Vec::new(),
            mode: libc::S_IFDIR as u32 | 0o755,
            symlink_target: None,
        }
    }
    pub fn symlink(path: &str, target: &str) -> Self {
        Self {
            path: path.to_string(),
            content: Vec::new(),
            mode: libc::S_IFLNK as u32 | 0o777,
            symlink_target: Some(target.to_string()),
        }
    }
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

    /// Build a `TestFixture` with the golden layer populated from
    /// the given seeds. Blobs are stored at per-test-unique keys
    /// (`<prefix>/golden_blob_<i>`) to avoid cross-test collisions
    /// within the single shared test bucket; the manifest lives on
    /// the local filesystem (ManifestSource::LocalPath), which
    /// sidesteps needing a separate S3 bucket for golden objects.
    ///
    /// Returned fixture's inode cache is already populated with the
    /// golden layer; calling `lookup` on a seed path hits tier 2.
    pub fn with_golden(seeds: Vec<GoldenSeed>) -> (Self, tempfile::TempDir) {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let prefix = format!("t_{}_{}", ts, id);

        let config = test_config();
        let bucket_name = config.bucket.clone();
        let client = S3Client::new(config.clone());

        // Seed each file blob at a per-test-unique hash key. The
        // "hash" we record in the manifest entries is the full S3
        // key minus the bucket, which `S3Client::get_from` then
        // fetches verbatim.
        let mut entries = Vec::new();
        for (i, seed) in seeds.iter().enumerate() {
            let hash = if seed.content.is_empty() {
                None
            } else {
                let key = format!("{prefix}/golden_blob_{i}");
                client
                    .put(&key, &seed.content)
                    .expect("seed_golden PUT failed");
                Some(key)
            };

            entries.push(GoldenManifestEntry {
                path: seed.path.clone(),
                hash,
                mode: seed.mode,
                uid: 0,
                gid: 0,
                size: seed.content.len() as u64,
                symlink_target: seed.symlink_target.clone(),
            });
        }

        // Serialize the manifest to a tempfile — LocalPath source
        // so we don't need a separate golden bucket for tests.
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest_path = tmp.path().join("manifest.bin");
        let manifest = GoldenManifest {
            entries,
        };
        std::fs::write(&manifest_path, bincode::serialize(&manifest).unwrap())
            .expect("write manifest");

        let golden_cfg = GoldenConfig {
            bucket: bucket_name,
            manifest: ManifestSource::LocalPath(manifest_path),
        };

        let fs = S3Fs::new(FsConfig {
            s3: config,
            golden: Some(golden_cfg),
            ..Default::default()
        });
        fs.init(FsOptions::empty()).expect("S3Fs init failed");

        let verify = S3Client::new(test_config());
        let fixture = Self {
            fs,
            client: verify,
            prefix,
            owns_prefix: true,
        };
        (fixture, tmp)
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
