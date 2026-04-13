//! Integration tests for the golden filesystem layer (Phase G1).
//!
//! Each test seeds a manifest + blobs via `TestFixture::with_golden`
//! (the manifest is a local tempfile; blobs live in the shared S3
//! test bucket at per-test-unique keys) and then drives FUSE ops
//! through the resulting `S3Fs`.

mod harness;

use fuse_backend_rs::abi::fuse_abi::CreateIn;
use fuse_backend_rs::api::filesystem::FileSystem;

use harness::context::{cname, root_ctx};
use harness::fixture::{GoldenSeed, TestFixture};
use harness::s3::require_s3;
use harness::zerocopy::{TestReader, TestWriter};

use s3_rootfs::blob_cache::is_golden_ino;

const ROOT: u64 = 1;

/// Walk a slash-separated path from root, returning the final ino.
fn walk(fix: &TestFixture, path: &str) -> u64 {
    let mut ino = ROOT;
    for component in path.split('/') {
        if component.is_empty() {
            continue;
        }
        ino = fix.fs.lookup(&root_ctx(), ino, &cname(component)).unwrap().inode;
    }
    ino
}

/// Full-file read via FUSE open/read/release dance.
fn read_all(fix: &TestFixture, ino: u64) -> Vec<u8> {
    let (handle, _, _) = fix
        .fs
        .open(&root_ctx(), ino, libc::O_RDONLY as u32, 0)
        .expect("open failed");
    let handle = handle.expect("open returned no handle");
    let mut w = TestWriter::new();
    let n = fix
        .fs
        .read(&root_ctx(), ino, handle, &mut w, 4 * 1024 * 1024, 0, None, 0)
        .expect("read failed");
    fix.fs.release(&root_ctx(), ino, 0, handle, false, false, None).ok();
    w.buf[..n].to_vec()
}

/// Lookup against a golden-seeded path returns an Entry whose inode
/// has bit 63 set — the "this came from the golden tier" marker.
#[test]
fn lookup_tier2_hits_golden() {
    require_s3!();
    let (fix, _tmp) = TestFixture::with_golden(vec![
        GoldenSeed::dir("etc"),
        GoldenSeed::file("etc/hosts", libc::S_IFREG as u32 | 0o644, b"127.0.0.1\tlocalhost\n"),
    ]);
    let etc = fix.fs.lookup(&root_ctx(), ROOT, &cname("etc")).unwrap();
    assert!(is_golden_ino(etc.inode), "etc should come from golden tier");

    let hosts = fix.fs.lookup(&root_ctx(), etc.inode, &cname("hosts")).unwrap();
    assert!(is_golden_ino(hosts.inode));
    assert_eq!(hosts.attr.st_size, 20);
}

/// getattr on a golden inode returns golden metadata.
#[test]
fn getattr_on_golden_ino() {
    require_s3!();
    let (fix, _tmp) = TestFixture::with_golden(vec![GoldenSeed::file(
        "README",
        libc::S_IFREG as u32 | 0o644,
        b"golden readme",
    )]);
    let readme = fix.fs.lookup(&root_ctx(), ROOT, &cname("README")).unwrap();
    let (st, _ttl) = fix.fs.getattr(&root_ctx(), readme.inode, None).unwrap();
    assert_eq!(st.st_size, 13);
    assert_eq!(st.st_mode & 0o7777, 0o644);
    assert!(is_golden_ino(st.st_ino));
}

/// Reading a golden file hits the RAM blob cache — no S3 round-trip
/// for blob content on subsequent reads. We don't have easy access
/// to request counters here, so the assertion is correctness-only:
/// the bytes come back intact and lookup set a golden ino.
#[test]
fn read_golden_serves_from_ram() {
    require_s3!();
    let payload = b"contents of a golden-inherited file".to_vec();
    let (fix, _tmp) = TestFixture::with_golden(vec![GoldenSeed::file(
        "file.txt",
        libc::S_IFREG as u32 | 0o644,
        &payload,
    )]);
    let ino = walk(&fix, "file.txt");
    assert!(is_golden_ino(ino));
    let got = read_all(&fix, ino);
    assert_eq!(got, payload);
}

/// readlink on a golden symlink returns the inline target from the
/// manifest — no S3 GET.
#[test]
fn readlink_golden_serves_inline_target() {
    require_s3!();
    let (fix, _tmp) = TestFixture::with_golden(vec![GoldenSeed::symlink("bin", "usr/bin")]);
    let link = fix.fs.lookup(&root_ctx(), ROOT, &cname("bin")).unwrap();
    assert!(is_golden_ino(link.inode));
    let target = fix.fs.readlink(&root_ctx(), link.inode).unwrap();
    assert_eq!(target, b"usr/bin");
}

/// Opening a golden-backed file O_RDWR materializes its content
/// into the handle buffer; release flushes the buffer to vm-<id>;
/// subsequent reads (cold session) see the new bytes at the visible
/// path.
#[test]
fn open_write_golden_materializes() {
    require_s3!();
    // Unique path per test — the shared S3 bucket's per-VM
    // namespace isn't prefix-scoped (tests write to bucket/<path>
    // directly during release), so parallel tests would collide
    // on `bucket/config\x01meta`. Give this test its own path.
    let (fix, _tmp) = TestFixture::with_golden(vec![GoldenSeed::file(
        "mat_config",
        libc::S_IFREG as u32 | 0o644,
        b"original",
    )]);
    let ino = walk(&fix, "mat_config");
    assert!(is_golden_ino(ino));

    // Open RDWR, write a new byte at offset 0. Release flushes.
    let (handle, _, _) = fix.fs.open(&root_ctx(), ino, libc::O_RDWR as u32, 0).unwrap();
    let handle = handle.unwrap();
    let mut r = TestReader::new(b"X".to_vec());
    fix.fs.write(&root_ctx(), ino, handle, &mut r, 1, 0, None, false, 0, 0).unwrap();
    fix.fs.release(&root_ctx(), ino, 0, handle, true, false, None).unwrap();

    // Read through the same fixture (per-VM cache is now populated
    // with the materialized path; resolve_inode routes the zombie
    // golden ino to the per-VM entry).
    let got = read_all(&fix, ino);
    assert_eq!(got, b"Xriginal", "materialized write overlay on byte 0");
}

/// After materialize-on-write, getattr and read on the ORIGINAL
/// golden ino still work (kernel may hold the old ino in its dentry
/// cache) but serve the fresh post-write content. This is the
/// load-bearing correctness test for vm:2.8's pushback 2.
#[test]
fn post_materialize_golden_ino_serves_fresh_content() {
    require_s3!();
    // Unique path — see `open_write_golden_materializes` for why.
    let (fix, _tmp) = TestFixture::with_golden(vec![GoldenSeed::file(
        "zombie_config",
        libc::S_IFREG as u32 | 0o644,
        b"OLDCONTENT",
    )]);
    let golden_ino = walk(&fix, "zombie_config");
    assert!(is_golden_ino(golden_ino));

    // Materialize via write + release.
    let (handle, _, _) = fix
        .fs
        .open(&root_ctx(), golden_ino, libc::O_RDWR as u32, 0)
        .unwrap();
    let handle = handle.unwrap();
    let mut r = TestReader::new(b"NEW".to_vec());
    fix.fs.write(&root_ctx(), golden_ino, handle, &mut r, 3, 0, None, false, 0, 0).unwrap();
    fix.fs.release(&root_ctx(), golden_ino, 0, handle, true, false, None).unwrap();

    // Now the same (golden) ino is a "zombie" — kernel still has
    // it, but the path is shadowed in per-VM. resolve_inode must
    // route to the per-VM entry, not to stale golden bytes.
    let (st, _ttl) = fix.fs.getattr(&root_ctx(), golden_ino, None).unwrap();
    assert_eq!(st.st_size, 10, "post-materialize getattr sees new size (NEWCONTENT = 10 bytes)");

    let got = read_all(&fix, golden_ino);
    assert_eq!(got, b"NEWCONTENT", "post-materialize read returns fresh content, not stale golden");
}

/// Unlinking a golden-only path issues no S3 DELETE but creates a
/// sticky negative cache entry — subsequent lookups return ENOENT.
#[test]
fn unlink_golden_inserts_negative() {
    require_s3!();
    let (fix, _tmp) = TestFixture::with_golden(vec![GoldenSeed::file(
        "dead.txt",
        libc::S_IFREG as u32 | 0o644,
        b"about to die",
    )]);
    // Pre-unlink: lookup works.
    assert!(fix.fs.lookup(&root_ctx(), ROOT, &cname("dead.txt")).is_ok());

    fix.fs
        .unlink(&root_ctx(), ROOT, &cname("dead.txt"))
        .expect("unlink failed");

    // Post-unlink: lookup returns ENOENT.
    let err = fix
        .fs
        .lookup(&root_ctx(), ROOT, &cname("dead.txt"))
        .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOENT));

    // Cold reader starts fresh — it doesn't see the negative. But
    // this test's assertion is about the live fixture's behavior
    // post-unlink, which is what a running VM would observe.
}

/// readdir on a golden directory enumerates its children from the
/// pre-built by_parent index, unioned with any per-VM children at
/// the same path. Negatives are filtered.
#[test]
fn readdir_unions_golden_and_vm() {
    require_s3!();
    let (fix, _tmp) = TestFixture::with_golden(vec![
        GoldenSeed::dir("shared"),
        GoldenSeed::file("shared/golden_child", libc::S_IFREG as u32 | 0o644, b"g"),
    ]);
    // Also create a per-VM file under the same shared directory.
    // We need the per-VM cache to know about "shared" with the
    // correct parent ino — seed the root marker and create under
    // golden's "shared" dir via a lookup + create.
    let shared_ino = fix.fs.lookup(&root_ctx(), ROOT, &cname("shared")).unwrap().inode;
    let args = CreateIn {
        flags: libc::O_WRONLY as u32,
        mode: 0o644,
        umask: 0,
        fuse_flags: 0,
    };
    let (_, handle, _, _) = fix
        .fs
        .create(&root_ctx(), shared_ino, &cname("vm_child"), args)
        .unwrap();
    let handle = handle.unwrap();
    let mut r = TestReader::new(b"v".to_vec());
    fix.fs
        .write(&root_ctx(), ROOT, handle, &mut r, 1, 0, None, false, 0, 0)
        .ok();
    fix.fs
        .release(&root_ctx(), ROOT, 0, handle, true, false, None)
        .unwrap();

    // readdir on shared_ino should surface both children.
    let mut collected: Vec<String> = Vec::new();
    fix.fs
        .readdir(&root_ctx(), shared_ino, 0, 4096, 0, &mut |e| {
            collected.push(String::from_utf8_lossy(e.name).into_owned());
            Ok(1)
        })
        .unwrap();
    collected.sort();
    assert_eq!(collected, vec!["golden_child".to_string(), "vm_child".to_string()]);
}

/// readdir filters entries that have been negatively cached via
/// unlink on a golden-inherited path.
#[test]
fn readdir_filters_negatives() {
    require_s3!();
    let (fix, _tmp) = TestFixture::with_golden(vec![
        GoldenSeed::dir("mnt"),
        GoldenSeed::file("mnt/keep", libc::S_IFREG as u32 | 0o644, b"k"),
        GoldenSeed::file("mnt/gone", libc::S_IFREG as u32 | 0o644, b"g"),
    ]);
    let mnt_ino = fix.fs.lookup(&root_ctx(), ROOT, &cname("mnt")).unwrap().inode;

    // Unlink one golden-inherited child.
    fix.fs
        .unlink(&root_ctx(), mnt_ino, &cname("gone"))
        .unwrap();

    let mut collected: Vec<String> = Vec::new();
    fix.fs
        .readdir(&root_ctx(), mnt_ino, 0, 4096, 0, &mut |e| {
            collected.push(String::from_utf8_lossy(e.name).into_owned());
            Ok(1)
        })
        .unwrap();
    collected.sort();
    assert_eq!(collected, vec!["keep".to_string()]);
}

/// With `golden = None`, everything behaves exactly as pre-G1 —
/// the sentinel empty cache is transparent to callers.
#[test]
fn golden_disabled_zero_cost() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file(
        "regular.txt",
        libc::S_IFREG as u32 | 0o644,
        0,
        0,
        b"no golden",
    );
    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "regular.txt").unwrap();
    // No golden ino: bit 63 clear.
    assert!(!is_golden_ino(entry.inode));
    let got = read_all(&fix, entry.inode);
    assert_eq!(got, b"no golden");
}
