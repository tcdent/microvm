mod harness;

use fuse_backend_rs::abi::fuse_abi::CreateIn;
use fuse_backend_rs::api::filesystem::FileSystem;
use harness::context::{root_ctx, cname};
use harness::fixture::TestFixture;
use harness::s3::require_s3;
use harness::zerocopy::{TestReader, TestWriter};

const ROOT: u64 = 1;

#[test]
fn mkdir_creates_directory() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.fs.mkdir(&root_ctx(), dir.inode, &cname("newdir"), 0o755, 0).unwrap();

    assert!(entry.attr.st_mode & libc::S_IFDIR as u32 != 0);

    // Verify it's findable via lookup
    let found = fix.lookup(dir.inode, "newdir").unwrap();
    assert_eq!(found.inode, entry.inode);
}

#[test]
fn create_file() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let args = CreateIn {
        flags: libc::O_WRONLY as u32,
        mode: 0o644,
        umask: 0,
        fuse_flags: 0,
    };
    let (entry, handle, _, _) = fix.fs.create(&root_ctx(), dir.inode, &cname("new.txt"), args).unwrap();

    assert!(entry.attr.st_mode & libc::S_IFREG as u32 != 0);
    assert!(handle.is_some());

    // Release the handle (empty file)
    fix.fs.release(&root_ctx(), entry.inode, 0, handle.unwrap(), true, false, None).unwrap();

    // Verify via lookup
    let found = fix.lookup(dir.inode, "new.txt").unwrap();
    assert_eq!(found.inode, entry.inode);
}

#[test]
fn create_and_write() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let args = CreateIn {
        flags: libc::O_WRONLY as u32,
        mode: 0o644,
        umask: 0,
        fuse_flags: 0,
    };
    let (entry, handle, _, _) = fix.fs.create(&root_ctx(), dir.inode, &cname("written.txt"), args).unwrap();
    let handle = handle.unwrap();

    let data = b"test data 12345";
    let mut reader = TestReader::new(data.to_vec());
    let n = fix.fs.write(&root_ctx(), entry.inode, handle, &mut reader,
                         data.len() as u32, 0, None, false, 0, 0).unwrap();
    assert_eq!(n, data.len());

    // Release flushes to S3
    fix.fs.release(&root_ctx(), entry.inode, 0, handle, true, false, None).unwrap();

    // Verify via direct S3 GET — reconstruct the key
    let path = fix.path("written.txt");
    let meta = s3_rootfs::KeyMeta::new(libc::S_IFREG as u32 | 0o755, 0, 0);
    let key = s3_rootfs::encode_key(&path, meta);
    let stored = fix.client.get(&key).unwrap();
    assert_eq!(stored, data);
}

#[test]
fn write_at_offset() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let args = CreateIn {
        flags: libc::O_WRONLY as u32,
        mode: 0o644,
        umask: 0,
        fuse_flags: 0,
    };
    let (entry, handle, _, _) = fix.fs.create(&root_ctx(), dir.inode, &cname("offset.bin"), args).unwrap();
    let handle = handle.unwrap();

    // Write "hello" at offset 0
    let mut r1 = TestReader::new(b"hello".to_vec());
    fix.fs.write(&root_ctx(), entry.inode, handle, &mut r1, 5, 0, None, false, 0, 0).unwrap();

    // Write "world" at offset 10 (gap should be zero-filled)
    let mut r2 = TestReader::new(b"world".to_vec());
    fix.fs.write(&root_ctx(), entry.inode, handle, &mut r2, 5, 10, None, false, 0, 0).unwrap();

    fix.fs.release(&root_ctx(), entry.inode, 0, handle, true, false, None).unwrap();

    // Verify via S3
    let path = fix.path("offset.bin");
    let meta = s3_rootfs::KeyMeta::new(libc::S_IFREG as u32 | 0o755, 0, 0);
    let key = s3_rootfs::encode_key(&path, meta);
    let stored = fix.client.get(&key).unwrap();
    assert_eq!(stored.len(), 15);
    assert_eq!(&stored[0..5], b"hello");
    assert_eq!(&stored[5..10], &[0, 0, 0, 0, 0]);
    assert_eq!(&stored[10..15], b"world");
}

#[test]
fn write_then_read_back() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let data = b"roundtrip test data";

    // Create and write
    let args = CreateIn {
        flags: libc::O_WRONLY as u32,
        mode: 0o644,
        umask: 0,
        fuse_flags: 0,
    };
    let (entry, handle, _, _) = fix.fs.create(&root_ctx(), dir.inode, &cname("roundtrip.txt"), args).unwrap();
    let wh = handle.unwrap();
    let mut reader = TestReader::new(data.to_vec());
    fix.fs.write(&root_ctx(), entry.inode, wh, &mut reader,
                 data.len() as u32, 0, None, false, 0, 0).unwrap();
    fix.fs.release(&root_ctx(), entry.inode, 0, wh, true, false, None).unwrap();

    // Open for read and read back
    let (rh, _, _) = fix.fs.open(&root_ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
    let mut w = TestWriter::new();
    let n = fix.fs.read(&root_ctx(), entry.inode, rh.unwrap(), &mut w, 4096, 0, None, 0).unwrap();

    assert_eq!(n, data.len());
    assert_eq!(&w.buf[..n], data);
}

#[test]
fn unlink_removes_file() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("doomed.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"goodbye");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    // Verify it exists
    let entry = fix.lookup(dir.inode, "doomed.txt").unwrap();
    assert_ne!(entry.inode, 0);

    // Unlink
    fix.fs.unlink(&root_ctx(), dir.inode, &cname("doomed.txt")).unwrap();

    // Should be gone from cache
    let result = fix.lookup(dir.inode, "doomed.txt");
    assert!(result.is_err());
}

#[test]
fn release_clean_handle_no_s3_write() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("readonly.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"original");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "readonly.txt").unwrap();

    // Open read-only
    let (handle, _, _) = fix.fs.open(&root_ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();

    // Release without writing — should not flush
    fix.fs.release(&root_ctx(), entry.inode, 0, handle.unwrap(), false, false, None).unwrap();

    // Original data should be unchanged
    let path = fix.path("readonly.txt");
    let meta = s3_rootfs::KeyMeta::new(libc::S_IFREG as u32 | 0o644, 0, 0);
    let key = s3_rootfs::encode_key(&path, meta);
    let stored = fix.client.get(&key).unwrap();
    assert_eq!(stored, b"original");
}
