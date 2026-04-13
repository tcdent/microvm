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

    // Release enqueues into the deferred-write pool — the FUSE
    // handler returns fast. Wait for the worker to drain before
    // checking S3 directly (research/13 §2: deferred writes).
    fix.fs.release(&root_ctx(), entry.inode, 0, handle, true, false, None).unwrap();
    fix.fs.sync_all();

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
    fix.fs.sync_all();

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

#[test]
fn read_your_writes_without_sync() {
    // P0 regression: research/13 §2.1 read-your-writes.
    // After release() enqueues into the deferred-write pool, a
    // subsequent open+read of the same path MUST see the written
    // bytes WITHOUT any sync/fsync. Reproduces the bug that broke
    // `echo 'nameserver 1.1.1.1' > /etc/resolv.conf; cat /etc/resolv.conf`
    // in the guest (returned 0 bytes until sync).
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    // Create + write + release (enqueues, does NOT sync)
    let args = CreateIn {
        flags: libc::O_WRONLY as u32,
        mode: 0o644,
        umask: 0,
        fuse_flags: 0,
    };
    let (entry, handle, _, _) = fix.fs.create(&root_ctx(), dir.inode, &cname("ryw.txt"), args).unwrap();
    let wh = handle.unwrap();
    let data = b"nameserver 1.1.1.1\n";
    let mut reader = TestReader::new(data.to_vec());
    fix.fs.write(&root_ctx(), entry.inode, wh, &mut reader,
                 data.len() as u32, 0, None, false, 0, 0).unwrap();
    fix.fs.release(&root_ctx(), entry.inode, 0, wh, true, false, None).unwrap();

    // DELIBERATELY NO sync_all() — this is the whole point.

    // Fresh open for read on the same path. getattr should see the
    // new size, read should see the new bytes.
    let found = fix.lookup(dir.inode, "ryw.txt").unwrap();
    assert_eq!(found.attr.st_size, data.len() as i64,
        "getattr must reflect inflight write size without sync");

    let (rh, _, _) = fix.fs.open(&root_ctx(), found.inode, libc::O_RDONLY as u32, 0).unwrap();
    let mut w = TestWriter::new();
    let n = fix.fs.read(&root_ctx(), found.inode, rh.unwrap(), &mut w, 4096, 0, None, 0).unwrap();
    assert_eq!(n, data.len(), "read must return full buffer from inflight");
    assert_eq!(&w.buf[..n], data, "read must return the just-written bytes");
}

#[test]
fn read_your_writes_on_golden_file_via_trunc() {
    // Reproduces the `echo > /etc/resolv.conf` bug: a golden-inherited
    // file is opened O_WRONLY|O_TRUNC, written, released. Next open+read
    // (cold kernel dcache still caching the golden ino) must see the
    // written bytes without sync.
    require_s3!();
    use harness::fixture::GoldenSeed;
    let (fix, _tmp) = harness::fixture::TestFixture::with_golden(vec![
        GoldenSeed::dir("etc"),
        GoldenSeed::file("etc/resolv.conf",
            libc::S_IFREG as u32 | 0o644,
            b"original golden contents\n"),
    ]);

    // Initial lookup to warm the kernel dcache → golden ino
    let etc = fix.lookup(ROOT, "etc").unwrap();
    let original = fix.lookup(etc.inode, "resolv.conf").unwrap();
    let golden_ino = original.inode;

    // Open O_WRONLY|O_TRUNC on the GOLDEN ino (mimics bash `>`)
    let flags = (libc::O_WRONLY | libc::O_TRUNC) as u32;
    let (wh_opt, _, _) = fix.fs.open(&root_ctx(), golden_ino, flags, 0).unwrap();
    let wh = wh_opt.unwrap();

    // Note: the guest issued setattr(size=0) implicitly via O_TRUNC
    // in real life. Call setattr explicitly to match.
    let mut attr: libc::stat64 = unsafe { std::mem::zeroed() };
    attr.st_size = 0;
    let valid = fuse_backend_rs::abi::fuse_abi::SetattrValid::SIZE;
    let _ = fix.fs.setattr(&root_ctx(), golden_ino, attr, Some(wh), valid);

    let data = b"nameserver 1.1.1.1\n";
    let mut reader = TestReader::new(data.to_vec());
    fix.fs.write(&root_ctx(), golden_ino, wh, &mut reader,
                 data.len() as u32, 0, None, false, 0, 0).unwrap();
    fix.fs.release(&root_ctx(), golden_ino, 0, wh, true, false, None).unwrap();

    // NO sync. Read back via the SAME (golden) ino — kernel dcache
    // hasn't been invalidated, so it re-uses the golden ino.
    let (rh, _, _) = fix.fs.open(&root_ctx(), golden_ino, libc::O_RDONLY as u32, 0).unwrap();
    let mut w = TestWriter::new();
    let n = fix.fs.read(&root_ctx(), golden_ino, rh.unwrap(), &mut w, 4096, 0, None, 0).unwrap();
    assert_eq!(n, data.len(),
        "read via golden ino after trunc+write should return written bytes");
    assert_eq!(&w.buf[..n], data,
        "got {:?}", String::from_utf8_lossy(&w.buf[..n]));
}

#[test]
fn read_your_writes_golden_with_prior_read_cache() {
    // Prior read populates caches (or not), THEN trunc-write-read.
    // Catches the case where stale read cache intercepts the fresh read.
    require_s3!();
    use harness::fixture::GoldenSeed;
    let (fix, _tmp) = harness::fixture::TestFixture::with_golden(vec![
        GoldenSeed::dir("etc"),
        GoldenSeed::file("etc/resolv.conf",
            libc::S_IFREG as u32 | 0o644,
            b"original golden contents\n"),
    ]);

    let etc = fix.lookup(ROOT, "etc").unwrap();
    let r = fix.lookup(etc.inode, "resolv.conf").unwrap();
    let golden_ino = r.inode;

    // Prior read to warm caches (guest may have `cat`'d it earlier)
    let (rh0, _, _) = fix.fs.open(&root_ctx(), golden_ino, libc::O_RDONLY as u32, 0).unwrap();
    let mut w0 = TestWriter::new();
    fix.fs.read(&root_ctx(), golden_ino, rh0.unwrap(), &mut w0, 4096, 0, None, 0).unwrap();
    fix.fs.release(&root_ctx(), golden_ino, 0, rh0.unwrap(), false, false, None).unwrap();

    // Now: echo > /etc/resolv.conf equivalent
    let flags = (libc::O_WRONLY | libc::O_TRUNC) as u32;
    let (wh_opt, _, _) = fix.fs.open(&root_ctx(), golden_ino, flags, 0).unwrap();
    let wh = wh_opt.unwrap();
    let data = b"nameserver 1.1.1.1\n";
    let mut reader = TestReader::new(data.to_vec());
    fix.fs.write(&root_ctx(), golden_ino, wh, &mut reader,
                 data.len() as u32, 0, None, false, 0, 0).unwrap();
    fix.fs.release(&root_ctx(), golden_ino, 0, wh, true, false, None).unwrap();

    // Read back — no sync
    let (rh, _, _) = fix.fs.open(&root_ctx(), golden_ino, libc::O_RDONLY as u32, 0).unwrap();
    let mut w = TestWriter::new();
    let n = fix.fs.read(&root_ctx(), golden_ino, rh.unwrap(), &mut w, 4096, 0, None, 0).unwrap();
    assert_eq!(&w.buf[..n], data,
        "got {:?}", String::from_utf8_lossy(&w.buf[..n]));
}
