//! Drives the server the way a guest would and checks the host
//! directory and the shared index agree after every step.

use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::Path;

use sharedfs::proto::{self, ROOT_INO, ROOT_SLOT, ino_of, map, mask, setattr};
use sharedfs::{Request as R, Response as P, Server, Window};

const REGION: usize = 4 << 20;
const WINDOW: usize = 16 << 20;

fn server(root: &Path) -> Server {
    Server::open(root, REGION, Some(Window::reserve(WINDOW).unwrap()), None, false).unwrap()
}

/// Resolve a `/`-separated relative path through the index.
fn find(s: &Server, path: &str) -> Option<u32> {
    let mut slot = ROOT_SLOT;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        slot = s.index().lookup(slot, part.as_bytes())?;
    }
    Some(slot)
}

fn names(s: &Server, slot: u32) -> Vec<String> {
    let mut v: Vec<String> =
        s.index().children(slot).map(|c| String::from_utf8(s.index().live(c).unwrap().name().to_vec()).unwrap()).collect();
    v.sort();
    v
}

/// The inode number the guest would hold for a live slot.
fn ino(s: &Server, slot: u32) -> u64 {
    ino_of(slot, s.index().live(slot).unwrap().generation)
}

fn ok(s: &mut Server, r: R) -> P {
    s.handle(r).unwrap()
}

#[test]
fn scan_mirrors_host_tree() {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir_all(t.path().join("src/deep")).unwrap();
    fs::write(t.path().join("src/main.rs"), b"fn main() {}").unwrap();
    fs::write(t.path().join("src/deep/x"), b"x").unwrap();
    symlink("src/main.rs", t.path().join("link")).unwrap();

    let s = server(t.path());
    assert_eq!(s.index().live_count(), 6, "root + src + deep + main.rs + x + link");
    assert_eq!(names(&s, ROOT_SLOT), ["link", "src"]);
    let main = find(&s, "src/main.rs").unwrap();
    let e = s.index().live(main).unwrap();
    assert_eq!(e.size, 12);
    assert_eq!(e.mode & libc::S_IFMT, libc::S_IFREG);
    let md = fs::metadata(t.path().join("src/main.rs")).unwrap();
    assert_eq!(e.mtime, md.mtime() * 1_000_000_000 + md.mtime_nsec());
    let link = find(&s, "link").unwrap();
    assert_eq!(s.index().live(link).unwrap().mode & libc::S_IFMT, libc::S_IFLNK);
    assert_eq!(s.index().path(find(&s, "src/deep/x").unwrap()).unwrap(), Path::new("src/deep/x"));
}

#[test]
fn create_extend_map_write_fsync() {
    let t = tempfile::tempdir().unwrap();
    let mut s = server(t.path());

    let P::Node { ino, generation } =
        ok(&mut s, R::Create { parent: ROOT_INO, mode: 0o644, flags: libc::O_RDWR as u32, name: b"notes.md".to_vec() })
    else {
        panic!()
    };
    assert_eq!(generation, 1);
    let slot = find(&s, "notes.md").unwrap();
    assert_eq!(ino_of(slot, generation), ino);
    assert_eq!(fs::metadata(t.path().join("notes.md")).unwrap().mode() & 0o777, 0o644);

    // The guest grows the file before writing past its end, then maps a chunk for writing and
    // stores straight into the host page cache.
    assert_eq!(ok(&mut s, R::Extend { ino, size: 11 }), P::Empty);
    assert_eq!(s.index().live(slot).unwrap().size, 11);
    assert_eq!(ok(&mut s, R::Map { ino, generation, foff: 0, len: 4096, woff: 0, flags: map::WRITE }), P::Empty);
    let base = s.window().unwrap().as_ptr();
    // SAFETY: Offset 0 of the window is now the first page of notes.md, mapped writable.
    unsafe { std::ptr::copy_nonoverlapping(b"hello world".as_ptr(), base, 11) };
    assert_eq!(ok(&mut s, R::Fsync { ino }), P::Empty);
    assert_eq!(fs::read_to_string(t.path().join("notes.md")).unwrap(), "hello world");

    // A host write shows through the mapping with no request at all.
    fs::write(t.path().join("notes.md"), b"HELLO world").unwrap();
    // SAFETY: Same mapping, still live.
    let seen = unsafe { std::slice::from_raw_parts(base, 11) };
    assert_eq!(seen, b"HELLO world");

    // Refused: a stale generation, a directory, a range outside the window, an unaligned offset.
    let stale = ino_of(slot, generation + 1);
    assert_eq!(s.handle(R::Map { ino: stale, generation: generation + 1, foff: 0, len: 4096, woff: 4096, flags: 0 }), Err(libc::ESTALE));
    let root_gen = s.index().live(ROOT_SLOT).unwrap().generation;
    assert_eq!(s.handle(R::Map { ino: ROOT_INO, generation: root_gen, foff: 0, len: 4096, woff: 4096, flags: 0 }), Err(libc::EINVAL));
    assert_eq!(s.handle(R::Map { ino, generation, foff: 0, len: 4096, woff: WINDOW as u64, flags: 0 }), Err(libc::ERANGE));
    assert_eq!(s.handle(R::Map { ino, generation, foff: 0, len: 4096, woff: 100, flags: 0 }), Err(libc::EINVAL));

    assert_eq!(ok(&mut s, R::Unmap { woff: 0, len: 4096 }), P::Empty);

    // Extend never shrinks.
    assert_eq!(ok(&mut s, R::Extend { ino, size: 3 }), P::Empty);
    assert_eq!(s.index().live(slot).unwrap().size, 11);

    // Duplicate create is refused by the host and the index is untouched.
    assert_eq!(
        s.handle(R::Create { parent: ROOT_INO, mode: 0o644, flags: 0, name: b"notes.md".to_vec() }),
        Err(libc::EEXIST)
    );
    assert_eq!(s.index().live_count(), 2);
}

#[test]
fn mkdir_rename_unlink_rmdir() {
    let t = tempfile::tempdir().unwrap();
    let mut s = server(t.path());

    let P::Node { ino: dir, .. } = ok(&mut s, R::Mkdir { parent: ROOT_INO, mode: 0o755, name: b"a".to_vec() }) else { panic!() };
    let P::Node { ino: sub, .. } = ok(&mut s, R::Mkdir { parent: dir, mode: 0o755, name: b"b".to_vec() }) else { panic!() };
    ok(&mut s, R::Create { parent: sub, mode: 0o600, flags: 0, name: b"f".to_vec() });
    assert!(t.path().join("a/b/f").is_file());

    // Rename across directories, then over an existing target.
    ok(&mut s, R::Mkdir { parent: ROOT_INO, mode: 0o755, name: b"c".to_vec() });
    let c = find(&s, "c").unwrap();
    let c_ino = ino_of(c, s.index().live(c).unwrap().generation);
    ok(&mut s, R::Rename { parent: sub, name: b"f".to_vec(), new_parent: c_ino, new_name: b"g".to_vec(), flags: 0 });
    assert!(t.path().join("c/g").is_file() && !t.path().join("a/b/f").exists());
    assert!(find(&s, "a/b/f").is_none() && find(&s, "c/g").is_some());
    assert_eq!(names(&s, find(&s, "a/b").unwrap()), Vec::<String>::new());

    ok(&mut s, R::Create { parent: c_ino, mode: 0o600, flags: 0, name: b"h".to_vec() });
    let before = s.index().live_count();
    ok(&mut s, R::Rename { parent: c_ino, name: b"g".to_vec(), new_parent: c_ino, new_name: b"h".to_vec(), flags: 0 });
    assert_eq!(s.index().live_count(), before - 1, "replaced target left the index");
    assert_eq!(names(&s, c), ["h"]);
    assert_eq!(fs::read_dir(t.path().join("c")).unwrap().count(), 1);

    // Non-empty rmdir is refused; after unlink it succeeds.
    assert_eq!(s.handle(R::Rmdir { parent: ROOT_INO, name: b"c".to_vec() }), Err(libc::ENOTEMPTY));
    assert_eq!(s.handle(R::Unlink { parent: ROOT_INO, name: b"c".to_vec() }), Err(libc::EISDIR));
    ok(&mut s, R::Unlink { parent: c_ino, name: b"h".to_vec() });
    ok(&mut s, R::Rmdir { parent: ROOT_INO, name: b"c".to_vec() });
    assert!(!t.path().join("c").exists() && find(&s, "c").is_none());
    assert_eq!(s.handle(R::Rmdir { parent: ROOT_INO, name: b"c".to_vec() }), Err(libc::ENOENT));

    // Slot reuse bumps the generation.
    let P::Node { ino: again, generation } = ok(&mut s, R::Mkdir { parent: ROOT_INO, mode: 0o755, name: b"c2".to_vec() }) else { panic!() };
    assert!(generation >= 2, "reused slot {again} should carry generation >= 2, got {generation}");
}

#[test]
fn symlink_readlink_and_setattr() {
    let t = tempfile::tempdir().unwrap();
    let mut s = server(t.path());
    let P::Node { ino: l, .. } = ok(&mut s, R::Symlink { parent: ROOT_INO, name: b"l".to_vec(), target: b"/etc/hostname".to_vec() }) else { panic!() };
    assert_eq!(ok(&mut s, R::Readlink { ino: l }), P::Readlink { target: b"/etc/hostname".to_vec() });
    assert_eq!(s.handle(R::Readlink { ino: ROOT_INO }), Err(libc::EINVAL));

    let P::Node { ino, .. } = ok(&mut s, R::Create { parent: ROOT_INO, mode: 0o644, flags: 0, name: b"f".to_vec() }) else { panic!() };
    ok(&mut s, R::Setattr { ino, valid: setattr::MODE | setattr::SIZE | setattr::MTIME, mode: 0o600, uid: 0, gid: 0, size: 4096, atime: 0, mtime: 1_700_000_000_000_000_123 });
    let md = fs::metadata(t.path().join("f")).unwrap();
    assert_eq!(md.mode() & 0o777, 0o600);
    assert_eq!(md.len(), 4096);
    assert_eq!(md.mtime(), 1_700_000_000);
    assert_eq!(md.mtime_nsec(), 123);
    let e = s.index().live(find(&s, "f").unwrap()).unwrap();
    assert_eq!((e.mode & 0o777, e.size, e.mtime), (0o600, 4096, 1_700_000_000_000_000_123));

    let P::Statfs { bsize, namelen, .. } = ok(&mut s, R::Statfs) else { panic!() };
    assert!(bsize > 0 && namelen >= 255);
}

#[test]
fn requests_cannot_escape_the_root() {
    let t = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), t.path().join("esc")).unwrap();
    fs::create_dir(t.path().join("d")).unwrap();
    let mut s = server(t.path());

    // A symlink is a leaf in the index: using it as a parent is ENOTDIR.
    let esc = ino(&s, find(&s, "esc").unwrap());
    assert_eq!(s.handle(R::Create { parent: esc, mode: 0o644, flags: 0, name: b"x".to_vec() }), Err(libc::ENOTDIR));

    // Names are single components.
    for bad in [&b"../x"[..], b"a/b", b".", b"..", b"", &[b'x'; 256]] {
        assert_eq!(s.handle(R::Mkdir { parent: ROOT_INO, mode: 0o755, name: bad.to_vec() }), Err(libc::EINVAL), "{bad:?}");
    }

    // If the host tree is tampered with so a directory becomes a symlink,
    // resolution refuses rather than following it.
    let d = ino(&s, find(&s, "d").unwrap());
    fs::remove_dir(t.path().join("d")).unwrap();
    symlink(outside.path(), t.path().join("d")).unwrap();
    assert!(matches!(s.handle(R::Create { parent: d, mode: 0o644, flags: 0, name: b"x".to_vec() }), Err(libc::ELOOP | libc::ENOTDIR)));
    assert!(fs::read_dir(outside.path()).unwrap().next().is_none(), "nothing was written outside");
}

#[test]
fn seqlock_reads_match_direct_reads() {
    let t = tempfile::tempdir().unwrap();
    for i in 0..50 {
        fs::write(t.path().join(format!("f{i}")), vec![b'x'; i]).unwrap();
    }
    let mut s = server(t.path());
    for slot in 0..s.index().entry_cap() {
        match (s.index().read(slot), s.index().entry(slot)) {
            (Some(copy), Some(direct)) => {
                assert_eq!(copy.seq & 1, 0, "stable entries have even seq");
                assert_eq!(copy.name(), direct.name());
                assert_eq!((copy.size, copy.flags, copy.parent), (direct.size, direct.flags, direct.parent));
            }
            _ => break,
        }
    }
    // Every write bumps seq by exactly two.
    let slot = find(&s, "f7").unwrap();
    let before = s.index().read(slot).unwrap().seq;
    let notes = ino(&s, slot);
    ok(&mut s, R::Extend { ino: notes, size: 3 });
    assert_eq!(s.index().read(slot).unwrap().seq, before + 2);
}

#[test]
fn index_fills_to_enospc_and_recovers() {
    let t = tempfile::tempdir().unwrap();
    let mut s = Server::open(t.path(), 64 << 10, None, None, false).unwrap();
    let cap = s.index().entry_cap();
    for i in 1..cap {
        ok(&mut s, R::Mkdir { parent: ROOT_INO, mode: 0o755, name: format!("d{i}").into_bytes() });
    }
    assert_eq!(s.handle(R::Mkdir { parent: ROOT_INO, mode: 0o755, name: b"one-too-many".to_vec() }), Err(libc::ENOSPC));
    assert!(!t.path().join("one-too-many").exists(), "capacity is checked before the host is touched");
    ok(&mut s, R::Rmdir { parent: ROOT_INO, name: b"d1".to_vec() });
    ok(&mut s, R::Mkdir { parent: ROOT_INO, mode: 0o755, name: b"fits-now".to_vec() });
    assert_eq!(s.index().live_count(), cap);
}

#[test]
fn wire_roundtrip() {
    let reqs = [
        R::Map { ino: 7, generation: 2, foff: 1 << 40, len: 2 << 20, woff: 6 << 20, flags: map::WRITE },
        R::Extend { ino: 3, size: 1 << 40 },
        R::Rename { parent: 1, name: b"a".to_vec(), new_parent: 9, new_name: b"b".to_vec(), flags: 0 },
        R::Setattr { ino: 2, valid: 63, mode: 1, uid: 2, gid: 3, size: 4, atime: -5, mtime: 6 },
        R::Statfs,
    ];
    for (i, r) in reqs.iter().enumerate() {
        let bytes = r.encode(100 + i as u64);
        let (unique, back) = R::decode(&bytes).unwrap();
        assert_eq!((unique, &back), (100 + i as u64, r));
        assert!(R::decode(&bytes[..bytes.len() - 1]).is_err(), "truncated input is rejected");
    }
    let resps: [(u32, Result<P, i32>); 4] = [
        (proto::op::CREATE, Ok(P::Node { ino: 5, generation: 2 })),
        (proto::op::MAP, Ok(P::Empty)),
        (proto::op::UNLINK, Ok(P::Empty)),
        (proto::op::EXTEND, Err(libc::ENOENT)),
    ];
    for (opc, r) in resps.iter() {
        let bytes = P::encode(42, r);
        assert_eq!(P::decode(*opc, &bytes).unwrap(), (42, r.clone()));
    }
}

#[test]
fn host_side_changes_reach_index() {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir_all(t.path().join("src")).unwrap();
    fs::write(t.path().join("src/a.txt"), b"a").unwrap();
    let mut s = server(t.path());
    assert_eq!(names(&s, ROOT_SLOT), vec!["src"]);

    // Created, grown, removed and renamed on the host, plus a new directory with content.
    fs::write(t.path().join("late.txt"), b"late").unwrap();
    fs::write(t.path().join("src/a.txt"), b"aaaa").unwrap();
    fs::create_dir(t.path().join("src/newdir")).unwrap();
    fs::write(t.path().join("src/newdir/inner"), b"i").unwrap();
    s.pump_watch();
    assert_eq!(names(&s, ROOT_SLOT), vec!["late.txt", "src"]);
    let a = find(&s, "src/a.txt").unwrap();
    assert_eq!(s.index().live(a).unwrap().size, 4);
    let newdir = find(&s, "src/newdir").unwrap();
    assert_eq!(names(&s, newdir), vec!["inner"]);

    // The new directory is watched too.
    fs::write(t.path().join("src/newdir/second"), b"2").unwrap();
    fs::remove_file(t.path().join("src/a.txt")).unwrap();
    fs::rename(t.path().join("late.txt"), t.path().join("src/moved.txt")).unwrap();
    s.pump_watch();
    assert_eq!(names(&s, newdir), vec!["inner", "second"]);
    assert_eq!(names(&s, ROOT_SLOT), vec!["src"]);
    let src = find(&s, "src").unwrap();
    assert_eq!(names(&s, src), vec!["moved.txt", "newdir"]);
    assert!(find(&s, "src/a.txt").is_none());

    // A directory moved on the host keeps its slot, so guest inodes under it survive.
    let before = find(&s, "src/newdir").unwrap();
    fs::rename(t.path().join("src/newdir"), t.path().join("src/renamed")).unwrap();
    let n = s.pump_watch();
    assert_eq!(find(&s, "src/renamed"), Some(before));
    assert!(find(&s, "src/newdir").is_none());
    assert_eq!(n.len(), 2, "moved_from and moved_to: {n:?}");
    fs::rename(t.path().join("src/renamed"), t.path().join("src/newdir")).unwrap();
    s.pump_watch();

    // Removing a whole subtree on the host drops every slot under it.
    let live_before = s.index().live_count();
    fs::remove_dir_all(t.path().join("src/newdir")).unwrap();
    s.pump_watch();
    assert!(find(&s, "src/newdir").is_none());
    assert_eq!(s.index().live_count(), live_before - 3);

    // Guest-made entries still work alongside the watcher.
    let src_ino = ino(&s, src);
    ok(&mut s, R::Mkdir { parent: src_ino, mode: 0o755, name: b"from_guest".to_vec() });
    fs::write(t.path().join("src/from_guest/x"), b"x").unwrap();
    s.pump_watch();
    assert_eq!(names(&s, find(&s, "src/from_guest").unwrap()), vec!["x"]);
}

#[test]
fn resized_index_is_adopted_next_time() {
    let t = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let index = state.path().join("index");
    for i in 0..10 {
        fs::write(t.path().join(format!("f{i}")), b"x").unwrap();
    }
    drop(Server::open(t.path(), 1 << 20, None, Some(&index), false).unwrap());
    // A bigger index starts afresh; the one after it must be adoptable.
    drop(Server::open(t.path(), 4 << 20, None, Some(&index), false).unwrap());
    fs::remove_file(t.path().join("f3")).unwrap();
    let s = Server::open(t.path(), 4 << 20, None, Some(&index), false).unwrap();
    assert!(s.take_pending_len() > 0, "the resized index was adopted and reconciled");
    assert!(find(&s, "f3").is_none());
}

#[test]
fn index_is_adopted_and_reconciled() {
    let t = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let index = state.path().join("index");
    fs::create_dir(t.path().join("keep")).unwrap();
    fs::write(t.path().join("keep/stay.txt"), b"one").unwrap();
    fs::write(t.path().join("gone.txt"), b"x").unwrap();
    fs::write(t.path().join("edit.txt"), b"old").unwrap();
    let (stay, stay_gen, gone) = {
        let s = Server::open(t.path(), 1 << 20, None, Some(&index), false).unwrap();
        assert!(s.take_pending_len() == 0, "a fresh index has nothing to report");
        let stay = find(&s, "keep/stay.txt").unwrap();
        (stay, s.index().live(stay).unwrap().generation, find(&s, "gone.txt").unwrap())
    };

    // With no daemon running: a removal, a rewrite and an addition.
    fs::remove_file(t.path().join("gone.txt")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(t.path().join("edit.txt"), b"something longer").unwrap();
    fs::write(t.path().join("keep/added.txt"), b"a").unwrap();

    let mut s = Server::open(t.path(), 1 << 20, None, Some(&index), false).unwrap();
    assert_eq!(find(&s, "keep/stay.txt"), Some(stay), "surviving entries keep their slot");
    assert_eq!(s.index().live(stay).unwrap().generation, stay_gen, "and their generation");
    assert!(find(&s, "gone.txt").is_none());
    assert!(find(&s, "keep/added.txt").is_some());
    assert_eq!(s.index().live(find(&s, "edit.txt").unwrap()).unwrap().size, 16);

    let mut events: Vec<(u32, Vec<u8>)> = s
        .take_pending()
        .into_iter()
        .filter_map(|n| match n {
            proto::Notification::Event { mask, name, .. } => Some((mask, name)),
            _ => None,
        })
        .collect();
    events.sort();
    assert_eq!(
        events,
        // The directory that gained a file changed too: its mtime moved.
        [
            (mask::MODIFY, b"edit.txt".to_vec()),
            (mask::CREATE, b"added.txt".to_vec()),
            (mask::DELETE, b"gone.txt".to_vec()),
            (mask::MODIFY | mask::ISDIR, b"keep".to_vec()),
        ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    let _ = gone;

    // The slot freed by gone.txt was reused during the reconcile (by added.txt), with a higher
    // generation, so the inode number a guest held for gone.txt is refused rather than reaching
    // the new file.
    assert_eq!(find(&s, "keep/added.txt"), Some(gone));
    assert_eq!(s.index().live(gone).unwrap().generation, 2);
    assert_eq!(s.handle(R::Fsync { ino: ino_of(gone, 1) }), Err(libc::ESTALE));
    assert_eq!(ok(&mut s, R::Fsync { ino: ino_of(gone, 2) }), P::Empty);
}

#[test]
fn nested_exports_are_refused() {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir_all(t.path().join("inner/leaf")).unwrap();
    let outer = Server::open(t.path(), 1 << 20, None, None, false).unwrap();
    assert!(Server::open(&t.path().join("inner"), 1 << 20, None, None, false).is_err(), "inside an export");
    drop(outer);
    let _inner = Server::open(&t.path().join("inner"), 1 << 20, None, None, false).unwrap();
    assert!(Server::open(t.path(), 1 << 20, None, None, false).is_err(), "enclosing an export");
    assert!(Server::open(&t.path().join("inner/leaf"), 1 << 20, None, None, false).is_err(), "inside, two levels down");
}

#[test]
fn read_only_exports_are_shared_and_refuse_writes() {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("f"), b"x").unwrap();
    let mut a = Server::open(t.path(), 1 << 20, None, None, true).unwrap();
    let _b = Server::open(t.path(), 1 << 20, None, None, true).expect("two readers share a read-only export");
    assert!(Server::open(t.path(), 1 << 20, None, None, false).is_err(), "no writer while it is shared");
    assert_eq!(a.handle(R::Create { parent: ROOT_INO, mode: 0o644, flags: 0, name: b"g".to_vec() }), Err(libc::EROFS));
    assert_eq!(a.handle(R::Unlink { parent: ROOT_INO, name: b"f".to_vec() }), Err(libc::EROFS));
    assert!(t.path().join("f").exists() && !t.path().join("g").exists());
}

#[test]
fn write_access_is_decided_at_open() {
    // git's packfile pattern: create with mode 0444 and write through the creator's descriptor.
    let t = tempfile::tempdir().unwrap();
    let mut s = server(t.path());
    let P::Node { ino, generation } =
        ok(&mut s, R::Create { parent: ROOT_INO, mode: 0o444, flags: libc::O_RDWR as u32, name: b"tmp_pack".to_vec() })
    else {
        panic!()
    };
    ok(&mut s, R::OpenWrite { ino });
    assert_eq!(ok(&mut s, R::Extend { ino, size: 5 }), P::Empty, "grow a 0444 file the guest has open");
    assert_eq!(ok(&mut s, R::Map { ino, generation, foff: 0, len: 4096, woff: 0, flags: map::WRITE }), P::Empty);
    let base = s.window().unwrap().as_ptr();
    // SAFETY: offset 0 of the window is now the file's first page, mapped writable.
    unsafe { std::ptr::copy_nonoverlapping(b"PACK!".as_ptr(), base, 5) };
    ok(&mut s, R::Fsync { ino });
    ok(&mut s, R::Unmap { woff: 0, len: 4096 });
    assert_eq!(fs::read(t.path().join("tmp_pack")).unwrap(), b"PACK!");
    assert_eq!(fs::metadata(t.path().join("tmp_pack")).unwrap().mode() & 0o777, 0o444);

    // A chmod after the open does not take the access away from the open file.
    let P::Node { ino: other, .. } =
        ok(&mut s, R::Create { parent: ROOT_INO, mode: 0o644, flags: libc::O_RDWR as u32, name: b"f".to_vec() })
    else {
        panic!()
    };
    ok(&mut s, R::OpenWrite { ino: other });
    ok(&mut s, R::Setattr { ino: other, valid: setattr::MODE, mode: 0o444, uid: 0, gid: 0, size: 0, atime: 0, mtime: 0 });
    assert_eq!(ok(&mut s, R::Extend { ino: other, size: 3 }), P::Empty, "fchmod 0444 after open, then write");
    assert_eq!(
        ok(&mut s, R::Setattr { ino: other, valid: setattr::SIZE, mode: 0, uid: 0, gid: 0, size: 1, atime: 0, mtime: 0 }),
        P::Empty,
        "ftruncate on the open file"
    );

    // Once the last writer has closed it, a new write access is checked against the mode again.
    ok(&mut s, R::ReleaseWrite { ino });
    ok(&mut s, R::ReleaseWrite { ino: other });
    assert_eq!(s.handle(R::OpenWrite { ino }), Err(libc::EACCES));
    assert_eq!(s.handle(R::Extend { ino, size: 10 }), Err(libc::EACCES));
}

#[test]
fn host_replacement_is_a_new_file() {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("f.rs"), vec![b'o'; 23594]).unwrap();
    let mut s = server(t.path());
    let slot = find(&s, "f.rs").unwrap();
    let generation = s.index().live(slot).unwrap().generation;
    let old = ino_of(slot, generation);
    ok(&mut s, R::OpenRead { ino: old });
    ok(&mut s, R::Map { ino: old, generation, foff: 0, len: 2 << 20, woff: 0, flags: 0 });

    // An editor's save: a temporary file renamed over the original, both done before the
    // daemon looks, so it never sees the temporary name.
    fs::write(t.path().join(".f.rs.tmp"), vec![b'n'; 26010]).unwrap();
    fs::rename(t.path().join(".f.rs.tmp"), t.path().join("f.rs")).unwrap();
    s.pump_watch();

    let now = find(&s, "f.rs").unwrap();
    let e = *s.index().live(now).unwrap();
    let fresh = ino_of(now, e.generation);
    assert_ne!(fresh, old, "a replaced file gets a new inode number");
    assert_eq!(e.size, 26010);
    // The old inode keeps reading the old file through its handle, as an open descriptor
    // does on any file system; the new one maps the new file.
    // SAFETY: offset 0 of the window maps the old file's first chunk.
    let page = unsafe { std::slice::from_raw_parts(s.window().unwrap().as_ptr(), 4096) };
    assert!(page.iter().all(|&b| b == b'o'));
    ok(&mut s, R::Unmap { woff: 0, len: 2 << 20 });
    ok(&mut s, R::Map { ino: fresh, generation: e.generation, foff: 0, len: 2 << 20, woff: 0, flags: 0 });
    // SAFETY: as above, now for the new file.
    let page = unsafe { std::slice::from_raw_parts(s.window().unwrap().as_ptr().add(24576), 1434) };
    assert!(page.iter().all(|&b| b == b'n'), "the new file's tail, past the old file's end");

    // Changed in place, a file keeps its inode number.
    fs::write(t.path().join("f.rs"), vec![b'm'; 100]).unwrap();
    s.pump_watch();
    let again = find(&s, "f.rs").unwrap();
    assert_eq!(ino_of(again, s.index().live(again).unwrap().generation), fresh);
    ok(&mut s, R::ReleaseRead { ino: old });
}

#[test]
fn unlinked_open_files_stay_reachable() {
    const CHUNK: u64 = 2 << 20;
    let t = tempfile::tempdir().unwrap();
    let mut s = server(t.path());
    let mut big = vec![0u8; 3 << 20];
    for (i, b) in big.iter_mut().enumerate() {
        *b = (i / 4096) as u8;
    }
    fs::write(t.path().join("big"), &big).unwrap();
    s.pump_watch();
    let slot = find(&s, "big").unwrap();
    let generation = s.index().live(slot).unwrap().generation;
    let ino = ino_of(slot, generation);

    // Open for reading, then unlink: the second chunk must still map, with the old data.
    ok(&mut s, R::OpenRead { ino });
    ok(&mut s, R::Unlink { parent: ROOT_INO, name: b"big".to_vec() });
    assert!(find(&s, "big").is_none());
    assert_eq!(ok(&mut s, R::Map { ino, generation, foff: CHUNK, len: 4096, woff: 0, flags: 0 }), P::Empty);
    // SAFETY: offset 0 of the window now holds the unlinked file's page at CHUNK.
    let page = unsafe { std::slice::from_raw_parts(s.window().unwrap().as_ptr(), 4096) };
    assert!(page.iter().all(|&b| b == (CHUNK / 4096) as u8), "old data, not a new file's");
    ok(&mut s, R::Unmap { woff: 0, len: 4096 });

    // A new file reusing the slot is a different file with a different inode number.
    let P::Node { ino: fresh, .. } =
        ok(&mut s, R::Create { parent: ROOT_INO, mode: 0o644, flags: libc::O_RDWR as u32, name: b"new".to_vec() })
    else {
        panic!()
    };
    assert_eq!(proto::slot_of(fresh), Some(slot), "slot reused");
    assert_ne!(fresh, ino);
    ok(&mut s, R::OpenWrite { ino: fresh });
    ok(&mut s, R::Extend { ino: fresh, size: 4096 });
    assert_eq!(fs::metadata(t.path().join("new")).unwrap().len(), 4096);

    // Open for writing, unlink, then grow and truncate through the handle.
    fs::write(t.path().join("w"), b"abc").unwrap();
    s.pump_watch();
    let ws = find(&s, "w").unwrap();
    let wino = ino_of(ws, s.index().live(ws).unwrap().generation);
    ok(&mut s, R::OpenWrite { ino: wino });
    ok(&mut s, R::Unlink { parent: ROOT_INO, name: b"w".to_vec() });
    assert_eq!(ok(&mut s, R::Extend { ino: wino, size: 8192 }), P::Empty);
    assert_eq!(
        ok(&mut s, R::Setattr { ino: wino, valid: setattr::SIZE, mode: 0, uid: 0, gid: 0, size: 1, atime: 0, mtime: 0 }),
        P::Empty
    );
    assert_eq!(ok(&mut s, R::Fsync { ino: wino }), P::Empty);

    // Once the guest has closed it, the unlinked file is gone for good.
    ok(&mut s, R::ReleaseRead { ino });
    ok(&mut s, R::ReleaseWrite { ino: wino });
    assert_eq!(s.handle(R::Map { ino, generation, foff: 0, len: 4096, woff: 0, flags: 0 }), Err(libc::ESTALE));
    assert_eq!(s.handle(R::Extend { ino: wino, size: 1 }), Err(libc::ESTALE));
}

#[test]
fn links_cost_the_same_however_many_exist() {
    // Incremental compilation hard-links thousands of files per session. Finding a linked
    // file's other names must not cost a stat of every multiply-linked file there is.
    let t = tempfile::tempdir().unwrap();
    for i in 0..5000 {
        fs::write(t.path().join(format!("o{i}")), b"x").unwrap();
        fs::hard_link(t.path().join(format!("o{i}")), t.path().join(format!("l{i}"))).unwrap();
    }
    let mut s = Server::open(t.path(), 16 << 20, None, None, false).unwrap();
    let started = std::time::Instant::now();
    for i in 0..400 {
        let src = ino(&s, find(&s, &format!("o{i}")).unwrap());
        ok(&mut s, R::Link { ino: src, parent: ROOT_INO, name: format!("n{i}").into_bytes() });
    }
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_secs(2), "400 links took {took:?}");
    // Every name of a linked file sees the new count.
    for name in ["o7", "l7", "n7"] {
        assert_eq!(s.index().live(find(&s, name).unwrap()).unwrap().nlink, 3, "{name}");
    }
}

#[test]
fn hard_links() {
    let t = tempfile::tempdir().unwrap();
    let mut s = server(t.path());
    fs::write(t.path().join("a"), b"shared").unwrap();
    s.pump_watch();
    let a = find(&s, "a").unwrap();
    let P::Node { ino, .. } = ok(&mut s, R::Mkdir { parent: ROOT_INO, mode: 0o755, name: b"d".to_vec() }) else { panic!() };
    let a_ino = ino_of(a, s.index().live(a).unwrap().generation);
    let P::Node { ino: b, .. } = ok(&mut s, R::Link { ino: a_ino, parent: ino, name: b"b".to_vec() }) else { panic!() };
    assert_eq!(fs::read(t.path().join("d/b")).unwrap(), b"shared");
    assert_eq!(fs::metadata(t.path().join("a")).unwrap().ino(), fs::metadata(t.path().join("d/b")).unwrap().ino());
    assert_eq!(s.index().live(a).unwrap().nlink, 2, "the old name's link count is refreshed");
    assert_eq!(s.index().live(proto::slot_of(b).unwrap()).unwrap().nlink, 2);
    // Removing one name leaves the other with one link.
    ok(&mut s, R::Unlink { parent: ROOT_INO, name: b"a".to_vec() });
    assert_eq!(s.index().live(proto::slot_of(b).unwrap()).unwrap().nlink, 1, "the other name is refreshed");
    // Directories cannot be linked; an existing name is refused.
    assert_eq!(s.handle(R::Link { ino, parent: ROOT_INO, name: b"d2".to_vec() }), Err(libc::EPERM));
    fs::write(t.path().join("c"), b"x").unwrap();
    s.pump_watch();
    assert_eq!(s.handle(R::Link { ino: b, parent: ROOT_INO, name: b"c".to_vec() }), Err(libc::EEXIST));
}
