use std::sync::atomic::{AtomicU64, Ordering};

use fuse_backend_rs::abi::fuse_abi::FsOptions;
use fuse_backend_rs::api::filesystem::FileSystem;

use s3_rootfs::config::FsConfig;
use s3_rootfs::{
    GoldenConfig, GoldenManifest, GoldenManifestEntry, KeyMeta, ManifestSource, S3Client, S3Fs,
    encode_key,
};

use super::s3::test_config;

/// Bench-side copy of the test-harness `GoldenSeed`. Kept lean —
/// only the flavors the benches actually use.
pub struct GoldenSeed {
    pub path: String,
    pub content: Vec<u8>,
    pub mode: u32,
}

impl GoldenSeed {
    pub fn file(path: &str, mode: u32, content: Vec<u8>) -> Self {
        Self { path: path.to_string(), content, mode }
    }
    pub fn dir(path: &str) -> Self {
        Self {
            path: path.to_string(),
            content: Vec::new(),
            mode: libc::S_IFDIR as u32 | 0o755,
        }
    }
}

static TEST_ID: AtomicU64 = AtomicU64::new(0);

pub struct TestFixture {
    pub fs: S3Fs,
    pub client: S3Client,
    pub prefix: String,
}

impl TestFixture {
    /// Bench-side parallel to `tests::with_golden`. Seeds blobs at
    /// per-fixture-unique keys into the shared test bucket, writes a
    /// bincode manifest to a tempfile, constructs `S3Fs` with
    /// `golden: Some(LocalPath)`. Returns the fixture + a tempdir
    /// guard (tests/benches must keep the guard alive as long as the
    /// fixture, because `load_golden` reads the manifest lazily at
    /// construction but the bench iter doesn't re-load).
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
                symlink_target: None,
            });
        }

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
        let fixture = Self { fs, client: verify, prefix };
        (fixture, tmp)
    }

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

        Self { fs, client, prefix }
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
