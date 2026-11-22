//! Executes requests against the host directory and keeps the index in step.
//!
//! Every operation resolves its target to a directory fd opened with
//! `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` plus a single final
//! name component, so nothing a request says can reach outside the root.
//! Mutations hit the host first and the index second.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::time::{Duration, Instant};
use std::fs::{self, File, Metadata, Permissions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use nix::fcntl::{AtFlags, Flock, FlockArg, OFlag, OpenHow, ResolveFlag, open, openat, openat2, readlinkat, renameat};
use nix::sys::stat::{Mode, fchmod, fstat, fstatat, futimens, mkdirat, utimensat, UtimensatFlags};
use nix::sys::statvfs::fstatvfs;
use nix::sys::time::TimeSpec;
use nix::unistd::{Gid, Uid, UnlinkatFlags, fchown, fchownat, fsync, ftruncate, linkat, symlinkat, unlinkat};

use crate::index::{Attr, Index, check_name};
use crate::proto::*;
use crate::watch::{Change, Watcher};
use crate::window::Window;

pub type Errno = i32;

/// Export roots served from this process, as `(dev, ino)`: several exports share one VMM, and
/// the lock table cannot tell them apart from each other.
static EXPORTS: LazyLock<Mutex<HashSet<(u64, u64)>>> = LazyLock::new(Default::default);

pub struct Server {
    root_fd: OwnedFd,
    root_id: (u64, u64),
    /// The host is the writer; the guest may not change anything.
    read_only: bool,
    /// Exclusive advisory lock on the export root for the life of the server: one writer per
    /// directory. Host editors never take it; a second daemon on the same directory is refused.
    _lock: Flock<OwnedFd>,
    root: PathBuf,
    index: Index,
    watch: Watcher,
    /// The data window, when the VMM provides one.
    window: Option<Window>,
    /// Live mappings by window offset: the slot mapped there and the length.
    maps: HashMap<u64, (u32, u64)>,
    /// Handles for files the guest has open, by inode number: one for writing while it has
    /// writers, one for reading while it has readers (a mapped file counts as open). Every
    /// request about the file goes through them, so it keeps the access POSIX granted at open
    /// even if the mode changes afterwards (git's 0444 packfiles), and stays reachable after it
    /// is unlinked, when its index slot is gone and may belong to another file. Keyed by inode
    /// number, which carries the generation, so an unlinked file and the new file in its old
    /// slot never share a handle.
    write_handles: HashMap<u64, OwnedFd>,
    read_handles: HashMap<u64, OwnedFd>,
    /// The differences found when an adopted index was reconciled with the directory. They are
    /// for a guest that resumes from an image taken before this daemon started, so they are
    /// handed out at start and again after each device reset, until the guest's first request
    /// shows that a mount has seen them.
    pending: Vec<Notification>,
    /// inotify events this daemon is about to cause by acting on the guest's requests. The
    /// guest's VFS has already reported each of those to its own watchers, so their echoes
    /// are not forwarded. Oldest first; an entry not matched within `ECHO_TTL` is forgotten.
    echoes: VecDeque<Echo>,
    /// Whether the guest has been refused a new name for a full index since there was last
    /// room, so the log says so once per episode rather than once per create.
    full_reported: Cell<bool>,
}

struct Echo {
    dir: u32,
    name: Vec<u8>,
    /// One inotify bit.
    mask: u32,
    at: Instant,
}

/// How long an expected echo may take to arrive. inotify delivers in order and the daemon
/// pumps between requests, so an echo is normally seen on the next pump.
const ECHO_TTL: Duration = Duration::from_secs(2);

fn nx(e: nix::Error) -> Errno {
    e as i32
}

/// Directories other sharedfs daemons hold their export lock on, as `(dev, ino)`. Read from the
/// kernel's lock table: a write flock held by another process that is a VMM, or that runs the
/// same program as this one, which is how the tests see each other.
fn other_export_roots() -> HashSet<(u64, u64)> {
    let me = std::process::id();
    let my_comm = fs::read_to_string("/proc/self/comm").unwrap_or_default();
    let mut out = HashSet::new();
    let Ok(locks) = fs::read_to_string("/proc/locks") else { return out };
    for line in locks.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 6 || f[1] != "FLOCK" || f[3] != "WRITE" {
            continue;
        }
        let Ok(pid) = f[4].parse::<u32>() else { continue };
        if pid == me {
            continue;
        }
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim() != "microvm" && comm != my_comm {
            continue;
        }
        let mut dev_ino = f[5].split(':');
        let (Some(maj), Some(min), Some(ino)) = (dev_ino.next(), dev_ino.next(), dev_ino.next()) else { continue };
        let (Ok(maj), Ok(min), Ok(ino)) = (u32::from_str_radix(maj, 16), u32::from_str_radix(min, 16), ino.parse::<u64>()) else { continue };
        out.insert((libc::makedev(maj, min), ino));
    }
    out
}

fn dir_bit(isdir: bool) -> u32 {
    if isdir { mask::ISDIR } else { 0 }
}

fn io_err(e: io::Error) -> Errno {
    e.raw_os_error().unwrap_or(libc::EIO)
}

fn attr_of(md: &Metadata) -> Attr {
    Attr {
        mode: md.mode(),
        nlink: md.nlink() as u32,
        uid: md.uid(),
        gid: md.gid(),
        size: md.size(),
        rdev: md.rdev(),
        atime: md.atime() * 1_000_000_000 + md.atime_nsec(),
        mtime: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
        ctime: md.ctime() * 1_000_000_000 + md.ctime_nsec(),
        dev: md.dev(),
        ino: md.ino(),
    }
}

fn attr_of_stat(st: &libc::stat) -> Attr {
    Attr {
        mode: st.st_mode,
        nlink: st.st_nlink as u32,
        uid: st.st_uid,
        gid: st.st_gid,
        size: st.st_size as u64,
        rdev: st.st_rdev,
        atime: st.st_atime * 1_000_000_000 + st.st_atime_nsec,
        mtime: st.st_mtime * 1_000_000_000 + st.st_mtime_nsec,
        ctime: st.st_ctime * 1_000_000_000 + st.st_ctime_nsec,
        dev: st.st_dev,
        ino: st.st_ino,
    }
}

/// Open flags a guest may pass through. Creation goes through CREATE.
const OPEN_MASK: i32 = libc::O_ACCMODE | libc::O_APPEND | libc::O_TRUNC | libc::O_NONBLOCK | libc::O_DSYNC | libc::O_SYNC | libc::O_NOATIME;

fn open_flags(flags: u32) -> OFlag {
    OFlag::from_bits_truncate(flags as i32 & OPEN_MASK) | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
}

impl Server {
    /// Serve `root` through an index of `region_len` bytes. With `index_file`, the index lives
    /// in that file and an index left there by an earlier daemon is adopted and reconciled with
    /// the directory, so a guest that kept inode numbers across the gap finds them still valid
    /// and learns of every change as a notification. Without it, the index is anonymous and
    /// built by scanning.
    ///
    /// A writable export makes the guest the directory's writer: it takes an exclusive lock on
    /// the root and is refused inside or around another writable export. A `read_only` export
    /// leaves the host as writer, so any number of guests may share it: it takes a shared lock,
    /// is not counted for nesting, and refuses every request that would change the directory.
    pub fn open(root: &Path, region_len: usize, window: Option<Window>, index_file: Option<&Path>, read_only: bool) -> io::Result<Server> {
        let md = fs::symlink_metadata(root)?;
        if !md.is_dir() {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let root_fd = open(root, OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC, Mode::empty())?;
        let lock_fd = open(root, OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC, Mode::empty())?;
        let mode = if read_only { FlockArg::LockSharedNonblock } else { FlockArg::LockExclusiveNonblock };
        let lock = Flock::lock(lock_fd, mode).map_err(|(_, e)| {
            io::Error::new(io::ErrorKind::AddrInUse, format!("{} is already exported writable ({e})", root.display()))
        })?;
        let root_id = (md.dev(), md.ino());
        // Only writable exports claim a tree; a read-only one claims nothing and checks nothing.
        let mut foreign = if read_only { HashSet::new() } else { other_export_roots() };
        if !read_only {
            foreign.extend(EXPORTS.lock().unwrap().iter().copied());
        }
        for ancestor in root.ancestors().skip(1) {
            if fs::metadata(ancestor).is_ok_and(|m| foreign.contains(&(m.dev(), m.ino()))) {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("{} lies inside the export {}", root.display(), ancestor.display())));
            }
        }
        let (index, adopted) = match index_file {
            Some(p) => Index::open_file(p, region_len, attr_of(&md)),
            None => Index::create(region_len, attr_of(&md)).map(|ix| (ix, false)),
        }
        .map_err(io::Error::from_raw_os_error)?;
        let watch = Watcher::new()?;
        let mut s = Server {
            root_fd,
            root_id,
            read_only,
            _lock: lock,
            root: root.to_path_buf(),
            index,
            watch,
            window,
            maps: HashMap::new(),
            write_handles: HashMap::new(),
            read_handles: HashMap::new(),
            pending: Vec::new(),
            echoes: VecDeque::new(),
            full_reported: Cell::new(false),
        };
        s.watch.add(ROOT_SLOT, root)?;
        if adopted {
            s.index.set_attr(ROOT_SLOT, attr_of(&md)).map_err(io::Error::from_raw_os_error)?;
            s.reconcile(&foreign)?;
        } else {
            s.scan_dir(ROOT_SLOT, root, &foreign)?;
        }
        if !read_only {
            EXPORTS.lock().unwrap().insert(root_id);
        }
        Ok(s)
    }

    /// The reconcile notifications, as long as no request has arrived.
    pub fn take_pending(&mut self) -> Vec<Notification> {
        self.pending.clone()
    }

    /// A request arrived: whichever guest sent it has a working mount, and that mount either
    /// received the reconcile notifications or never needed them.
    pub fn forget_pending(&mut self) {
        self.pending.clear();
    }

    /// How many reconcile notifications are held; for tests.
    pub fn take_pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Brings an adopted index in line with the directory: every directory the index knows is
    /// re-examined name by name, so entries that survived keep their slots, and what was added,
    /// removed or changed while no daemon was watching becomes a notification. Parents are
    /// visited before children, so a directory removed whole is handled once, at its parent.
    fn reconcile(&mut self, foreign: &HashSet<(u64, u64)>) -> io::Result<()> {
        let mut queue = VecDeque::from([ROOT_SLOT]);
        while let Some(dir) = queue.pop_front() {
            let Some(rel) = self.index.path(dir) else { continue };
            let path = self.root.join(&rel);
            let Ok(md) = fs::symlink_metadata(&path) else { continue };
            if !md.is_dir() {
                continue;
            }
            if dir != ROOT_SLOT && foreign.contains(&(md.dev(), md.ino())) {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("{} is exported separately", path.display())));
            }
            let _ = self.watch.add(dir, &path);
            let children: Vec<u32> = self.index.children(dir).filter(|&c| self.index.is_dir(c)).collect();
            let mut events = self.resync_dir(dir);
            self.pending.append(&mut events);
            queue.extend(children.into_iter().filter(|&c| self.index.live(c).is_some()));
        }
        Ok(())
    }

    /// Adds everything under `dir` (known to the index as `slot`) and watches each directory.
    /// A directory another daemon exports makes the whole export fail: one writer per tree.
    fn scan_dir(&mut self, slot: u32, dir: &Path, foreign: &HashSet<(u64, u64)>) -> io::Result<()> {
        let mut stack: Vec<(u32, PathBuf)> = vec![(slot, dir.to_path_buf())];
        while let Some((slot, dir)) = stack.pop() {
            for ent in fs::read_dir(&dir)? {
                let ent = ent?;
                let md = ent.metadata()?; // lstat
                if md.is_dir() && foreign.contains(&(md.dev(), md.ino())) {
                    return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("{} is exported separately", ent.path().display())));
                }
                let name = ent.file_name();
                let child = self
                    .index
                    .insert(slot, name.as_bytes(), attr_of(&md))
                    .map_err(io::Error::from_raw_os_error)?;
                if md.is_dir() {
                    // A directory that vanishes between readdir and here just isn't watched;
                    // its parent's events cover its removal.
                    let _ = self.watch.add(child, &ent.path());
                    stack.push((child, ent.path()));
                }
            }
        }
        Ok(())
    }

    /// The fd to poll for host-side changes; readable when [`Server::pump_watch`] has work.
    pub fn watch_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.watch.fd()
    }

    /// Applies every change made on the host since the last call to the index, and returns
    /// what the guest should be told about them.
    ///
    /// Mutations the daemon performs for the guest also surface here; recognising those echoes
    /// comes with the guest's mutation support.
    pub fn pump_watch(&mut self) -> Vec<Notification> {
        let mut out = Vec::new();
        for change in self.watch.poll() {
            match change {
                Change::Name { slot: dir, name, mask, cookie } => {
                    // The index is brought in line either way; only the report is withheld
                    // for an echo of the guest's own change.
                    let echo = self.consume_echo(dir, &name, mask);
                    if let Some((slot, generation, isdir)) = self.sync_name(dir, &name) {
                        if !echo {
                            let mask = if isdir { mask | mask::ISDIR } else { mask & !mask::ISDIR };
                            out.push(Notification::Event { mask, slot, parent: dir, cookie, generation, name });
                        }
                    }
                }
                Change::Moved { from, from_name, to, to_name, cookie } => {
                    let echo = self.consume_echo(from, &from_name, mask::MOVED_FROM)
                        & self.consume_echo(to, &to_name, mask::MOVED_TO);
                    let events = self.sync_move(from, &from_name, to, &to_name, cookie);
                    if !echo {
                        out.extend(events);
                    }
                }
                Change::Overflow => {
                    let slots: Vec<u32> = self.watch.slots().collect();
                    for slot in slots {
                        // The guest drops its caches on OVERFLOW; the per-name events are moot.
                        self.resync_dir(slot);
                    }
                    out.push(Notification::Overflow);
                }
            }
        }
        out
    }

    /// Makes the index agree with the host about `name` in directory `dir`, whatever happened
    /// to it: created, removed, replaced, renamed in or out, or changed in place.
    ///
    /// Returns the entry's slot, generation and directory-ness after the sync, or for a removal
    /// the slot it had, so the caller can describe the event to the guest. `None` when the name
    /// is absent on both sides.
    fn sync_name(&mut self, dir: u32, name: &[u8]) -> Option<(u32, u64, bool)> {
        if check_name(name).is_err() {
            return None;
        }
        let fd = self.dir_fd(dir).ok()?;
        let on_disk = fstatat(&fd, OsStr::from_bytes(name), AtFlags::AT_SYMLINK_NOFOLLOW).ok();
        let in_index = self.index.lookup(dir, name);
        let result = match (on_disk, in_index) {
            (Some(st), Some(slot)) => {
                let same_type = self.index.live(slot).is_some_and(|e| e.mode & libc::S_IFMT == st.st_mode & libc::S_IFMT);
                // A different host file under the same name (an editor's write-then-rename,
                // seen only after the temporary name was gone) is a new file, not a changed
                // one. Keeping the entry would keep the guest's inode, and with it the window
                // chunks that still map the old file.
                let same_file = self.index.host_file(slot).is_none_or(|id| id == (st.st_dev, st.st_ino));
                if same_type && same_file {
                    let _ = self.index.set_attr(slot, attr_of_stat(&st));
                    self.describe(slot)
                } else {
                    self.remove_tree(slot);
                    self.insert_from_disk(dir, name, &st).and_then(|s| self.describe(s))
                }
            }
            (Some(st), None) => self.insert_from_disk(dir, name, &st).and_then(|s| self.describe(s)),
            (None, Some(slot)) => {
                let was = self.describe(slot);
                self.remove_tree(slot);
                was
            }
            (None, None) => None,
        };
        let _ = self.refresh(dir);
        result
    }

    /// Records that acting on a request is about to raise `mask` for `name` in `dir`.
    fn expect_echo(&mut self, dir: u32, name: &[u8], mask: u32) {
        self.echoes.push_back(Echo { dir, name: name.to_vec(), mask, at: Instant::now() });
    }

    /// Matches an arriving event against the expected echoes, dropping stale ones on the way.
    fn consume_echo(&mut self, dir: u32, name: &[u8], mask: u32) -> bool {
        let now = Instant::now();
        while self.echoes.front().is_some_and(|e| now.duration_since(e.at) > ECHO_TTL) {
            self.echoes.pop_front();
        }
        match self.echoes.iter().position(|e| e.dir == dir && e.name == name && mask & e.mask != 0) {
            Some(i) => {
                self.echoes.remove(i);
                true
            }
            None => false,
        }
    }

    fn describe(&self, slot: u32) -> Option<(u32, u64, bool)> {
        let e = self.index.live(slot)?;
        Some((slot, e.generation, e.mode & libc::S_IFMT == libc::S_IFDIR))
    }

    /// Applies a rename the host performed, keeping the entry's slot so the guest's inode
    /// survives it. Falls back to two independent syncs when the index disagrees with the
    /// event, which happens if the entry was created and moved before we looked.
    fn sync_move(&mut self, from: u32, from_name: &[u8], to: u32, to_name: &[u8], cookie: u32) -> Vec<Notification> {
        let moved = self.index.lookup(from, from_name).filter(|_| check_name(to_name).is_ok());
        let Some(slot) = moved else {
            let mut out = Vec::new();
            if let Some((slot, generation, isdir)) = self.sync_name(from, from_name) {
                out.push(Notification::Event { mask: mask::DELETE | dir_bit(isdir), slot, parent: from, cookie, generation, name: from_name.to_vec() });
            }
            if let Some((slot, generation, isdir)) = self.sync_name(to, to_name) {
                out.push(Notification::Event { mask: mask::CREATE | dir_bit(isdir), slot, parent: to, cookie, generation, name: to_name.to_vec() });
            }
            return out;
        };
        if let Some(target) = self.index.lookup(to, to_name) {
            self.remove_tree(target);
        }
        if self.index.rename(slot, to, to_name).is_err() {
            return Vec::new();
        }
        let _ = self.refresh(slot);
        let _ = self.refresh(from);
        if to != from {
            let _ = self.refresh(to);
        }
        let Some((slot, generation, isdir)) = self.describe(slot) else { return Vec::new() };
        vec![
            Notification::Event { mask: mask::MOVED_FROM | dir_bit(isdir), slot, parent: from, cookie, generation, name: from_name.to_vec() },
            Notification::Event { mask: mask::MOVED_TO | dir_bit(isdir), slot, parent: to, cookie, generation, name: to_name.to_vec() },
        ]
    }

    fn insert_from_disk(&mut self, dir: u32, name: &[u8], st: &libc::stat) -> Option<u32> {
        let Ok(slot) = self.index.insert(dir, name, attr_of_stat(st)) else {
            eprintln!("sharedfs: index full, {:?} not added", OsStr::from_bytes(name));
            return None;
        };
        if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
            if let Some(rel) = self.index.path(slot) {
                let path = self.root.join(rel);
                let _ = self.watch.add(slot, &path);
                let _ = self.scan_dir(slot, &path, &HashSet::new());
            }
        }
        Some(slot)
    }

    /// Removes `slot` and everything below it from the index and the watch set.
    fn remove_tree(&mut self, slot: u32) {
        let children: Vec<u32> = self.index.children(slot).collect();
        for c in children {
            self.remove_tree(c);
        }
        self.watch.remove(slot);
        let _ = self.index.remove(slot);
    }

    /// Re-examines every name a directory has on disk or in the index and returns an event for
    /// each name whose entry was created, removed or changed by it.
    fn resync_dir(&mut self, slot: u32) -> Vec<Notification> {
        let mut out = Vec::new();
        let Some(rel) = self.index.path(slot) else { return out };
        let mut names: Vec<Vec<u8>> =
            self.index.children(slot).filter_map(|c| self.index.live(c).map(|e| e.name().to_vec())).collect();
        if let Ok(rd) = fs::read_dir(self.root.join(rel)) {
            names.extend(rd.flatten().map(|e| e.file_name().as_bytes().to_vec()));
        }
        names.sort();
        names.dedup();
        for name in names {
            let before = self.index.lookup(slot, &name).and_then(|s| self.index.live(s).map(|e| (s, e.generation, e.size, e.mtime, e.mode, e.uid, e.gid)));
            let Some((s, generation, isdir)) = self.sync_name(slot, &name) else { continue };
            let after = self.index.live(s).filter(|e| e.generation == generation).map(|e| (s, e.generation, e.size, e.mtime, e.mode, e.uid, e.gid));
            let mask = match (before, after) {
                (None, Some(_)) => mask::CREATE,
                (Some(_), None) => mask::DELETE,
                (Some(b), Some(a)) if b.0 != a.0 || b.1 != a.1 => mask::CREATE,
                (Some(b), Some(a)) if b.2 != a.2 || b.3 != a.3 => mask::MODIFY,
                (Some(b), Some(a)) if b != a => mask::ATTRIB,
                _ => continue,
            };
            out.push(Notification::Event { mask: mask | dir_bit(isdir), slot: s, parent: slot, cookie: 0, generation, name });
        }
        out
    }

    pub fn window(&self) -> Option<&Window> {
        self.window.as_ref()
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    // --- resolution -------------------------------------------------

    /// The live slot an inode number names, or `ESTALE` if the entry it was taken from is gone.
    fn slot(&self, ino: u64) -> Result<u32, Errno> {
        let s = slot_of(ino).ok_or(libc::ENOENT)?;
        let e = self.index.live(s).ok_or(libc::ESTALE)?;
        if e.generation as u32 != generation_of(ino) {
            return Err(libc::ESTALE);
        }
        Ok(s)
    }

    /// Directory fd for a directory slot, confined beneath the root.
    fn dir_fd(&self, slot: u32) -> Result<OwnedFd, Errno> {
        if !self.index.is_dir(slot) {
            return Err(libc::ENOTDIR);
        }
        let path = self.index.path(slot).ok_or(libc::ENOENT)?;
        let how = OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS);
        openat2(&self.root_fd, &path, how).map_err(nx)
    }

    /// Parent directory fd and final component for any slot. The root
    /// resolves to itself with the name `.`.
    fn locate(&self, slot: u32) -> Result<(OwnedFd, OsString), Errno> {
        if slot == ROOT_SLOT {
            return Ok((self.dir_fd(ROOT_SLOT)?, OsString::from(".")));
        }
        let e = self.index.live(slot).ok_or(libc::ENOENT)?;
        Ok((self.dir_fd(e.parent)?, OsStr::from_bytes(e.name()).to_owned()))
    }

    /// Directory fd for a parent about to gain a child.
    fn dir_fd_for_insert(&self, parent: u32) -> Result<OwnedFd, Errno> {
        if !self.index.has_room() {
            if !self.full_reported.replace(true) {
                eprintln!("sharedfs: index full at {} entries, refusing new names", self.index.entry_cap());
            }
            return Err(libc::ENOSPC);
        }
        self.full_reported.set(false);
        self.dir_fd(parent)
    }

    fn child(&self, parent: u32, name: &[u8]) -> Result<(OwnedFd, u32), Errno> {
        check_name(name)?;
        let fd = self.dir_fd(parent)?;
        let slot = self.index.lookup(parent, name).ok_or(libc::ENOENT)?;
        Ok((fd, slot))
    }

    /// A held handle for `ino`, for writing or for reading.
    fn held(&self, ino: u64, write: bool) -> Option<OwnedFd> {
        let table = if write { &self.write_handles } else { &self.read_handles };
        table.get(&ino).and_then(|h| h.as_fd().try_clone_to_owned().ok())
    }

    /// The slot `ino` names, if that entry still exists.
    fn live_slot(&self, ino: u64) -> Option<u32> {
        self.slot(ino).ok()
    }

    /// Parent and name of `ino`'s entry, if it still exists: what inotify will report on.
    fn entry_name(&self, ino: u64) -> Option<(u32, Vec<u8>)> {
        let e = self.index.live(self.live_slot(ino)?)?;
        Some((e.parent, e.name().to_vec()))
    }

    /// Re-stat `ino`'s entry if it still exists.
    fn refresh_ino(&mut self, ino: u64) {
        if let Some(slot) = self.live_slot(ino) {
            let _ = self.refresh(slot);
        }
    }

    /// A descriptor to write `ino` through: the held handle when the guest has the file open
    /// for writing, else a fresh open by name. Returns whether it was freshly opened.
    fn write_fd(&self, ino: u64) -> Result<(OwnedFd, bool), Errno> {
        if let Some(h) = self.held(ino, true) {
            return Ok((h, false));
        }
        let (fd, name) = self.locate(self.slot(ino)?)?;
        let f = openat(&fd, name.as_os_str(), OFlag::O_WRONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC, Mode::empty()).map_err(nx)?;
        Ok((f, true))
    }

    /// The host file behind `slot`, as `(dev, ino)`.
    fn host_id(&self, slot: u32) -> Option<(u64, u64)> {
        let (fd, name) = self.locate(slot).ok()?;
        let st = fstatat(&fd, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW).ok()?;
        Some((st.st_dev, st.st_ino))
    }

    /// Refreshes every name of the host file `id`. A hard link's other names share its link
    /// count and attributes, but the index keeps one entry per name, found by the host file
    /// each was made for. (A scan of every multiply-linked entry, with a stat each, made every
    /// link cost as much as all the links before it: incremental builds hard-link thousands.)
    fn refresh_names_of(&mut self, id: (u64, u64)) {
        for slot in self.index.names_of(id).to_vec() {
            let _ = self.refresh(slot);
        }
    }

    /// Re-stat `slot` on the host and store the result.
    fn refresh(&mut self, slot: u32) -> Result<(), Errno> {
        let (fd, name) = self.locate(slot)?;
        let st = fstatat(&fd, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW).map_err(nx)?;
        self.index.set_attr(slot, attr_of_stat(&st))
    }

    // --- requests ---------------------------------------------------

    pub fn handle(&mut self, req: Request) -> Result<Response, Errno> {
        use Request as R;
        use Response as P;
        let mutating = match &req {
            R::Map { flags, .. } => flags & map::WRITE != 0,
            R::Extend { .. } | R::OpenWrite { .. } | R::Create { .. } | R::Mkdir { .. } | R::Symlink { .. } | R::Link { .. } | R::Unlink { .. }
            | R::Rmdir { .. } | R::Rename { .. } | R::Setattr { .. } => true,
            _ => false,
        };
        if self.read_only && mutating {
            return Err(libc::EROFS);
        }
        match req {
            R::Map { ino, generation, foff, len, woff, flags } => {
                if foff % crate::window::PAGE != 0 {
                    return Err(libc::EINVAL);
                }
                let writable = flags & map::WRITE != 0;
                self.window.as_ref().ok_or(libc::ENOSYS)?.check(woff, len)?;
                // The guest only maps a file it has open, so a handle is normally there and the
                // entry need not exist any more; a fresh open by name is the fallback.
                let held = if writable { self.held(ino, true) } else { self.held(ino, false).or_else(|| self.held(ino, true)) };
                let f = match held {
                    Some(f) => {
                        if fstat(&f).map_err(nx)?.st_mode & libc::S_IFMT != libc::S_IFREG {
                            return Err(libc::EINVAL);
                        }
                        f
                    }
                    None => {
                        let slot = self.slot(ino)?;
                        let e = *self.index.live(slot).ok_or(libc::ENOENT)?;
                        if e.generation != generation {
                            return Err(libc::ESTALE);
                        }
                        if e.mode & libc::S_IFMT != libc::S_IFREG {
                            return Err(libc::EINVAL);
                        }
                        let (fd, name) = self.locate(slot)?;
                        if writable {
                            // Closing a descriptor opened for writing reports CLOSE_WRITE.
                            self.expect_echo(e.parent, name.as_bytes(), mask::CLOSE_WRITE);
                        }
                        let fl = if writable { OFlag::O_RDWR } else { OFlag::O_RDONLY } | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
                        openat(&fd, name.as_os_str(), fl, Mode::empty()).map_err(nx)?
                    }
                };
                let window = self.window.as_ref().ok_or(libc::ENOSYS)?;
                window.map(woff, len, f.as_fd(), foff, writable).map_err(nx)?;
                self.maps.insert(woff, (slot_of(ino).unwrap_or(NO_SLOT), len));
                Ok(P::Empty)
            }
            R::Unmap { woff, len } => {
                let window = self.window.as_ref().ok_or(libc::ENOSYS)?;
                window.unmap(woff, len).map_err(nx)?;
                self.maps.remove(&woff);
                Ok(P::Empty)
            }
            R::Extend { ino, size } => {
                let (f, reopened) = self.write_fd(ino)?;
                let named = self.entry_name(ino);
                let st = fstat(&f).map_err(nx)?;
                if size > st.st_size as u64 {
                    ftruncate(&f, size as libc::off_t).map_err(nx)?;
                    if let Some((dir, name)) = &named {
                        self.expect_echo(*dir, name, mask::MODIFY);
                    }
                }
                if let (true, Some((dir, name))) = (reopened, &named) {
                    self.expect_echo(*dir, name, mask::CLOSE_WRITE);
                }
                drop(f);
                self.refresh_ino(ino);
                Ok(P::Empty)
            }
            R::OpenWrite { ino } => {
                if !self.write_handles.contains_key(&ino) {
                    let (fd, name) = self.locate(self.slot(ino)?)?;
                    let base = OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
                    // Read-write when the mode allows it (the window maps shared and
                    // writable, which needs it); write-only for a file only writable.
                    let f = openat(&fd, name.as_os_str(), OFlag::O_RDWR | base, Mode::empty())
                        .or_else(|_| openat(&fd, name.as_os_str(), OFlag::O_WRONLY | base, Mode::empty()))
                        .map_err(nx)?;
                    self.write_handles.insert(ino, f);
                }
                Ok(P::Empty)
            }
            R::ReleaseWrite { ino } => {
                // The entry may be gone by now (unlinked while open); the handle goes regardless.
                if let Some(f) = self.write_handles.remove(&ino) {
                    if let Some((dir, name)) = self.entry_name(ino) {
                        self.expect_echo(dir, &name, mask::CLOSE_WRITE);
                    }
                    drop(f);
                    self.refresh_ino(ino);
                }
                Ok(P::Empty)
            }
            R::OpenRead { ino } => {
                if !self.read_handles.contains_key(&ino) {
                    let (fd, name) = self.locate(self.slot(ino)?)?;
                    // A file the guest opened write-only may not be readable at all; it is
                    // then reached through its write handle.
                    if let Ok(f) = openat(&fd, name.as_os_str(), OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC, Mode::empty()) {
                        self.read_handles.insert(ino, f);
                    }
                }
                Ok(P::Empty)
            }
            R::ReleaseRead { ino } => {
                self.read_handles.remove(&ino);
                Ok(P::Empty)
            }
            R::Fsync { ino } => {
                let f = match self.held(ino, true).or_else(|| self.held(ino, false)) {
                    Some(f) => f,
                    None => {
                        let (fd, name) = self.locate(self.slot(ino)?)?;
                        openat(&fd, name.as_os_str(), OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC, Mode::empty()).map_err(nx)?
                    }
                };
                fsync(&f).map_err(nx)?;
                self.refresh_ino(ino);
                Ok(P::Empty)
            }
            R::Create { parent, mode, flags, name } => {
                check_name(&name)?;
                let p = self.slot(parent)?;
                let fd = self.dir_fd_for_insert(p)?;
                // Opened read-write as its creator, which POSIX allows whatever mode the new
                // file gets, and kept: the guest's open of the file it just created uses this
                // handle (or gives it up, if that open is read-only).
                let fl = open_flags(flags & !(libc::O_ACCMODE as u32)) | OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL;
                let f = openat(&fd, OsStr::from_bytes(&name), fl, Mode::from_bits_truncate(mode & 0o7777)).map_err(nx)?;
                self.expect_echo(p, &name, mask::CREATE);
                let node = self.node(p, &fd, &name)?;
                if let Response::Node { ino, .. } = node {
                    self.write_handles.insert(ino, f);
                }
                Ok(node)
            }
            R::Mkdir { parent, mode, name } => {
                check_name(&name)?;
                let p = self.slot(parent)?;
                let fd = self.dir_fd_for_insert(p)?;
                let n = OsStr::from_bytes(&name);
                mkdirat(&fd, n, Mode::from_bits_truncate(mode & 0o7777)).map_err(nx)?;
                self.expect_echo(p, &name, mask::CREATE);
                self.node(p, &fd, &name)
            }
            R::Symlink { parent, name, target } => {
                check_name(&name)?;
                let p = self.slot(parent)?;
                let fd = self.dir_fd_for_insert(p)?;
                symlinkat(OsStr::from_bytes(&target), &fd, OsStr::from_bytes(&name)).map_err(nx)?;
                self.expect_echo(p, &name, mask::CREATE);
                self.node(p, &fd, &name)
            }
            R::Link { ino, parent, name } => {
                check_name(&name)?;
                let src = self.slot(ino)?;
                if self.index.is_dir(src) {
                    return Err(libc::EPERM);
                }
                let p = self.slot(parent)?;
                let (src_dir, src_name) = self.locate(src)?;
                let fd = self.dir_fd_for_insert(p)?;
                linkat(&src_dir, src_name.as_os_str(), &fd, OsStr::from_bytes(&name), AtFlags::empty()).map_err(nx)?;
                self.expect_echo(p, &name, mask::CREATE);
                let node = self.node(p, &fd, &name)?;
                // The link count changed for every name of the file: the old name first, as
                // its entry does not yet know it has company.
                let _ = self.refresh(src);
                if let Some(id) = self.host_id(src) {
                    self.refresh_names_of(id);
                }
                Ok(node)
            }
            R::Unlink { parent, name } => {
                let p = self.slot(parent)?;
                let (fd, slot) = self.child(p, &name)?;
                if self.index.is_dir(slot) {
                    return Err(libc::EISDIR);
                }
                let linked = self.index.live(slot).is_some_and(|e| e.nlink > 1);
                let host_id = if linked { self.host_id(slot) } else { None };
                unlinkat(&fd, OsStr::from_bytes(&name), UnlinkatFlags::NoRemoveDir).map_err(nx)?;
                self.expect_echo(p, &name, mask::DELETE);
                self.index.remove(slot)?;
                self.refresh(p)?;
                if let Some(id) = host_id {
                    self.refresh_names_of(id);
                }
                Ok(P::Empty)
            }
            R::Rmdir { parent, name } => {
                let p = self.slot(parent)?;
                let (fd, slot) = self.child(p, &name)?;
                if !self.index.is_dir(slot) {
                    return Err(libc::ENOTDIR);
                }
                if self.index.children(slot).next().is_some() {
                    return Err(libc::ENOTEMPTY);
                }
                unlinkat(&fd, OsStr::from_bytes(&name), UnlinkatFlags::RemoveDir).map_err(nx)?;
                self.expect_echo(p, &name, mask::DELETE);
                self.watch.remove(slot);
                self.index.remove(slot)?;
                self.refresh(p)?;
                Ok(P::Empty)
            }
            R::Rename { parent, name, new_parent, new_name, flags } => {
                if flags != 0 {
                    return Err(libc::EINVAL);
                }
                check_name(&new_name)?;
                let p = self.slot(parent)?;
                let np = self.slot(new_parent)?;
                let (fd, slot) = self.child(p, &name)?;
                let nfd = self.dir_fd(np)?;
                let target = self.index.lookup(np, &new_name);
                renameat(&fd, OsStr::from_bytes(&name), &nfd, OsStr::from_bytes(&new_name)).map_err(nx)?;
                self.expect_echo(p, &name, mask::MOVED_FROM);
                self.expect_echo(np, &new_name, mask::MOVED_TO);
                if let Some(t) = target {
                    self.index.remove(t)?;
                }
                self.index.rename(slot, np, &new_name)?;
                self.refresh(slot)?;
                self.refresh(p)?;
                if np != p {
                    self.refresh(np)?;
                }
                Ok(P::Empty)
            }
            R::Setattr { ino, valid, mode, uid, gid, size, atime, mtime } => {
                let Ok(slot) = self.slot(ino) else {
                    // Unlinked but still open in the guest: fchmod, fchown, ftruncate and
                    // futimens through its handle, as on any file system.
                    let f = self.held(ino, true).or_else(|| self.held(ino, false)).ok_or(libc::ESTALE)?;
                    if valid & setattr::MODE != 0 {
                        fchmod(&f, Mode::from_bits_truncate(mode & 0o7777)).map_err(nx)?;
                    }
                    if valid & (setattr::UID | setattr::GID) != 0 {
                        let u = (valid & setattr::UID != 0).then(|| Uid::from_raw(uid));
                        let g = (valid & setattr::GID != 0).then(|| Gid::from_raw(gid));
                        fchown(&f, u, g).map_err(nx)?;
                    }
                    if valid & setattr::SIZE != 0 {
                        let w = self.held(ino, true).ok_or(libc::EBADF)?;
                        ftruncate(&w, size as libc::off_t).map_err(nx)?;
                    }
                    if valid & (setattr::ATIME | setattr::MTIME) != 0 {
                        let ts = |set: bool, ns: i64| {
                            if set { TimeSpec::new(ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000)) } else { TimeSpec::new(0, libc::UTIME_OMIT) }
                        };
                        futimens(&f, &ts(valid & setattr::ATIME != 0, atime), &ts(valid & setattr::MTIME != 0, mtime)).map_err(nx)?;
                    }
                    return Ok(P::Empty);
                };
                let (fd, name) = self.locate(slot)?;
                let n = name.as_os_str();
                // What inotify will report for these changes, per `fsnotify_change`.
                let dir = self.index.live(slot).unwrap().parent;
                if valid & (setattr::MODE | setattr::UID | setattr::GID) != 0
                    || valid & (setattr::ATIME | setattr::MTIME) == (setattr::ATIME | setattr::MTIME)
                {
                    self.expect_echo(dir, name.as_bytes(), mask::ATTRIB);
                }
                if valid & setattr::SIZE != 0 || valid & (setattr::ATIME | setattr::MTIME) == setattr::MTIME {
                    self.expect_echo(dir, name.as_bytes(), mask::MODIFY);
                }
                if valid & setattr::SIZE != 0 && !self.write_handles.contains_key(&ino) {
                    self.expect_echo(dir, name.as_bytes(), mask::CLOSE_WRITE);
                }
                if valid & setattr::MODE != 0 {
                    let f = openat(&fd, n, OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC, Mode::empty()).map_err(nx)?;
                    let via_proc = format!("/proc/self/fd/{}", f.as_raw_fd());
                    fs::set_permissions(via_proc, Permissions::from_mode(mode & 0o7777)).map_err(io_err)?;
                }
                if valid & (setattr::UID | setattr::GID) != 0 {
                    let u = (valid & setattr::UID != 0).then(|| Uid::from_raw(uid));
                    let g = (valid & setattr::GID != 0).then(|| Gid::from_raw(gid));
                    fchownat(&fd, n, u, g, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(nx)?;
                }
                if valid & setattr::SIZE != 0 {
                    // ftruncate on an open file uses its handle; a truncate by path, which
                    // the guest's VFS has permission-checked, reopens.
                    let (f, _) = self.write_fd(ino)?;
                    File::from(f).set_len(size).map_err(io_err)?;
                }
                if valid & (setattr::ATIME | setattr::MTIME) != 0 {
                    let ts = |set: bool, ns: i64| {
                        if set {
                            TimeSpec::new(ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000))
                        } else {
                            TimeSpec::new(0, libc::UTIME_OMIT)
                        }
                    };
                    utimensat(&fd, n, &ts(valid & setattr::ATIME != 0, atime), &ts(valid & setattr::MTIME != 0, mtime), UtimensatFlags::NoFollowSymlink)
                        .map_err(nx)?;
                }
                self.refresh(slot)?;
                Ok(P::Empty)
            }
            R::Readlink { ino } => {
                let slot = self.slot(ino)?;
                let e = self.index.live(slot).unwrap();
                if e.mode & libc::S_IFMT != libc::S_IFLNK {
                    return Err(libc::EINVAL);
                }
                let (fd, name) = self.locate(slot)?;
                let t = readlinkat(&fd, name.as_os_str()).map_err(nx)?;
                Ok(P::Readlink { target: t.as_bytes().to_vec() })
            }
            R::Statfs => {
                let v = fstatvfs(&self.root_fd).map_err(nx)?;
                Ok(P::Statfs {
                    bsize: v.block_size() as u64,
                    blocks: v.blocks() as u64,
                    bfree: v.blocks_free() as u64,
                    bavail: v.blocks_available() as u64,
                    files: v.files() as u64,
                    ffree: v.files_free() as u64,
                    namelen: v.name_max() as u32,
                })
            }
        }
    }

    /// Register a just-created child of `p` and refresh the parent.
    fn node(&mut self, p: u32, fd: &OwnedFd, name: &[u8]) -> Result<Response, Errno> {
        let st = fstatat(fd, OsStr::from_bytes(name), AtFlags::AT_SYMLINK_NOFOLLOW).map_err(nx)?;
        let slot = self.index.insert(p, name, attr_of_stat(&st))?;
        if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
            if let Some(rel) = self.index.path(slot) {
                let _ = self.watch.add(slot, &self.root.join(rel));
            }
        }
        self.refresh(p)?;
        let generation = self.index.live(slot).unwrap().generation;
        Ok(Response::Node { ino: ino_of(slot, generation), generation })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if !self.read_only {
            EXPORTS.lock().unwrap().remove(&self.root_id);
        }
    }
}

impl AsFd for Server {
    /// The index region, for mapping into a guest.
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.index.fd()
    }
}
