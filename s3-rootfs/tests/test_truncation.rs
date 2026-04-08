/// Regression test for the dir-exhaustion heuristic in probe_and_cache().
///
/// Bug: When LIST with delimiter=/ returns CommonPrefixes, they count toward
/// S3's max-keys limit but NOT toward result.objects.len(). A directory with
/// 10 files + 2 subdirectories = 12 items (truncated at max-keys=12), but
/// objects.len() == 10. The old heuristic compared objects.len() < max_keys
/// (10 < 12 → "dir exhausted") and falsely negative-cached entries that
/// sorted after the first 12 results.
///
/// Fix: Use result.is_truncated from S3's IsTruncated XML tag instead.

mod harness;

use fuse_backend_rs::api::filesystem::FileSystem;
use harness::context::root_ctx;
use harness::fixture::TestFixture;
use harness::s3::require_s3;

const ROOT: u64 = 1;

#[test]
fn truncated_dir_with_common_prefixes_does_not_false_negative() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    // Create >12 root-level entries: mix of files and directories.
    // With delimiter=/, directories produce CommonPrefixes which count
    // toward max-keys but not toward objects.len().
    //
    // 8 files (aa through ah) + 5 directories (dir_a through dir_e) = 13 entries.
    // Each directory needs at least one child so S3 returns it as a CommonPrefix.
    for name in &["aa", "ab", "ac", "ad", "ae", "af", "ag", "ah"] {
        fix.seed_file(name, libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    }
    for name in &["dir_a", "dir_b", "dir_c", "dir_d", "dir_e"] {
        fix.seed_dir(name);
        // Seed a child so the dir shows up as a CommonPrefix with delimiter=/
        fix.seed_file(
            &format!("{}/child.txt", name),
            libc::S_IFREG as u32 | 0o644, 0, 0, b"",
        );
    }

    // Also seed a file that sorts AFTER all the above — "zz_late".
    // This is the entry that would be falsely negative-cached by the old bug.
    fix.seed_file("zz_late", libc::S_IFREG as u32 | 0o644, 0, 0, b"found me");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    // Phase 1 of probe_and_cache will LIST the test prefix dir with max-keys=12.
    // With 8 files + 5 CommonPrefixes = 13 items, S3 truncates at 12.
    // "zz_late" sorts last and won't be in the first 12 results.
    //
    // Old bug: objects.len() (<=8) < max_keys (12) → "dir exhausted" → ENOENT
    // Fix: is_truncated=true → fall through to precise probe → found

    let result = fix.lookup(dir.inode, "zz_late");
    assert!(result.is_ok(),
        "zz_late should be found via precise probe fallback, got: {:?}",
        result.err());

    let entry = result.unwrap();
    assert!(entry.attr.st_mode & libc::S_IFREG as u32 != 0);

    // Verify it was NOT negative-cached
    let stats = fix.fs.cache_stats();
    assert_eq!(stats.negative_count, 0,
        "zz_late should not be negative-cached, but negative_count={}",
        stats.negative_count);
}

#[test]
fn small_dir_exhausted_correctly_returns_enoent() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    // Seed a directory with fewer than 12 entries — no truncation.
    // A lookup for a nonexistent entry should correctly return ENOENT
    // via the "dir exhausted" fast path (is_truncated=false).
    for name in &["alpha", "beta", "gamma"] {
        fix.seed_file(name, libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    }

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    let result = fix.lookup(dir.inode, "nonexistent");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOENT));

    // Should be negative-cached (dir was fully enumerated, entry absent)
    let stats = fix.fs.cache_stats();
    let path = fix.path("nonexistent");
    // We can't directly check the negative cache for a specific path via
    // cache_stats, but negative_count should have increased
    assert!(stats.negative_count > 0,
        "expected negative cache entry for nonexistent path");
}

#[test]
fn truncated_dir_prefills_siblings() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    // Seed 15 files to ensure truncation at max-keys=12
    for i in 0..15 {
        fix.seed_file(
            &format!("f_{:02}", i),
            libc::S_IFREG as u32 | 0o644, 0, 0, b"",
        );
    }

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let stats_before = fix.fs.cache_stats();

    // Lookup f_00 — phase 1 should pre-fill ~12 siblings
    fix.lookup(dir.inode, "f_00").unwrap();

    let stats_after = fix.fs.cache_stats();
    let prefilled = stats_after.inode_count - stats_before.inode_count;
    assert!(prefilled > 1,
        "expected pre-fill of siblings, got only {} new entries", prefilled);

    // Lookup f_14 — sorts late, should fall back to precise probe
    let result = fix.lookup(dir.inode, "f_14");
    assert!(result.is_ok(),
        "f_14 should be found via precise probe, got: {:?}", result.err());
}
