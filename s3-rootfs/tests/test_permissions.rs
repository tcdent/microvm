mod harness;

use fuse_backend_rs::api::filesystem::SetattrValid;
use fuse_backend_rs::api::filesystem::FileSystem;
use harness::context::root_ctx;
use harness::fixture::TestFixture;
use harness::s3::require_s3;

const ROOT: u64 = 1;

#[test]
fn file_mode_from_seed() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("script.sh", libc::S_IFREG as u32 | 0o755, 0, 0, b"#!/bin/sh");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "script.sh").unwrap();

    // Full mode should include file type + permission bits
    assert_eq!(entry.attr.st_mode, libc::S_IFREG as u32 | 0o755);
}

#[test]
fn uid_gid_from_seed() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("owned.txt", libc::S_IFREG as u32 | 0o644, 1000, 2000, b"");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "owned.txt").unwrap();

    assert_eq!(entry.attr.st_uid, 1000);
    assert_eq!(entry.attr.st_gid, 2000);
}

#[test]
fn directory_vs_file_vs_symlink_types() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("reg.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    fix.seed_dir("dir");
    fix.seed_symlink("link", "/target");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    let reg = fix.lookup(dir.inode, "reg.txt").unwrap();
    let d = fix.lookup(dir.inode, "dir").unwrap();
    let lnk = fix.lookup(dir.inode, "link").unwrap();

    assert_eq!(reg.attr.st_mode & libc::S_IFMT as u32, libc::S_IFREG as u32);
    assert_eq!(d.attr.st_mode & libc::S_IFMT as u32, libc::S_IFDIR as u32);
    assert_eq!(lnk.attr.st_mode & libc::S_IFMT as u32, libc::S_IFLNK as u32);
}

#[test]
fn setattr_returns_current_attrs_without_error() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("target.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"data");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "target.txt").unwrap();

    let attr: libc::stat64 = unsafe { std::mem::zeroed() };
    let valid = SetattrValid::empty();
    let (st, ttl) = fix.fs.setattr(&root_ctx(), entry.inode, attr, None, valid).unwrap();

    assert_eq!(st.st_ino, entry.inode);
    assert_eq!(st.st_size, 4);
    assert!(ttl.as_secs() > 0);
}

#[test]
fn setattr_mode_change_persists_to_s3() {
    // setattr currently does NOT persist mode changes to S3.
    // This test documents the expected behavior — it should fail
    // until setattr persistence is implemented.
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("chmod.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "chmod.txt").unwrap();

    // Attempt to chmod to 0o755
    let mut attr: libc::stat64 = unsafe { std::mem::zeroed() };
    attr.st_mode = libc::S_IFREG as u32 | 0o755;
    let valid = SetattrValid::MODE;
    let (st, _) = fix.fs.setattr(&root_ctx(), entry.inode, attr, None, valid).unwrap();

    // The returned attrs should reflect the new mode
    assert_eq!(st.st_mode & 0o7777, 0o755,
        "setattr should update mode in response — got {:o}", st.st_mode & 0o7777);
}
