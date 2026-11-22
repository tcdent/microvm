//! Watches the exported tree for changes made on the host, so edits from
//! an editor, a `git checkout` or anything else outside the guest reach
//! the index. One inotify watch per directory slot; the server thread
//! reads the events and applies them, which keeps the index single-writer.

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;

use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor};

/// What happened in a watched directory. The index only needs to know which
/// name in which directory to re-examine; the mask and cookie ride along so
/// the guest can be told the same thing its own inotify would have said.
pub(crate) enum Change {
    /// Re-examine `name` in directory `slot`. `mask` holds the inotify bits.
    Name { slot: u32, name: Vec<u8>, mask: u32, cookie: u32 },
    /// An entry moved within the tree; both halves of the pair arrived.
    Moved { from: u32, from_name: Vec<u8>, to: u32, to_name: Vec<u8>, cookie: u32 },
    /// The kernel dropped events; every directory needs a resync.
    Overflow,
}

pub(crate) struct Watcher {
    inotify: Inotify,
    by_wd: HashMap<WatchDescriptor, u32>,
    by_slot: HashMap<u32, WatchDescriptor>,
}

const DIR_EVENTS: AddWatchFlags = AddWatchFlags::IN_CREATE
    .union(AddWatchFlags::IN_DELETE)
    .union(AddWatchFlags::IN_MOVED_FROM)
    .union(AddWatchFlags::IN_MOVED_TO)
    .union(AddWatchFlags::IN_ATTRIB)
    .union(AddWatchFlags::IN_CLOSE_WRITE)
    .union(AddWatchFlags::IN_MODIFY)
    .union(AddWatchFlags::IN_ONLYDIR)
    .union(AddWatchFlags::IN_DONT_FOLLOW);

impl Watcher {
    pub(crate) fn new() -> nix::Result<Self> {
        Ok(Self {
            inotify: Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)?,
            by_wd: HashMap::new(),
            by_slot: HashMap::new(),
        })
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.inotify.as_fd()
    }

    /// Starts watching the directory at `path`, which the index knows as `slot`.
    pub(crate) fn add(&mut self, slot: u32, path: &Path) -> nix::Result<()> {
        let wd = self.inotify.add_watch(path, DIR_EVENTS)?;
        if let Some(old) = self.by_slot.insert(slot, wd) {
            if old != wd {
                self.by_wd.remove(&old);
            }
        }
        self.by_wd.insert(wd, slot);
        Ok(())
    }

    /// Stops watching `slot`. Idempotent; the directory may already be gone.
    pub(crate) fn remove(&mut self, slot: u32) {
        if let Some(wd) = self.by_slot.remove(&slot) {
            self.by_wd.remove(&wd);
            let _ = self.inotify.rm_watch(wd);
        }
    }

    pub(crate) fn slots(&self) -> impl Iterator<Item = u32> + '_ {
        self.by_slot.keys().copied()
    }

    /// Drains pending events into changes. Returns an empty list when nothing is queued.
    ///
    /// A move inside the tree arrives as `IN_MOVED_FROM` then `IN_MOVED_TO` with one cookie,
    /// normally in the same read. Pairs are folded into [`Change::Moved`] so the entry keeps
    /// its slot; a half without its partner in the batch is a move out of or into the tree
    /// and is reported as a removal or a creation.
    pub(crate) fn poll(&mut self) -> Vec<Change> {
        let events = match self.inotify.read_events() {
            Ok(ev) => ev,
            Err(nix::errno::Errno::EAGAIN) => return Vec::new(),
            Err(_) => return vec![Change::Overflow],
        };
        let mut out = Vec::with_capacity(events.len());
        let mut pending_from: Vec<(u32, u32, Vec<u8>)> = Vec::new();
        for ev in events {
            self.translate(ev, &mut out, &mut pending_from);
        }
        for (cookie, slot, name) in pending_from {
            out.push(Change::Name { slot, name, mask: AddWatchFlags::IN_DELETE.bits(), cookie });
        }
        out
    }

    fn translate(&mut self, ev: InotifyEvent, out: &mut Vec<Change>, pending_from: &mut Vec<(u32, u32, Vec<u8>)>) {
        use std::os::unix::ffi::OsStrExt;
        if ev.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
            out.push(Change::Overflow);
            return;
        }
        if ev.mask.contains(AddWatchFlags::IN_IGNORED) {
            if let Some(slot) = self.by_wd.remove(&ev.wd) {
                self.by_slot.remove(&slot);
            }
            return;
        }
        let Some(&slot) = self.by_wd.get(&ev.wd) else { return };
        let Some(name) = ev.name else { return };
        let name = name.as_bytes().to_vec();
        let isdir = ev.mask & AddWatchFlags::IN_ISDIR;
        if ev.mask.contains(AddWatchFlags::IN_MOVED_FROM) {
            pending_from.push((ev.cookie, slot, name));
            return;
        }
        if ev.mask.contains(AddWatchFlags::IN_MOVED_TO) {
            if let Some(i) = pending_from.iter().position(|(c, _, _)| *c == ev.cookie) {
                let (cookie, from, from_name) = pending_from.swap_remove(i);
                out.push(Change::Moved { from, from_name, to: slot, to_name: name, cookie });
            } else {
                out.push(Change::Name { slot, name, mask: (AddWatchFlags::IN_CREATE | isdir).bits(), cookie: ev.cookie });
            }
            return;
        }
        let interesting = AddWatchFlags::IN_CREATE
            | AddWatchFlags::IN_DELETE
            | AddWatchFlags::IN_ATTRIB
            | AddWatchFlags::IN_CLOSE_WRITE
            | AddWatchFlags::IN_MODIFY;
        let mask = (ev.mask & (interesting | AddWatchFlags::IN_ISDIR)).bits();
        if mask & interesting.bits() != 0 {
            out.push(Change::Name { slot, name, mask, cookie: 0 });
        }
    }
}
