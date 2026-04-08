mod harness;

use fuse_backend_rs::api::filesystem::DirEntry;
use fuse_backend_rs::api::filesystem::FileSystem;
use harness::context::root_ctx;
use harness::fixture::TestFixture;
use harness::s3::require_s3;

const ROOT: u64 = 1;

fn collect_readdir(fix: &TestFixture, ino: u64) -> Vec<(String, u32)> {
    let (dh, _) = fix.fs.opendir(&root_ctx(), ino, 0).unwrap();
    let mut entries = Vec::new();
    fix.fs.readdir(&root_ctx(), ino, dh.unwrap(), 4096, 0,
        &mut |de: DirEntry| {
            entries.push((
                String::from_utf8_lossy(de.name).to_string(),
                de.type_,
            ));
            Ok(de.name.len())
        }).unwrap();
    entries
}

#[test]
fn readdir_lists_children() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("a.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    fix.seed_file("b.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    fix.seed_file("c.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entries = collect_readdir(&fix, dir.inode);

    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"a.txt"), "missing a.txt in {:?}", names);
    assert!(names.contains(&"b.txt"), "missing b.txt in {:?}", names);
    assert!(names.contains(&"c.txt"), "missing c.txt in {:?}", names);
}

#[test]
fn readdir_includes_subdirs() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("file.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    fix.seed_dir("subdir");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entries = collect_readdir(&fix, dir.inode);

    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"file.txt"), "missing file.txt in {:?}", names);
    assert!(names.contains(&"subdir"), "missing subdir in {:?}", names);

    // Verify types
    let file_type = entries.iter().find(|(n, _)| n == "file.txt").unwrap().1;
    let dir_type = entries.iter().find(|(n, _)| n == "subdir").unwrap().1;
    assert_eq!(file_type, libc::DT_REG as u32);
    assert_eq!(dir_type, libc::DT_DIR as u32);
}

#[test]
fn readdir_empty_dir() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_dir("empty");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let subdir = fix.lookup(dir.inode, "empty").unwrap();
    let entries = collect_readdir(&fix, subdir.inode);

    assert!(entries.is_empty(), "expected empty dir, got {:?}", entries);
}

#[test]
fn readdir_with_offset() {
    require_s3!();
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("aa.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    fix.seed_file("bb.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    fix.seed_file("cc.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");

    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let (dh, _) = fix.fs.opendir(&root_ctx(), dir.inode, 0).unwrap();

    // Read with offset=1 should skip the first entry
    let mut entries = Vec::new();
    fix.fs.readdir(&root_ctx(), dir.inode, dh.unwrap(), 4096, 1,
        &mut |de: DirEntry| {
            entries.push(String::from_utf8_lossy(de.name).to_string());
            Ok(de.name.len())
        }).unwrap();

    // Should have fewer entries than the full listing
    let full = collect_readdir(&fix, dir.inode);
    assert!(entries.len() < full.len(),
        "offset=1 should return fewer entries: got {} vs full {}",
        entries.len(), full.len());
}

#[test]
fn opendir_returns_handle() {
    require_s3!();
    let fix = TestFixture::new();

    let (handle, _) = fix.fs.opendir(&root_ctx(), ROOT, 0).unwrap();
    assert!(handle.is_some());
}
