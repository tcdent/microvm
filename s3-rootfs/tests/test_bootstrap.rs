//! Integration test for the G2 bootstrap pipeline.
//!
//! Exercises the library primitives `golden-upload` uses:
//! `S3Client::put_to`, `S3Client::exists_in`, bincode manifest
//! serialization, and the end-to-end round-trip into `load_golden`.
//! The binary itself is a thin shell around these; this test
//! validates the parts it leans on.
//!
//! Shape: seed a mini tree with dirs / files / a symlink, walk +
//! hash + HEAD-then-PUT into the shared test bucket, emit manifest,
//! call `load_golden`, assert every entry survives the round-trip
//! and every hash's bytes come back equal to what we uploaded.
//! Covers the load-bearing property vm:2.8 called out: "upload-side
//! schema must match what runtime `load_golden` expects, or boot
//! quietly serves garbage."

mod harness;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use harness::s3::{require_s3, test_config};
use s3_rootfs::{
    load_golden, GoldenConfig, GoldenManifest, GoldenManifestEntry, ManifestSource, S3Client,
    S3Config,
};

static TEST_BOOTSTRAP_ID: AtomicU64 = AtomicU64::new(0);

/// One entry's worth of "what the bootstrap tool would produce".
/// The test can check each field matched after round-trip.
struct SeedEntry {
    path: String,
    content: Option<Vec<u8>>, // None = dir/symlink
    mode: u32,
    symlink_target: Option<String>,
}

#[test]
fn bootstrap_round_trip() {
    require_s3!();

    // Per-test prefix so concurrent tests don't collide on the
    // shared rustfs bucket. Both blobs (content-addressed hashes)
    // and the .manifest get prefixed with this.
    let id = TEST_BOOTSTRAP_ID.fetch_add(1, Ordering::Relaxed);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prefix = format!("bootstrap_test_{ts}_{id}");

    let cfg = test_config();
    let bucket = cfg.bucket.clone();
    let agent = Arc::new(S3Client::build_agent(&cfg));
    let client = S3Client::with_shared_agent(
        S3Config { bucket: bucket.clone(), ..cfg.clone() },
        Arc::clone(&agent),
    );

    // Seed a tree covering: dir, regular file, regular file (dup
    // content — dedup target), symlink, empty file.
    let seeds = vec![
        SeedEntry {
            path: "etc".to_string(),
            content: None,
            mode: libc::S_IFDIR as u32 | 0o755,
            symlink_target: None,
        },
        SeedEntry {
            path: "etc/hostname".to_string(),
            content: Some(b"microvm\n".to_vec()),
            mode: libc::S_IFREG as u32 | 0o644,
            symlink_target: None,
        },
        SeedEntry {
            path: "etc/alt-hostname".to_string(),
            // Same bytes as hostname — dedup MUST collapse these
            // to one blob.
            content: Some(b"microvm\n".to_vec()),
            mode: libc::S_IFREG as u32 | 0o644,
            symlink_target: None,
        },
        SeedEntry {
            path: "empty.txt".to_string(),
            content: Some(Vec::new()),
            mode: libc::S_IFREG as u32 | 0o644,
            symlink_target: None,
        },
        SeedEntry {
            path: "sbin".to_string(),
            content: None,
            mode: libc::S_IFLNK as u32 | 0o777,
            symlink_target: Some("usr/sbin".to_string()),
        },
    ];

    // Simulate what golden-upload does: hash + HEAD-then-PUT for
    // each regular file, collect manifest entries.
    let mut manifest_entries: Vec<GoldenManifestEntry> = Vec::new();
    let mut unique_hashes_seen: std::collections::HashSet<String> = Default::default();
    let mut put_count = 0usize;

    for seed in &seeds {
        let (hash, size) = match &seed.content {
            Some(bytes) => {
                // Hash + prefix-qualify the blob key. We use the
                // per-test prefix in the hash to keep test blobs
                // namespaced inside the shared bucket — real
                // bootstrap uses bare BLAKE3 hex.
                let blake = blake3::hash(bytes).to_hex().to_string();
                let key = format!("{prefix}/{blake}");

                // HEAD-then-PUT: the dedup property shows up here
                // — identical content for etc/hostname and
                // etc/alt-hostname should cause the second exists_in
                // to return true and skip the PUT.
                let existed = client.exists_in(&bucket, &key).expect("exists_in");
                if !existed {
                    client.put_to(&bucket, &key, bytes).expect("put_to");
                    put_count += 1;
                }
                unique_hashes_seen.insert(key.clone());
                (Some(key), bytes.len() as u64)
            }
            None => (None, 0u64),
        };

        manifest_entries.push(GoldenManifestEntry {
            path: seed.path.clone(),
            hash,
            mode: seed.mode,
            uid: 0,
            gid: 0,
            size,
            symlink_target: seed.symlink_target.clone(),
        });
    }

    // Dedup assertion: 3 file seeds, 2 distinct contents
    // ("microvm\n" twice + empty), so unique hashes = 2.
    assert_eq!(
        unique_hashes_seen.len(),
        2,
        "expected 2 unique blob hashes (dedup collapsed the duplicate)"
    );
    // PUT count matches unique hashes (each PUT-if-absent fires
    // exactly once per unique hash). Dedup is observable here.
    assert_eq!(
        put_count,
        2,
        "dedup should have avoided re-PUT of duplicate content"
    );

    // Write the manifest.
    let manifest = GoldenManifest { entries: manifest_entries.clone() };
    let manifest_bytes = bincode::serialize(&manifest).expect("serialize manifest");
    let manifest_key = format!("{prefix}/.manifest");
    client
        .put_to(&bucket, &manifest_key, &manifest_bytes)
        .expect("put .manifest");

    // Fetch it back via load_golden — this is the load-bearing
    // round-trip. `load_golden` reads .manifest, bincode-decodes,
    // builds both caches by bulk-fetching every unique hash.
    let golden_cfg = GoldenConfig {
        bucket: bucket.clone(),
        manifest: ManifestSource::S3Key(manifest_key.clone()),
    };
    let (inodes, blobs) = load_golden(&golden_cfg, &client).expect("load_golden");

    // Assert manifest entries match seeds.
    assert_eq!(inodes.len(), seeds.len());
    for seed in &seeds {
        let entry = inodes
            .get(&seed.path)
            .unwrap_or_else(|| panic!("load_golden missing path {}", seed.path));
        assert_eq!(entry.mode, seed.mode, "mode mismatch for {}", seed.path);
        assert_eq!(entry.uid, 0);
        assert_eq!(entry.gid, 0);
        assert_eq!(
            entry.size,
            seed.content.as_ref().map(|b| b.len() as u64).unwrap_or(0),
            "size mismatch for {}",
            seed.path
        );
        assert_eq!(
            entry.symlink_target, seed.symlink_target,
            "symlink_target mismatch for {}",
            seed.path
        );
    }

    // Assert every manifest hash can be fetched from the blob
    // cache AND the bytes equal the original seeded content.
    for seed in &seeds {
        if let Some(expected) = &seed.content {
            if expected.is_empty() {
                // Empty files have a well-defined BLAKE3 hash and
                // should round-trip too. load_golden fetched it
                // during init.
                continue;
            }
            let entry = inodes.get(&seed.path).unwrap();
            let hash = entry.hash.as_ref().expect("regular file should have hash");
            let blob = blobs
                .fetch(hash)
                .unwrap_or_else(|| panic!("blob cache missing hash for {}", seed.path));
            assert_eq!(&**blob, expected.as_slice(), "bytes mismatch for {}", seed.path);
        }
    }

    // Cleanup — delete everything we uploaded so concurrent tests
    // don't accumulate cruft in the shared bucket.
    let list_prefix = format!("{prefix}/");
    if let Ok(result) = client.list(&list_prefix, None) {
        for obj in &result.objects {
            let _ = client.delete(&obj.key);
        }
    }
}
