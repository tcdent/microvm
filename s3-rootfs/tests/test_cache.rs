mod harness;

use std::time::Instant;

use fuse_backend_rs::api::filesystem::DirEntry;
use fuse_backend_rs::api::filesystem::FileSystem;
use harness::context::root_ctx;
use harness::fixture::TestFixture;
use harness::s3::require_s3;

use s3_rootfs::CacheStats;

const ROOT: u64 = 1;

fn print_stats(label: &str, s: &CacheStats) {
    let total = s.hits + s.misses;
    let rate = if total > 0 { s.hits as f64 / total as f64 * 100.0 } else { 0.0 };
    eprintln!("  [{label}] entries={} negative={} hits={} misses={} neg_hits={} hit_rate={:.1}%",
        s.inode_count, s.negative_count, s.hits, s.misses, s.negative_hits, rate);
}

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

/// Simulates a realistic workload: readdir then stat each child,
/// repeated access to the same files, some misses. Reports hit rates.
#[test]
fn hit_rate_realistic_workload() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_dir("src");
    for i in 0..8 {
        fix.seed_file(
            &format!("src/file_{:02}.rs", i),
            libc::S_IFREG as u32 | 0o644, 0, 0,
            format!("// file {i}").as_bytes(),
        );
    }
    fix.seed_dir("docs");
    fix.seed_file("docs/readme.md", libc::S_IFREG as u32 | 0o644, 0, 0, b"# readme");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    // Phase 1: readdir on src/ (should warm cache)
    let src = fix.lookup(dir.inode, "src").unwrap();
    let (dh, _) = fix.fs.opendir(&root_ctx(), src.inode, 0).unwrap();
    fix.fs.readdir(&root_ctx(), src.inode, dh.unwrap(), 4096, 0,
        &mut |_de: DirEntry| Ok(1)).unwrap();

    let after_readdir = fix.fs.cache_stats();
    print_stats("after readdir src/", &after_readdir);

    // Phase 2: lookup each child of src/ (should be cache hits if readdir warmed)
    for i in 0..8 {
        fix.lookup(src.inode, &format!("file_{:02}.rs", i)).unwrap();
    }

    let after_lookups = fix.fs.cache_stats();
    print_stats("after 8 lookups", &after_lookups);

    // Phase 3: repeat lookups (definitely cached)
    for i in 0..8 {
        fix.lookup(src.inode, &format!("file_{:02}.rs", i)).unwrap();
    }

    let after_repeat = fix.fs.cache_stats();
    print_stats("after repeat lookups", &after_repeat);

    // Phase 4: some misses
    for name in &["nonexistent.rs", "missing.txt", "nope"] {
        let _ = fix.lookup(src.inode, name);
    }

    let after_misses = fix.fs.cache_stats();
    print_stats("after 3 misses", &after_misses);

    // Phase 5: repeat the misses (should hit negative cache)
    for name in &["nonexistent.rs", "missing.txt", "nope"] {
        let _ = fix.lookup(src.inode, name);
    }

    let final_stats = fix.fs.cache_stats();
    print_stats("final", &final_stats);

    // Report
    let total = final_stats.hits + final_stats.misses;
    let hit_rate = if total > 0 { final_stats.hits as f64 / total as f64 * 100.0 } else { 0.0 };
    eprintln!("\n  SUMMARY: {}/{} cache hits ({:.1}%), {} negative hits",
        final_stats.hits, total, hit_rate, final_stats.negative_hits);
}

/// Simulates cold path traversal — kernel resolving an absolute path
/// for execve/shebang with no prior readdir. E.g.:
///   execve("/usr/bin/python3") → lookup("usr"), lookup("bin"), lookup("python3")
///
/// This is where pre-fill should help: the first lookup on "usr" should
/// pre-fill "usr/bin", "usr/lib", etc. from the sorted keyspace.
#[test]
fn hit_rate_cold_execve_path() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    // Simulate /usr/bin/python3 and /usr/lib/libz.so
    fix.seed_dir("usr");
    fix.seed_dir("usr/bin");
    fix.seed_dir("usr/lib");
    fix.seed_file("usr/bin/bash", libc::S_IFREG as u32 | 0o755, 0, 0, b"ELF");
    fix.seed_file("usr/bin/ls", libc::S_IFREG as u32 | 0o755, 0, 0, b"ELF");
    fix.seed_file("usr/bin/python3", libc::S_IFREG as u32 | 0o755, 0, 0, b"ELF");
    fix.seed_file("usr/lib/libz.so", libc::S_IFREG as u32 | 0o755, 0, 0, b"ELF");
    fix.seed_symlink("usr/bin/python", "python3");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let baseline = fix.fs.cache_stats();
    print_stats("baseline (after test prefix lookup)", &baseline);

    // execve("/usr/bin/python3") — three serial lookups, no readdir
    eprintln!("\n  --- execve /usr/bin/python3 ---");
    let usr = fix.lookup(dir.inode, "usr").unwrap();
    let s1 = fix.fs.cache_stats();
    print_stats("after lookup(usr)", &s1);

    let bin = fix.lookup(usr.inode, "bin").unwrap();
    let s2 = fix.fs.cache_stats();
    print_stats("after lookup(bin)", &s2);

    let python3 = fix.lookup(bin.inode, "python3").unwrap();
    let s3 = fix.fs.cache_stats();
    print_stats("after lookup(python3)", &s3);

    assert!(python3.attr.st_mode & libc::S_IFREG as u32 != 0);

    // Now simulate a second execve for /usr/bin/bash — should be mostly cached
    eprintln!("\n  --- execve /usr/bin/bash ---");
    let usr2 = fix.lookup(dir.inode, "usr").unwrap();
    let bin2 = fix.lookup(usr2.inode, "bin").unwrap();
    let bash = fix.lookup(bin2.inode, "bash").unwrap();
    let s4 = fix.fs.cache_stats();
    print_stats("after second execve (bash)", &s4);

    assert!(bash.attr.st_mode & libc::S_IFREG as u32 != 0);

    // And /usr/lib/libz.so (dynamic linker resolving a library)
    eprintln!("\n  --- ld.so resolve /usr/lib/libz.so ---");
    let lib = fix.lookup(usr.inode, "lib").unwrap();
    let libz = fix.lookup(lib.inode, "libz.so").unwrap();
    let s5 = fix.fs.cache_stats();
    print_stats("after ld.so resolve (libz.so)", &s5);

    // Also test symlink: /usr/bin/python -> python3
    eprintln!("\n  --- symlink /usr/bin/python ---");
    let python_link = fix.lookup(bin.inode, "python").unwrap();
    assert!(python_link.attr.st_mode & libc::S_IFLNK as u32 != 0);
    let target = fix.fs.readlink(&root_ctx(), python_link.inode).unwrap();
    assert_eq!(target, b"python3");
    let s6 = fix.fs.cache_stats();
    print_stats("after symlink resolve", &s6);

    // Final report
    let total = s6.hits + s6.misses;
    let hit_rate = if total > 0 { s6.hits as f64 / total as f64 * 100.0 } else { 0.0 };
    eprintln!("\n  SUMMARY: {}/{} cache hits ({:.1}%), {} negative hits",
        s6.hits, total, hit_rate, s6.negative_hits);
    eprintln!("  S3 LIST calls (approx): {} (one per miss)", s6.misses);
}
