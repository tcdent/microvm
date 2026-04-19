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

    // Fresh lookup + explicit getattr on the same path. The kernel
    // calls getattr (not lookup) for stat(), so both must agree.
    let found = fix.lookup(dir.inode, "ryw.txt").unwrap();
    assert_eq!(found.attr.st_size, data.len() as i64,
        "lookup must reflect inflight write size without sync");

    let (stat, _ttl) = fix.fs.getattr(&root_ctx(), found.inode, None).unwrap();
    assert_eq!(stat.st_size, data.len() as i64,
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

#[test]
fn rename_dir_preserves_deferred_write_content() {
    // Reproduce nvm install pattern: extract many files into staging
    // dir, close them all (deferred writes enqueued), then rename
    // the staging dir to the final location. Content must survive.
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let root_dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    // Create staging dir
    fix.fs.mkdir(&root_ctx(), root_dir.inode, &cname("staging"), 0o755, 0).unwrap();
    let staging = fix.lookup(root_dir.inode, "staging").unwrap();

    // Create 20 files with distinct content, close each
    let file_count = 20;
    for i in 0..file_count {
        let name = format!("file_{:03}.js", i);
        let content = format!("// module {} content here\nexport default {{}}\n", i);
        let args = CreateIn {
            flags: libc::O_WRONLY as u32,
            mode: 0o644,
            umask: 0,
            fuse_flags: 0,
        };
        let (entry, handle, _, _) = fix.fs.create(&root_ctx(), staging.inode, &cname(&name), args).unwrap();
        let wh = handle.unwrap();
        let mut reader = TestReader::new(content.as_bytes().to_vec());
        fix.fs.write(&root_ctx(), entry.inode, wh, &mut reader,
                     content.len() as u32, 0, None, false, 0, 0).unwrap();
        // Flush (synchronous, like kernel does before close returns)
        fix.fs.flush(&root_ctx(), entry.inode, wh, 0).unwrap();
        // Release (async in real kernel, but we call it here)
        fix.fs.release(&root_ctx(), entry.inode, 0, wh, true, false, None).unwrap();
    }

    // NO sync — rename should drain internally
    // Create destination dir
    fix.fs.mkdir(&root_ctx(), root_dir.inode, &cname("final"), 0o755, 0).unwrap();

    // Rename staging -> final (this is what nvm does)
    fix.fs.rename(&root_ctx(), root_dir.inode, &cname("staging"),
                  root_dir.inode, &cname("final"), 0).unwrap();

    // Read back every file from the new location
    let final_dir = fix.lookup(root_dir.inode, "final").unwrap();
    for i in 0..file_count {
        let name = format!("file_{:03}.js", i);
        let expected = format!("// module {} content here\nexport default {{}}\n", i);
        let entry = fix.lookup(final_dir.inode, &name)
            .unwrap_or_else(|_| panic!("file {} missing after rename", name));
        let (rh, _, _) = fix.fs.open(&root_ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
        let mut w = TestWriter::new();
        let n = fix.fs.read(&root_ctx(), entry.inode, rh.unwrap(), &mut w, 4096, 0, None, 0).unwrap();
        assert_eq!(&w.buf[..n], expected.as_bytes(),
            "file_{:03}.js: expected {:?}, got {:?}", i,
            expected, String::from_utf8_lossy(&w.buf[..n]));
    }
}

#[test]
fn rename_dir_with_pending_release() {
    // Simulates async FUSE_RELEASE: flush is sync (kernel waits),
    // release is fire-and-forget (may not arrive before rename).
    // Rename must still preserve content from flush-enqueued data.
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let root_dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    fix.fs.mkdir(&root_ctx(), root_dir.inode, &cname("staging"), 0o755, 0).unwrap();
    let staging = fix.lookup(root_dir.inode, "staging").unwrap();

    // Create files, flush (sync), but DO NOT release (simulating
    // kernel's async release that hasn't arrived yet)
    let mut open_handles: Vec<(u64, u64)> = Vec::new(); // (inode, handle)
    let file_count = 10;
    for i in 0..file_count {
        let name = format!("mod_{}.js", i);
        let content = format!("exports.val = {};\n", i);
        let args = CreateIn {
            flags: libc::O_WRONLY as u32,
            mode: 0o644,
            umask: 0,
            fuse_flags: 0,
        };
        let (entry, handle, _, _) = fix.fs.create(&root_ctx(), staging.inode, &cname(&name), args).unwrap();
        let wh = handle.unwrap();
        let mut reader = TestReader::new(content.as_bytes().to_vec());
        fix.fs.write(&root_ctx(), entry.inode, wh, &mut reader,
                     content.len() as u32, 0, None, false, 0, 0).unwrap();
        fix.fs.flush(&root_ctx(), entry.inode, wh, 0).unwrap();
        open_handles.push((entry.inode, wh));
        // NO release — simulates kernel's async release delay
    }

    // Rename fires BEFORE release arrives
    fix.fs.rename(&root_ctx(), root_dir.inode, &cname("staging"),
                  root_dir.inode, &cname("final"), 0).unwrap();

    // Now releases arrive (async, after rename)
    for (ino, wh) in &open_handles {
        fix.fs.release(&root_ctx(), *ino, 0, *wh, true, false, None).unwrap();
    }
    fix.fs.sync_all();

    // Read back
    let final_dir = fix.lookup(root_dir.inode, "final").unwrap();
    for i in 0..file_count {
        let name = format!("mod_{}.js", i);
        let expected = format!("exports.val = {};\n", i);
        let entry = fix.lookup(final_dir.inode, &name)
            .unwrap_or_else(|_| panic!("{} missing after rename", name));
        let (rh, _, _) = fix.fs.open(&root_ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
        let mut w = TestWriter::new();
        let n = fix.fs.read(&root_ctx(), entry.inode, rh.unwrap(), &mut w, 4096, 0, None, 0).unwrap();
        assert_eq!(&w.buf[..n], expected.as_bytes(),
            "mod_{}.js: expected {:?}, got {:?}", i,
            expected, String::from_utf8_lossy(&w.buf[..n]));
    }
}

#[test]
fn s3_copy_preserves_content_with_metadata_keys() {
    // Verify RustFS CopyObject works correctly with our
    // \x01-separated metadata keys. If this fails, the rename
    // corruption is a backend bug, not an FS-layer bug.
    require_s3!();
    let fix = TestFixture::new();
    let meta = s3_rootfs::KeyMeta::new(libc::S_IFREG as u32 | 0o755, 0, 0);

    // PUT 50 files with distinct content via our key encoding
    let count = 50;
    for i in 0..count {
        let path = fix.path(&format!("copytest/file_{:03}.js", i));
        let key = s3_rootfs::encode_key(&path, meta);
        let content = format!("// content for file {}\nexport default {{}}\n", i);
        fix.client.put(&key, content.as_bytes()).unwrap();
    }

    // COPY each to a new prefix using the same method as rename
    for i in 0..count {
        let old_path = fix.path(&format!("copytest/file_{:03}.js", i));
        let new_path = fix.path(&format!("copied/file_{:03}.js", i));
        let old_key = s3_rootfs::encode_key(&old_path, meta);
        let new_key = s3_rootfs::encode_key(&new_path, meta);
        fix.client.copy(&old_key, &new_key).unwrap();
    }

    // Verify each copied file has correct content
    for i in 0..count {
        let new_path = fix.path(&format!("copied/file_{:03}.js", i));
        let new_key = s3_rootfs::encode_key(&new_path, meta);
        let expected = format!("// content for file {}\nexport default {{}}\n", i);
        let data = fix.client.get(&new_key).unwrap();
        assert_eq!(data, expected.as_bytes(),
            "file_{:03}.js copy mismatch: got {:?}", i,
            String::from_utf8_lossy(&data));
    }
}

#[test]
fn rename_dir_at_scale() {
    // npm's bundled node_modules has ~1000+ files.
    // Test at scale to catch races that don't appear with 20 files.
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();

    let root_dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    fix.fs.mkdir(&root_ctx(), root_dir.inode, &cname("staging"), 0o755, 0).unwrap();
    let staging = fix.lookup(root_dir.inode, "staging").unwrap();

    // Nest dirs 3 levels deep with files at each level
    let mut dirs = vec![staging.inode];
    for d in 0..5 {
        let dname = format!("dir_{}", d);
        let de = fix.fs.mkdir(&root_ctx(), staging.inode, &cname(&dname), 0o755, 0).unwrap();
        dirs.push(de.attr.st_ino);
        for sd in 0..3 {
            let sdname = format!("sub_{}", sd);
            let sde = fix.fs.mkdir(&root_ctx(), de.attr.st_ino, &cname(&sdname), 0o755, 0).unwrap();
            dirs.push(sde.attr.st_ino);
        }
    }

    // Create 500 files spread across dirs
    let file_count = 500;
    let mut expected: Vec<(String, String)> = Vec::new();
    for i in 0..file_count {
        let parent_ino = dirs[i % dirs.len()];
        let name = format!("f_{:04}.json", i);
        let content = format!("{{\"id\":{},\"name\":\"pkg_{}\"}}", i, i);
        let args = CreateIn {
            flags: libc::O_WRONLY as u32,
            mode: 0o644,
            umask: 0,
            fuse_flags: 0,
        };
        let (entry, handle, _, _) = fix.fs.create(&root_ctx(), parent_ino, &cname(&name), args).unwrap();
        let wh = handle.unwrap();
        let mut reader = TestReader::new(content.as_bytes().to_vec());
        fix.fs.write(&root_ctx(), entry.inode, wh, &mut reader,
                     content.len() as u32, 0, None, false, 0, 0).unwrap();
        fix.fs.flush(&root_ctx(), entry.inode, wh, 0).unwrap();
        fix.fs.release(&root_ctx(), entry.inode, 0, wh, true, false, None).unwrap();
        expected.push((name, content));
    }

    // Rename staging -> final
    fix.fs.rename(&root_ctx(), root_dir.inode, &cname("staging"),
                  root_dir.inode, &cname("final"), 0).unwrap();

    // Cold reader — fresh caches, reads from S3 only
    let cold = TestFixture::attach_to(&fix);
    let cold_root = cold.lookup(ROOT, &cold.prefix).unwrap();
    let final_dir = cold.lookup(cold_root.inode, "final").unwrap();

    // Verify a sample — check 50 files spread across dirs
    for i in (0..file_count).step_by(10) {
        let parent_idx = i % dirs.len();
        let parent_name = if parent_idx == 0 {
            "final".to_string()
        } else {
            let d = (parent_idx - 1) / 4;
            let sd = (parent_idx - 1) % 4;
            if sd == 0 {
                format!("final/dir_{}", d)
            } else {
                format!("final/dir_{}/sub_{}", d, sd - 1)
            }
        };
        // Walk to the parent
        let mut parent_ino = cold_root.inode;
        for seg in parent_name.split('/') {
            parent_ino = cold.lookup(parent_ino, seg).unwrap().inode;
        }
        let (name, exp_content) = &expected[i];
        let entry = cold.lookup(parent_ino, name)
            .unwrap_or_else(|_| panic!("file {} missing in {}", name, parent_name));
        let (rh, _, _) = cold.fs.open(&root_ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
        let mut w = TestWriter::new();
        let n = cold.fs.read(&root_ctx(), entry.inode, rh.unwrap(), &mut w, 4096, 0, None, 0).unwrap();
        assert_eq!(&w.buf[..n], exp_content.as_bytes(),
            "{}/{}: expected {:?}, got {:?} ({} bytes)", parent_name, name,
            exp_content, String::from_utf8_lossy(&w.buf[..n]), n);
    }
}

#[test]
fn ruleset_loads_and_routes_ephemeral() {
    // End-to-end integration test: S3Fs::new loads a real TOML file,
    // creates a file at an ephemeral path, writes, releases, and
    // verifies the data is NOT in S3 (ephemeral skips PUT).
    require_s3!();

    // Write a minimal ruleset TOML to a tempfile
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), r#"
[[rule]]
glob = "**/.npm/_cacache/**"
durability = "ephemeral"
"#).unwrap();

    let fix = TestFixture::with_config(|cfg| {
        cfg.ruleset_path = Some(tmp.path().to_path_buf());
    });
    fix.seed_root();

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();

    // Create a nested path that matches the ephemeral rule
    fix.fs.mkdir(&root_ctx(), dir.inode, &cname(".npm"), 0o755, 0).unwrap();
    let npm_dir = fix.lookup(dir.inode, ".npm").unwrap();
    fix.fs.mkdir(&root_ctx(), npm_dir.inode, &cname("_cacache"), 0o755, 0).unwrap();
    let cache_dir = fix.lookup(npm_dir.inode, "_cacache").unwrap();

    // Create + write + release a file under _cacache
    let args = CreateIn {
        flags: libc::O_WRONLY as u32,
        mode: 0o644,
        umask: 0,
        fuse_flags: 0,
    };
    let (entry, handle, _, _) = fix.fs.create(&root_ctx(), cache_dir.inode, &cname("sha256-abc"), args).unwrap();
    let wh = handle.unwrap();
    let data = b"cached package content";
    let mut reader = TestReader::new(data.to_vec());
    fix.fs.write(&root_ctx(), entry.inode, wh, &mut reader,
                 data.len() as u32, 0, None, false, 0, 0).unwrap();
    fix.fs.release(&root_ctx(), entry.inode, 0, wh, true, false, None).unwrap();

    // Drain any durable writes (should be none for this path)
    fix.fs.sync_all();

    // The file should be readable via FUSE (inflight map)
    let found = fix.lookup(cache_dir.inode, "sha256-abc").unwrap();
    let (rh, _, _) = fix.fs.open(&root_ctx(), found.inode, libc::O_RDONLY as u32, 0).unwrap();
    let mut w = TestWriter::new();
    let n = fix.fs.read(&root_ctx(), found.inode, rh.unwrap(), &mut w, 4096, 0, None, 0).unwrap();
    assert_eq!(&w.buf[..n], data, "ephemeral file must be readable via inflight map");

    // S3 should have NO object at all — both create() and release()
    // skip S3 for ephemeral paths.
    let path = fix.path(".npm/_cacache/sha256-abc");
    let meta = s3_rootfs::KeyMeta::new(libc::S_IFREG as u32 | 0o755, 0, 0);
    let key = s3_rootfs::encode_key(&path, meta);
    let result = fix.client.get(&key);
    assert!(result.is_err(),
        "ephemeral file must NOT exist in S3 at all — got Ok({} bytes)",
        result.as_ref().map(|b| b.len()).unwrap_or(0));
}
