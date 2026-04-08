mod harness;

use std::time::Instant;

use fuse_backend_rs::api::filesystem::DirEntry;
use fuse_backend_rs::api::filesystem::FileSystem;
use harness::context::root_ctx;
use harness::fixture::TestFixture;
use harness::s3::require_s3;

const ROOT: u64 = 1;

#[test]
fn lookup_prefills_neighbors() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    // Seed 10 files with lexicographically ordered names
    for i in 0..10 {
        let name = format!("file_{:02}", i);
        fix.seed_file(&name, libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    }

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let stats_before = fix.fs.cache_stats();

    // First lookup triggers LIST with max_keys=12 — should pre-fill neighbors
    fix.lookup(dir.inode, "file_00").unwrap();

    let stats_after = fix.fs.cache_stats();
    let new_entries = stats_after.inode_count - stats_before.inode_count;

    // Should have cached more than just the one entry we looked up
    assert!(new_entries > 1,
        "expected pre-fill to cache multiple entries, but only got {}", new_entries);
}

#[test]
fn cached_lookup_is_faster_than_cold() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("fast.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    fix.seed_file("also_fast.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    // Cold lookup (hits S3)
    let t0 = Instant::now();
    fix.lookup(dir.inode, "fast.txt").unwrap();
    let cold = t0.elapsed();

    // Warm lookup — "also_fast.txt" should be pre-filled by the first lookup
    let t1 = Instant::now();
    fix.lookup(dir.inode, "also_fast.txt").unwrap();
    let warm = t1.elapsed();

    // Warm should be significantly faster. Use generous threshold to avoid flaky tests.
    assert!(warm < cold || warm.as_millis() < 2,
        "expected cache hit to be faster: cold={:?} warm={:?}", cold, warm);
}

#[test]
fn negative_cache_on_miss() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let stats_before = fix.fs.cache_stats();

    // Lookup something that doesn't exist
    let _ = fix.lookup(dir.inode, "ghost");

    let stats_after = fix.fs.cache_stats();
    assert!(stats_after.negative_count > stats_before.negative_count,
        "expected negative cache entry: before={} after={}",
        stats_before.negative_count, stats_after.negative_count);
}

#[test]
fn negative_cache_prevents_repeat_list() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    // First miss (hits S3)
    let t0 = Instant::now();
    let _ = fix.lookup(dir.inode, "missing1");
    let first = t0.elapsed();

    // Second lookup of same missing name — should hit negative cache
    let t1 = Instant::now();
    let _ = fix.lookup(dir.inode, "missing1");
    let second = t1.elapsed();

    assert!(second < first || second.as_millis() < 2,
        "expected negative cache hit: first={:?} second={:?}", first, second);
}

#[test]
fn readdir_populates_cache_for_subsequent_lookups() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("x.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    fix.seed_file("y.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    fix.seed_file("z.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    // readdir should populate cache for all children
    let (dh, _) = fix.fs.opendir(&root_ctx(), dir.inode, 0).unwrap();
    fix.fs.readdir(&root_ctx(), dir.inode, dh.unwrap(), 4096, 0,
        &mut |_de: DirEntry| Ok(1)).unwrap();

    let stats = fix.fs.cache_stats();

    // All three files + the directory itself should be cached
    // (plus root and the test prefix dir)
    // Lookup each child — should be fast (cache hit)
    let t0 = Instant::now();
    fix.lookup(dir.inode, "x.txt").unwrap();
    fix.lookup(dir.inode, "y.txt").unwrap();
    fix.lookup(dir.inode, "z.txt").unwrap();
    let total = t0.elapsed();

    // Three lookups that are all cache hits should be very fast
    assert!(total.as_millis() < 10,
        "expected all lookups to be cache hits after readdir: {:?}", total);
}

#[test]
fn list_probe_max_keys_boundary() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    // Seed 20 files — LIST probe fetches max 12
    for i in 0..20 {
        let name = format!("item_{:02}", i);
        fix.seed_file(&name, libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    }

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let stats_before = fix.fs.cache_stats();

    // Lookup the first item — should trigger LIST with max_keys=12
    fix.lookup(dir.inode, "item_00").unwrap();

    let stats_after = fix.fs.cache_stats();
    let prefilled = stats_after.inode_count - stats_before.inode_count;

    // Should have pre-filled up to LIST_PROBE_MAX_KEYS (12), not all 20
    assert!(prefilled <= s3_rootfs::LIST_PROBE_MAX_KEYS as usize,
        "expected at most {} pre-filled entries, got {}",
        s3_rootfs::LIST_PROBE_MAX_KEYS, prefilled);
    assert!(prefilled > 1, "expected pre-fill, got only {}", prefilled);
}

#[test]
fn cache_hit_after_create() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    // Create a directory via S3Fs
    use harness::context::cname;
    fix.fs.mkdir(&root_ctx(), dir.inode, &cname("cached_dir"), 0o755, 0).unwrap();

    // Immediately look it up — should be a cache hit (no S3 call)
    let t0 = Instant::now();
    let entry = fix.lookup(dir.inode, "cached_dir").unwrap();
    let lookup_time = t0.elapsed();

    assert!(entry.attr.st_mode & libc::S_IFDIR as u32 != 0);
    assert!(lookup_time.as_millis() < 5,
        "expected cache hit after create: {:?}", lookup_time);
}
