mod harness;

use fuse_backend_rs::api::filesystem::FileSystem;
use harness::context::root_ctx;
use harness::fixture::TestFixture;
use harness::s3::require_s3;
use harness::zerocopy::TestWriter;

const ROOT: u64 = 1;

#[test]
fn lookup_existing_file() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("hello.txt", libc::S_IFREG as u32 | 0o644, 1000, 1000, b"hello world");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "hello.txt").unwrap();

    assert_ne!(entry.inode, 0);
    assert_eq!(entry.attr.st_size, 11);
    assert_eq!(entry.attr.st_uid, 1000);
    assert_eq!(entry.attr.st_gid, 1000);
    assert!(entry.attr.st_mode & libc::S_IFREG as u32 != 0);
}

#[test]
fn lookup_existing_directory() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_dir("subdir");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "subdir").unwrap();

    assert_ne!(entry.inode, 0);
    assert!(entry.attr.st_mode & libc::S_IFDIR as u32 != 0);
}

#[test]
fn lookup_nonexistent_returns_enoent() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let result = fix.lookup(dir.inode, "nonexistent");

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
}

#[test]
fn getattr_root() {
    require_s3!();
    let fix = TestFixture::new();

    let (st, ttl) = fix.fs.getattr(&root_ctx(), ROOT, None).unwrap();
    assert_eq!(st.st_ino, ROOT);
    assert!(st.st_mode & libc::S_IFDIR as u32 != 0);
    assert!(ttl.as_secs() > 0);
}

#[test]
fn getattr_after_lookup() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("data.bin", libc::S_IFREG as u32 | 0o755, 0, 0, b"0123456789");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "data.bin").unwrap();

    let (st, _) = fix.fs.getattr(&root_ctx(), entry.inode, None).unwrap();
    assert_eq!(st.st_ino, entry.inode);
    assert_eq!(st.st_size, 10);
    assert_eq!(entry.attr.st_size, st.st_size);
}

#[test]
fn read_file_contents() {
    require_s3!();
    let fix = TestFixture::new();
    let data = b"the quick brown fox";
    fix.seed_root();
    fix.seed_file("fox.txt", libc::S_IFREG as u32 | 0o644, 0, 0, data);

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "fox.txt").unwrap();

    let (handle, _, _) = fix.fs.open(&root_ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
    let mut w = TestWriter::new();
    let n = fix.fs.read(&root_ctx(), entry.inode, handle.unwrap(), &mut w, 4096, 0, None, 0).unwrap();

    assert_eq!(n, data.len());
    assert_eq!(&w.buf[..n], data);
}

#[test]
fn read_with_offset() {
    require_s3!();
    let fix = TestFixture::new();
    let data = b"abcdefghijklmnop";
    fix.seed_root();
    fix.seed_file("offset.bin", libc::S_IFREG as u32 | 0o644, 0, 0, data);

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "offset.bin").unwrap();

    let (handle, _, _) = fix.fs.open(&root_ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
    let mut w = TestWriter::new();
    let n = fix.fs.read(&root_ctx(), entry.inode, handle.unwrap(), &mut w, 5, 4, None, 0).unwrap();

    assert_eq!(n, 5);
    assert_eq!(&w.buf[..n], b"efghi");
}

#[test]
fn readlink_symlink() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_symlink("mylink", "/usr/bin/bash");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "mylink").unwrap();

    assert!(entry.attr.st_mode & libc::S_IFLNK as u32 != 0);

    let target = fix.fs.readlink(&root_ctx(), entry.inode).unwrap();
    assert_eq!(target, b"/usr/bin/bash");
}
