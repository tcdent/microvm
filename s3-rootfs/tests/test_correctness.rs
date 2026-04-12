// Correctness tests for two filed bugs surfaced during the 305d729
// refactor review:
//
// - Bug 1: hardlink write-through breaks on append. After `ln f1 f2`,
//   writing more bytes through f1's handle should update the shared
//   data and leave BOTH paths' bodies as hardlink refs. Today, f1's
//   body gets overwritten with the direct file content, so a fresh
//   session reading f2 sees stale data.
//
// - Rename non-atomicity: documented at s3fs.rs:rename() — no fault
//   injection test yet, but this file pins the happy-path behavior so
//   a future atomic rewrite doesn't regress it.
//
// Tests that reproduce filed bugs are marked #[ignore] with a comment
// pointing at the fix commit. Remove the attribute in the same commit
// that lands the fix.

mod harness;

use fuse_backend_rs::abi::fuse_abi::CreateIn;
use fuse_backend_rs::api::filesystem::FileSystem;
use harness::context::{root_ctx, cname};
use harness::fixture::TestFixture;
use harness::s3::require_s3;
use harness::zerocopy::{TestReader, TestWriter};

const ROOT: u64 = 1;

/// Read the full content of a file through a FUSE open/read/release
/// dance. Helper for tests that care about what the filesystem hands
/// back, not about the specific handle plumbing.
fn read_via_fuse(fix: &TestFixture, parent_ino: u64, name: &str) -> Vec<u8> {
    let entry = fix.fs.lookup(&root_ctx(), parent_ino, &cname(name))
        .expect("lookup failed");
    let (handle, _, _) = fix.fs.open(&root_ctx(), entry.inode, libc::O_RDONLY as u32, 0)
        .expect("open failed");
    let handle = handle.expect("open returned no handle");
    let mut w = TestWriter::new();
    let n = fix.fs.read(&root_ctx(), entry.inode, handle, &mut w, 1 << 20, 0, None, 0)
        .expect("read failed");
    fix.fs.release(&root_ctx(), entry.inode, 0, handle, false, false, None).ok();
    w.buf[..n].to_vec()
}

/// Bug 1: hardlink write-through.
///
/// Scenario: create f1 with content, hardlink f1 -> f2, then — crucially
/// in a COLD session — append to f1. The cold session matters: when the
/// cache already knows f1 is a hardlink, open() picks the right key and
/// release() writes through to `.hardlinks/<uuid>`, and the bug is
/// masked. The real failure is the remount case: cold open of a
/// hardlinked file preloads from the visible key (ref bytes), and
/// release writes those bytes (or whatever the handle buffer became)
/// back to the visible key, detaching it from the shared data.
///
/// After the fix, both names must read back the appended content from
/// a fresh reader.
#[test]
#[ignore = "bug: cold open of a hardlinked file corrupts the ref on release — fix pending"]
fn hardlink_write_through_append() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    let dir_ino = fix.lookup(ROOT, &fix.prefix).unwrap().inode;

    // Step 1: create f1 with "hello"
    let args = CreateIn {
        flags: libc::O_WRONLY as u32,
        mode: 0o644,
        umask: 0,
        fuse_flags: 0,
    };
    let (f1, h1, _, _) = fix.fs.create(&root_ctx(), dir_ino, &cname("f1"), args).unwrap();
    let h1 = h1.unwrap();
    let mut r = TestReader::new(b"hello".to_vec());
    fix.fs.write(&root_ctx(), f1.inode, h1, &mut r, 5, 0, None, false, 0, 0).unwrap();
    fix.fs.release(&root_ctx(), f1.inode, 0, h1, true, false, None).unwrap();

    // Step 2: hardlink f1 -> f2 (on the owning fixture so `fix` is the
    // one who wrote the hardlink ref and knows the layout).
    let _f2 = fix.fs.link(&root_ctx(), f1.inode, dir_ino, &cname("f2")).unwrap();

    // Step 3: cold writer — fresh S3Fs against the same prefix, empty
    // inode cache, no hl_uuid hint. Mirrors unmount-then-`echo WORLD
    // >> f1` in a freshly-booted VM.
    {
        let writer = TestFixture::attach_to(&fix);
        let wdir = writer.lookup(ROOT, &writer.prefix).unwrap().inode;
        let wf1 = writer.lookup(wdir, "f1").unwrap();
        let (wh, _, _) = writer.fs.open(&root_ctx(), wf1.inode, libc::O_RDWR as u32, 0).unwrap();
        let wh = wh.unwrap();
        let mut r = TestReader::new(b"WORLD".to_vec());
        writer.fs.write(&root_ctx(), wf1.inode, wh, &mut r, 5, 5, None, false, 0, 0).unwrap();
        writer.fs.release(&root_ctx(), wf1.inode, 0, wh, true, false, None).unwrap();
        // writer drops here — attach_to means no prefix cleanup
    }

    // Step 4: cold reader — yet another fresh session to read back the
    // final on-disk state without any cache leakage.
    let cold = TestFixture::attach_to(&fix);
    let cold_dir = cold.lookup(ROOT, &cold.prefix).unwrap().inode;

    let f1_bytes = read_via_fuse(&cold, cold_dir, "f1");
    let f2_bytes = read_via_fuse(&cold, cold_dir, "f2");

    assert_eq!(f1_bytes, b"helloWORLD",
        "f1 content after cold append — got {:?}", String::from_utf8_lossy(&f1_bytes));
    assert_eq!(f2_bytes, b"helloWORLD",
        "f2 (hardlink sibling) should see the same updated content — got {:?}",
        String::from_utf8_lossy(&f2_bytes));
}

/// Regression anchor for rename() directory happy path. The non-atomic
/// failure paths (documented inline at s3fs.rs:rename()) are not
/// exercised here — no fault injection yet. This test just pins the
/// behavior callers rely on so the eventual atomic rewrite doesn't
/// silently regress directory rename semantics.
#[test]
fn rename_dir_happy_path() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    // Seed olddir/child.txt directly so the test starts from a known
    // on-disk state rather than depending on mkdir/create working.
    fix.seed_dir("olddir");
    fix.seed_file("olddir/child.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"payload");

    let dir_ino = fix.lookup(ROOT, &fix.prefix).unwrap().inode;

    // Warm the cache with the old layout
    let old = fix.lookup(dir_ino, "olddir").unwrap();
    assert!(old.attr.st_mode & libc::S_IFDIR as u32 != 0);
    let child_pre = fix.lookup(old.inode, "child.txt").unwrap();
    assert_eq!(child_pre.attr.st_size, 7);

    // Rename the directory
    fix.fs.rename(&root_ctx(), dir_ino, &cname("olddir"), dir_ino, &cname("newdir"), 0)
        .expect("rename failed");

    // New path is reachable and the child is still there with its data
    let new = fix.lookup(dir_ino, "newdir").expect("newdir lookup failed after rename");
    assert!(new.attr.st_mode & libc::S_IFDIR as u32 != 0);
    let child_post = read_via_fuse(&fix, new.inode, "child.txt");
    assert_eq!(child_post, b"payload");

    // Cold reader: verify the rename actually persisted to S3 and
    // wasn't papered over by cache state.
    let cold = TestFixture::attach_to(&fix);
    let cold_dir = cold.lookup(ROOT, &cold.prefix).unwrap().inode;
    let cold_new = cold.lookup(cold_dir, "newdir")
        .expect("newdir should exist in cold session");
    let cold_child = read_via_fuse(&cold, cold_new.inode, "child.txt");
    assert_eq!(cold_child, b"payload");

    // Old path is gone
    assert!(cold.lookup(cold_dir, "olddir").is_err(),
        "olddir should not be visible after rename");
}
