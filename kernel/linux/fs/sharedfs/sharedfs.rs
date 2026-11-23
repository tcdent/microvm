// SPDX-License-Identifier: GPL-2.0

//! sharedfs: a file system whose metadata and data both live in host memory.
//!
//! The host keeps every entry's name, attributes and tree links in a shared memory index and is
//! the only writer of it. The guest reads that index directly for lookups, attributes and
//! directory listings. File data is read and written in place through a shared data window the
//! host maps file ranges into, so there is one copy of every byte: the host page cache.
//! Mutations and mapping requests go over a virtqueue; host-side changes come back on a second
//! one and are raised as fsnotify events in the guest.
//!
//! One kernel module binds the virtio devices and registers the file system type. Each device is
//! one export and carries a tag in its configuration space; a mount names the tag as its source.

mod data;
mod index;
mod queue;

use core::ptr::NonNull;

use kernel::{
    alloc::{KBox, KVec},
    bindings,
    device::{Bound, Core},
    fs::{self, address_space, dax, dentry, dentry::DEntry, file, file::File, inode, inode::INode, notify, sb, Kiocb},
    iov::{IovIterDest, IovIterSource},
    mm::virt::VmaNew,
    prelude::*,
    str::CString,
    sync::aref::ARef,
    sync::{Arc, Locked},
    virtio,
    virtio::virtqueue::{Virtqueue, VirtqueueInfo},
};

use index::{Entry, Index, NO_SLOT, ROOT_SLOT};
use queue::{op, setattr, Req, Resp, Session, HEADER_LEN};

/// `RENAME_*` flags the VFS may pass. The host moves plainly; `RENAME_NOREPLACE` is already
/// enforced by the VFS's exclusive lookup of the new name.
const RENAME_NOREPLACE: u32 = 1;

/// Unassigned virtio device id used while the device is under development.
const DEVICE_ID: u32 = 0x5346;
/// `f_type` reported by statfs: "SHFS".
const SUPER_MAGIC: usize = 0x5348_4653;
/// Linux `PATH_MAX`, the longest symlink target the host will hand back.
const PATH_MAX: usize = 4096;
/// Shared memory region ids the device exposes.
const SHM_INDEX: u8 = 0;
const SHM_WINDOW: u8 = 1;
/// The export tag: NUL-padded at the start of the device configuration space, as virtio-fs does.
const TAG_LEN: usize = 36;

kernel::sync::global_lock! {
    // SAFETY: Initialised in `SharedFsModule::init`, before the device driver or the file system
    // type is registered.
    unsafe(uninit) static SESSIONS: Mutex<KVec<Arc<Session>>> = KVec::new();
}

/// The bound session whose tag is `tag`.
fn session_by_tag(tag: &[u8]) -> Option<Arc<Session>> {
    SESSIONS.lock().iter().find(|s| s.tag.as_slice() == tag).cloned()
}

// ---------------------------------------------------------------------------------------------
// The virtio driver.
// ---------------------------------------------------------------------------------------------

#[pin_data]
struct SharedFsDriver {
    session: Arc<Session>,
}

/// Completions on either queue arrive here, in interrupt context.
unsafe extern "C" fn queue_done(vq: *mut bindings::virtqueue) {
    // SAFETY: The virtio core passes the queue this callback was registered for.
    let vq = unsafe { Virtqueue::from_raw(vq) };
    let Ok(dev) = vq.dev() else { return };
    // SAFETY: The queue belongs to a device bound to this driver, whose data is set before the
    // queue can complete anything.
    let driver = unsafe { dev.drvdata::<SharedFsDriver>() };
    driver.session.kick_drain();
}

/// The two queues, in the order the device numbers them.
fn vqs_info() -> [VirtqueueInfo; 2] {
    [
        VirtqueueInfo::new(c"requests", false, Some(queue_done)),
        VirtqueueInfo::new(c"notifications", false, Some(queue_done)),
    ]
}

impl virtio::Driver for SharedFsDriver {
    type IdInfo = ();
    const ID_TABLE: virtio::IdTable<Self::IdInfo> = &SHAREDFS_TABLE;

    fn probe(vdev: &virtio::Device<Core<'_>>) -> impl PinInit<Self, Error> {
        let vqs_info = vqs_info();
        try_pin_init!(Self {
            session: {
                let mut raw = [0u8; TAG_LEN];
                vdev.config_read(0, &mut raw);
                let end = raw.iter().position(|&b| b == 0).unwrap_or(TAG_LEN);
                if end == 0 {
                    pr_err!("device has no tag\n");
                    return Err(EINVAL);
                }
                let mut tag = KVec::new();
                tag.extend_from_slice(&raw[..end], GFP_KERNEL)?;
                if session_by_tag(&tag).is_some() {
                    pr_err!("tag {} already bound\n", core::str::from_utf8(&tag).unwrap_or("?"));
                    return Err(EEXIST);
                }
                let region = vdev.shm_region(SHM_INDEX).ok_or(ENODEV)?;
                // SAFETY: The region was reported by the device for this driver.
                let index = unsafe { Index::map(region.addr, region.len) }?;
                let h = index.header();
                pr_info!(
                    "{}: index {} MiB at {:#x}: {} slots ({} live), table {}\n",
                    core::str::from_utf8(&tag).unwrap_or("?"),
                    index.len() >> 20,
                    region.addr,
                    h.entry_cap,
                    h.live,
                    h.table_cap
                );
                let dax = match vdev.shm_region(SHM_WINDOW) {
                    Some(w) => {
                        // SAFETY: The window is device memory the host keeps mapped for as long
                        // as the device exists.
                        let window = unsafe { dax::Window::new(w.addr, w.len) }?;
                        pr_info!("data window {} GiB at {:#x}\n", w.len >> 30, w.addr);
                        Some(dax::Device::new(window)?)
                    }
                    None => {
                        pr_warn!("no data window: files will not be readable\n");
                        None
                    }
                };
                let session = Session::new(tag, index, dax, vdev.find_vqs(&vqs_info)?)?;
                SESSIONS.lock().push(session.clone(), GFP_KERNEL)?;
                session
            },
        })
    }

    fn init(&self, _vdev: &virtio::Device<Bound>) -> Result {
        self.session.start_notifications()?;
        pr_info!("bound\n");
        Ok(())
    }

    /// Hibernation. The mount and its inodes survive in the image; the window mappings and the
    /// queues do not, since the daemon on the other side will be a new one. It will have the same
    /// index, so inode numbers stay valid, and it reports what changed while the guest was away.
    fn freeze(_vdev: &virtio::Device<Core<'_>>, this: core::pin::Pin<&Self>) -> Result {
        this.session.drop_window();
        this.session.freeze_queues();
        pr_info!("{}: frozen\n", core::str::from_utf8(&this.session.tag).unwrap_or("?"));
        Ok(())
    }

    fn restore(vdev: &virtio::Device<Core<'_>>, this: core::pin::Pin<&Self>) -> Result {
        this.session.restore_queues(vdev.find_vqs(&vqs_info())?)?;
        // The core would mark the device ready after this returns; the reopen below needs it now.
        vdev.ready();
        this.session.reopen_files();
        pr_info!("{}: restored\n", core::str::from_utf8(&this.session.tag).unwrap_or("?"));
        Ok(())
    }

    fn remove(_vdev: &virtio::Device<Core<'_>>, this: core::pin::Pin<&Self>) {
        SESSIONS.lock().retain(|s| !Arc::ptr_eq(s, &this.session));
        pr_info!("{}: unbound\n", core::str::from_utf8(&this.session.tag).unwrap_or("?"));
    }
}

kernel::virtio_device_table!(
    SHAREDFS_TABLE,
    MODULE_SHAREDFS_TABLE,
    <SharedFsDriver as virtio::Driver>::IdInfo,
    [(virtio::DeviceId::from_raw(DEVICE_ID), ())]
);

// ---------------------------------------------------------------------------------------------
// The file system.
// ---------------------------------------------------------------------------------------------

/// Per-inode state.
pub(crate) struct Node {
    pub(crate) slot: u32,
    /// The index entry's generation when the inode was created; a slot reused for another file
    /// gets a new generation, which is how a stale inode is told from a live one.
    pub(crate) generation: u64,
    session: Arc<Session>,
}

impl Drop for Node {
    fn drop(&mut self) {
        // The VFS has already dropped the inode's pages; give its window chunks back.
        self.session.release_file(ino_of(self.slot, self.generation));
    }
}

/// An inode number names an index slot and the low half of its generation, so a slot reused
/// for another file gets a new inode rather than colliding with one still cached, and the host
/// refuses a request that carries a stale number.
#[inline]
pub(crate) fn ino_of(slot: u32, generation: u64) -> u64 {
    ((generation as u32 as u64) << 32) | (u64::from(slot) + 1)
}

#[inline]
fn slot_of(ino: u64) -> Option<u32> {
    (ino as u32).checked_sub(1)
}

/// Nanoseconds since the epoch, as the host stores times.
fn nanos(t: fs::Timespec) -> i64 {
    t.sec.saturating_mul(1_000_000_000).saturating_add(i64::from(t.nsec))
}

/// Splits a host `dev_t` (glibc's 64-bit encoding) into major and minor.
fn host_dev(rdev: u64) -> (u32, u32) {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    (major as u32, minor as u32)
}

fn type_of(e: &Entry) -> Option<inode::Type> {
    let (major, minor) = host_dev(e.rdev);
    Some(match (e.mode as u16) & fs::mode::S_IFMT {
        fs::mode::S_IFDIR => inode::Type::Dir,
        fs::mode::S_IFREG => inode::Type::Reg,
        fs::mode::S_IFLNK => inode::Type::Lnk,
        fs::mode::S_IFIFO => inode::Type::Fifo,
        fs::mode::S_IFSOCK => inode::Type::Sock,
        fs::mode::S_IFCHR => inode::Type::Chr(major, minor),
        fs::mode::S_IFBLK => inode::Type::Blk(major, minor),
        _ => return None,
    })
}

fn attr_of(e: &Entry) -> inode::Attr {
    let size = i64::try_from(e.size).unwrap_or(i64::MAX);
    inode::Attr {
        mode: (e.mode & 0o7777) as u16,
        size,
        blocks: e.size.div_ceil(512),
        nlink: e.nlink,
        uid: e.uid,
        gid: e.gid,
        atime: fs::Timespec::from_nanos(e.atime),
        mtime: fs::Timespec::from_nanos(e.mtime),
        ctime: fs::Timespec::from_nanos(e.ctime),
    }
}

fn dir_type_of(e: &Entry) -> file::DirEntryType {
    type_of(e).map_or(file::DirEntryType::Unknown, Into::into)
}

/// Superblock data: the session, held for the life of the mount. Dropping it detaches the
/// superblock from the session under the session's lock, so nothing is delivered to a
/// superblock that is being torn down.
struct MountData {
    session: Arc<Session>,
}

impl Drop for MountData {
    fn drop(&mut self) {
        *self.session.mount.lock() = None;
    }
}

/// The session's handle on the mount using it: where host notifications are delivered.
///
/// # Invariants
///
/// `sb` is valid while this value sits inside `Session::mount`; `MountData::drop` takes it out
/// before the superblock is freed.
pub(crate) struct Mount {
    sb: NonNull<sb::SuperBlock<SharedFs>>,
}

// SAFETY: The pointer is only dereferenced under `Session::mount`, which keeps it valid.
unsafe impl Send for Mount {}

/// Event bits the host may send; anything else is dropped.
const KNOWN_EVENTS: u32 = notify::MODIFY
    | notify::ATTRIB
    | notify::CLOSE_WRITE
    | notify::MOVED_FROM
    | notify::MOVED_TO
    | notify::CREATE
    | notify::DELETE
    | notify::ISDIR;

impl Mount {
    pub(crate) fn sb(&self) -> &sb::SuperBlock<SharedFs> {
        // SAFETY: By the type invariants, the superblock is valid while `self` is reachable.
        unsafe { self.sb.as_ref() }
    }

    /// Notifications were lost. Forget every cached name; the next walk reads the index.
    pub(crate) fn on_overflow(&self) {
        self.sb().shrink_dcache();
    }

    /// Something happened on the host to `name` in directory `parent`, now (or until now) the
    /// entry in `slot`. Reports it to guest watchers the way the VFS would had the guest done it,
    /// and brings the affected cached inode up to date.
    ///
    /// Only inodes the guest has cached can have watchers, so an uncached parent or child means
    /// there is nobody to tell about that side of the event.
    pub(crate) fn on_event(
        &self,
        index: &Index,
        mask: u32,
        slot: u32,
        parent: u32,
        cookie: u32,
        generation: u64,
        name: &[u8],
    ) {
        let mask = mask & KNOWN_EVENTS;
        if mask & !notify::ISDIR == 0 {
            return;
        }
        let sb = self.sb();
        let dir = index.live(parent).and_then(|p| sb.cached_inode(ino_of(parent, p.generation)));
        let mut child = sb.cached_inode(ino_of(slot, generation));
        if child.is_none() && dir.is_some() && mask & (notify::DELETE | notify::MOVED_FROM) == 0 {
            // The entry is not cached but its directory is watched: instantiate it, so the event
            // can be reported the way `fsnotify_parent` reports a child's, with its name. A
            // non-dirent event reported on the directory alone would lose the name.
            child = index
                .live(slot)
                .filter(|e| e.generation == generation)
                .and_then(|e| SharedFs::iget(sb, slot, &e).ok());
        }

        if mask & (notify::CREATE | notify::MOVED_TO) != 0 {
            if let Some(dir) = &dir {
                dir.notify_dirent(mask, name, child.as_deref(), cookie);
            }
            if mask & notify::MOVED_TO != 0 {
                if let Some(child) = &child {
                    child.notify(notify::MOVE_SELF);
                }
            }
        } else if mask & (notify::DELETE | notify::MOVED_FROM) != 0 {
            if let Some(dir) = &dir {
                dir.notify_dirent(mask, name, child.as_deref(), cookie);
            }
            if mask & notify::DELETE != 0 {
                if let Some(child) = &child {
                    child.clear_nlink();
                    child.notify(notify::DELETE_SELF);
                }
            }
        } else if let Some(child) = &child {
            // MODIFY, CLOSE_WRITE or ATTRIB on an inode the guest holds. Data is shared by
            // reference, so only the attributes need refreshing; a shorter file also loses the
            // guest's mappings beyond its new end.
            if let Some(e) = index.live(slot).filter(|e| e.generation == generation) {
                let attr = attr_of(&e);
                let old_size = child.size();
                child.set_attr(&attr);
                if attr.size < old_size {
                    // The host shrank the file. Its window chunks beyond the new end must be
                    // unmapped from processes before the pages go, as on a local truncate.
                    let _ = dax::break_layout(child, attr.size, old_size);
                    child.truncate_pages(attr.size);
                }
            }
            match &dir {
                Some(dir) => dir.notify_child(mask, name, child),
                None => child.notify(mask),
            }
        }
    }
}

const DIR_IOPS: inode::Ops<SharedFs> = inode::Ops::new::<DirInodeOps>();
const FILE_IOPS: inode::Ops<SharedFs> = inode::Ops::new::<FileInodeOps>();
const LINK_IOPS: inode::Ops<SharedFs> = inode::Ops::new::<LinkInodeOps>();
const DIR_FOPS: file::Ops<SharedFs> = file::Ops::new::<DirOps>();
const FILE_FOPS: file::Ops<SharedFs> = file::Ops::new::<FileOps>();
const DENTRY_OPS: dentry::Ops<SharedFs> = dentry::Ops::new::<SharedFs>();
/// How file ranges map onto the window; `data.rs` implements it.
const IOMAP: dax::Ops<SharedFs> = dax::Ops::new::<SharedFs>();

pub(crate) struct SharedFs;

impl SharedFs {
    /// Refreshes an inode's attributes from its index entry, after an operation on it or on a
    /// directory it is in has changed them on the host.
    fn refresh(inode: &INode<Self>) {
        let session = &inode.super_block().data().session;
        let node = inode.data();
        if let Some(e) = session.index.live(node.slot).filter(|e| e.generation == node.generation) {
            inode.set_attr(&attr_of(&e));
        }
    }

    /// Sends a request that creates an entry under `parent` and returns the new entry's inode.
    fn create_entry(parent: &INode<Self>, req: &mut Req) -> Result<ARef<INode<Self>>> {
        let sb = parent.super_block();
        let session = &sb.data().session;
        let (resp, n) = session.request(req.as_bytes(), HEADER_LEN + 16)?;
        let mut body = Resp(&resp[HEADER_LEN..n]);
        let ino = body.u64()?;
        let generation = body.u64()?;
        let slot = slot_of(ino).ok_or(EIO)?;
        let e = session.index.live(slot).filter(|e| e.generation == generation).ok_or(EIO)?;
        if req.opcode() == op::CREATE {
            session.note_created(ino);
        }
        Self::refresh(parent);
        Self::iget(sb, slot, &e)
    }

    /// Sends a request with an empty response.
    fn simple(inode: &INode<Self>, req: &mut Req) -> Result {
        inode.super_block().data().session.request(req.as_bytes(), HEADER_LEN).map(|_| ())
    }
    /// Returns the inode for a live index entry, creating it on first use.
    fn iget(sb: &sb::SuperBlock<Self>, slot: u32, e: &Entry) -> Result<ARef<INode<Self>>> {
        let mut new = match sb.get_or_create_inode(ino_of(slot, e.generation))? {
            inode::Found::Existing(existing) => {
                if existing.data().generation != e.generation {
                    // The slot was reused while this inode was still cached. Its dentries fail
                    // revalidation and the inode goes away with them.
                    return Err(ESTALE);
                }
                return Ok(existing);
            }
            inode::Found::New(new) => new,
        };

        let typ = type_of(e).ok_or(EIO)?;
        match typ {
            inode::Type::Dir => {
                new.set_iops(DIR_IOPS).set_fops(DIR_FOPS);
            }
            inode::Type::Reg => {
                new.set_iops(FILE_IOPS).set_fops(FILE_FOPS).set_aops(address_space::Ops::dax()).set_dax();
            }
            inode::Type::Lnk => {
                new.set_iops(LINK_IOPS);
            }
            _ => {}
        }

        new.init(inode::Params {
            typ,
            attr: attr_of(e),
            value: Node {
                slot,
                generation: e.generation,
                session: sb.data().session.clone(),
            },
        })
    }
}

impl fs::FileSystem for SharedFs {
    type Data = KBox<MountData>;
    type INodeData = Node;
    const NAME: &'static CStr = c"sharedfs";
    const DENTRY_OPS: Option<dentry::Ops<Self>> = Some(DENTRY_OPS);

    fn fill_super(sb: &mut sb::SuperBlock<Self, sb::New>, source: Option<&CStr>) -> Result<KBox<MountData>> {
        let tag = source.ok_or(EINVAL)?;
        let session = session_by_tag(tag.to_bytes()).ok_or(ENODEV)?;
        if session.mount.lock().is_some() {
            // One mount per session for now; a second would need its own notification fan-out.
            return Err(EBUSY);
        }
        sb.set_magic(SUPER_MAGIC);
        Ok(KBox::new(MountData { session }, GFP_KERNEL)?)
    }

    fn init_root(sb: &sb::SuperBlock<Self>) -> Result<dentry::Root<Self>> {
        let session = &sb.data().session;
        let root = session.index.live(ROOT_SLOT).ok_or(ENOENT)?;
        let root = dentry::Root::try_new(Self::iget(sb, ROOT_SLOT, &root)?)?;
        // From here on host changes are delivered to this superblock; `MountData::drop` detaches it.
        *session.mount.lock() = Some(Mount { sb: NonNull::from(sb) });
        Ok(root)
    }

    fn statfs(dentry: &DEntry<Self>) -> Result<fs::Stat> {
        let session = &dentry.super_block().data().session;
        let mut req = Req::new(op::STATFS);
        let (resp, n) = session.request(req.as_bytes(), HEADER_LEN + 6 * 8 + 4)?;
        let mut r = Resp(&resp[HEADER_LEN..n]);
        let (bsize, blocks, bfree, bavail, files, ffree) =
            (r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?, r.u64()?);
        let namelen = r.u32()?;
        Ok(fs::Stat {
            magic: SUPER_MAGIC,
            namelen: namelen.try_into()?,
            bsize: bsize.try_into()?,
            files,
            ffree,
            blocks,
            bfree,
            bavail,
        })
    }
}

/// Inode operations of directories: names come and go through the host.
struct DirInodeOps;
/// Inode operations of regular files: only attributes change here, data goes through the window.
struct FileInodeOps;
/// Inode operations of symbolic links.
struct LinkInodeOps;

impl SharedFs {
    /// Attribute changes go to the host, which is the authority on them; the inode is then
    /// refreshed from the index. A shrinking file first loses the guest's mappings beyond its
    /// new end, as truncation does on any DAX file system.
    fn setattr_impl(_dentry: &DEntry<SharedFs>, inode: &Locked<&INode<SharedFs>, inode::WriteSem>, attr: &inode::SetAttr) -> Result {
        let mut valid = 0;
        let old_size = inode.size();
        let mode = attr.mode().inspect(|_| valid |= setattr::MODE).unwrap_or(0);
        let uid = attr.uid().inspect(|_| valid |= setattr::UID).unwrap_or(0);
        let gid = attr.gid().inspect(|_| valid |= setattr::GID).unwrap_or(0);
        let size = attr.size().inspect(|_| valid |= setattr::SIZE).unwrap_or(0);
        let atime = attr.atime().inspect(|_| valid |= setattr::ATIME).map_or(0, nanos);
        let mtime = attr.mtime().inspect(|_| valid |= setattr::MTIME).map_or(0, nanos);
        if valid & setattr::SIZE != 0 && size < old_size {
            dax::break_layout(inode, size, old_size)?;
        }
        let mut req = Req::new(op::SETATTR)
            .u64(inode.ino())
            .u32(valid)
            .u32(u32::from(mode))
            .u32(uid)
            .u32(gid)
            .u64(size as u64)
            .i64(atime)
            .i64(mtime);
        SharedFs::simple(inode, &mut req)?;
        let session = &inode.super_block().data().session;
        let node = inode.data();
        if session.index.live(node.slot).is_some_and(|e| e.generation == node.generation) {
            SharedFs::refresh(inode);
        } else {
            // Unlinked while open: no index entry to refresh from, so apply the change here.
            attr.apply_to(inode);
        }
        if valid & setattr::SIZE != 0 && size < old_size {
            inode.truncate_pages(size);
        }
        Ok(())
    }
}

#[vtable]
impl inode::Operations for DirInodeOps {
    type FileSystem = SharedFs;

    fn lookup(
        parent: &Locked<&INode<SharedFs>, inode::ReadSem>,
        dentry: dentry::Unhashed<'_, SharedFs>,
    ) -> Result<Option<ARef<DEntry<SharedFs>>>> {
        let session = &parent.super_block().data().session;
        match session.index.lookup(parent.data().slot, dentry.name()) {
            Some((slot, e)) => {
                let inode = SharedFs::iget(parent.super_block(), slot, &e)?;
                dentry.splice_alias(Some(inode))
            }
            None => dentry.splice_alias(None),
        }
    }

    fn create(
        parent: &Locked<&INode<SharedFs>, inode::WriteSem>,
        dentry: dentry::Unhashed<'_, SharedFs>,
        mode: u16,
        _excl: bool,
    ) -> Result<ARef<INode<SharedFs>>> {
        let mut req = Req::new(op::CREATE).u64(parent.ino()).u32(u32::from(mode)).u32(0).bytes(dentry.name());
        SharedFs::create_entry(parent, &mut req)
    }

    fn mkdir(
        parent: &Locked<&INode<SharedFs>, inode::WriteSem>,
        dentry: dentry::Unhashed<'_, SharedFs>,
        mode: u16,
    ) -> Result<ARef<INode<SharedFs>>> {
        let mut req = Req::new(op::MKDIR).u64(parent.ino()).u32(u32::from(mode)).bytes(dentry.name());
        SharedFs::create_entry(parent, &mut req)
    }

    fn symlink(
        parent: &Locked<&INode<SharedFs>, inode::WriteSem>,
        dentry: dentry::Unhashed<'_, SharedFs>,
        target: &CStr,
    ) -> Result<ARef<INode<SharedFs>>> {
        if target.to_bytes().len() >= PATH_MAX {
            return Err(ENAMETOOLONG);
        }
        let mut req = Req::new(op::SYMLINK).u64(parent.ino()).bytes(dentry.name()).blob(target.to_bytes());
        SharedFs::create_entry(parent, &mut req)
    }

    /// The host links the file; the new name is an entry of its own in the index, so it gets an
    /// inode of its own here too, reaching the same data. The old name's link count is
    /// refreshed.
    fn link(
        old: &DEntry<SharedFs>,
        parent: &Locked<&INode<SharedFs>, inode::WriteSem>,
        dentry: dentry::Unhashed<'_, SharedFs>,
    ) -> Result<ARef<INode<SharedFs>>> {
        let src = old.inode().ok_or(ENOENT)?;
        let mut req = Req::new(op::LINK).u64(src.ino()).u64(parent.ino()).bytes(dentry.name());
        let inode = SharedFs::create_entry(parent, &mut req)?;
        SharedFs::refresh(src);
        Ok(inode)
    }

    fn unlink(parent: &Locked<&INode<SharedFs>, inode::WriteSem>, dentry: &DEntry<SharedFs>) -> Result {
        let mut req = Req::new(op::UNLINK).u64(parent.ino()).bytes(dentry.name());
        SharedFs::simple(parent, &mut req)?;
        if let Some(victim) = dentry.inode() {
            victim.clear_nlink();
        }
        SharedFs::refresh(parent);
        Ok(())
    }

    fn rmdir(parent: &Locked<&INode<SharedFs>, inode::WriteSem>, dentry: &DEntry<SharedFs>) -> Result {
        let mut req = Req::new(op::RMDIR).u64(parent.ino()).bytes(dentry.name());
        SharedFs::simple(parent, &mut req)?;
        if let Some(victim) = dentry.inode() {
            victim.clear_nlink();
        }
        SharedFs::refresh(parent);
        Ok(())
    }

    fn rename(
        old_parent: &Locked<&INode<SharedFs>, inode::WriteSem>,
        old: &DEntry<SharedFs>,
        new_parent: &Locked<&INode<SharedFs>, inode::WriteSem>,
        new: &DEntry<SharedFs>,
        flags: u32,
    ) -> Result {
        if flags & !RENAME_NOREPLACE != 0 {
            return Err(EINVAL);
        }
        let mut req = Req::new(op::RENAME)
            .u64(old_parent.ino())
            .bytes(old.name())
            .u64(new_parent.ino())
            .bytes(new.name())
            .u32(0);
        SharedFs::simple(old_parent, &mut req)?;
        if let Some(victim) = new.inode() {
            victim.clear_nlink();
        }
        if let Some(moved) = old.inode() {
            SharedFs::refresh(moved);
        }
        SharedFs::refresh(old_parent);
        SharedFs::refresh(new_parent);
        Ok(())
    }

    fn setattr(dentry: &DEntry<SharedFs>, inode: &Locked<&INode<SharedFs>, inode::WriteSem>, attr: &inode::SetAttr) -> Result {
        SharedFs::setattr_impl(dentry, inode, attr)
    }
}

#[vtable]
impl inode::Operations for FileInodeOps {
    type FileSystem = SharedFs;

    fn setattr(dentry: &DEntry<SharedFs>, inode: &Locked<&INode<SharedFs>, inode::WriteSem>, attr: &inode::SetAttr) -> Result {
        SharedFs::setattr_impl(dentry, inode, attr)
    }
}

#[vtable]
impl inode::Operations for LinkInodeOps {
    type FileSystem = SharedFs;

    fn get_link(dentry: Option<&DEntry<SharedFs>>, inode: &INode<SharedFs>) -> Result<CString> {
        if dentry.is_none() {
            // RCU walk: asking the host sleeps, so have the VFS come back without RCU.
            return Err(ECHILD);
        }
        let session = &inode.super_block().data().session;
        let mut req = Req::new(op::READLINK).u64(inode.ino());
        let (resp, n) = session.request(req.as_bytes(), HEADER_LEN + 4 + PATH_MAX)?;
        let mut body = Resp(&resp[HEADER_LEN..n]);
        let target = body.blob()?;
        let mut buf = KVec::with_capacity(target.len() + 1, GFP_KERNEL)?;
        buf.extend_from_slice(target, GFP_KERNEL)?;
        buf.push(0, GFP_KERNEL)?;
        let cstr = CStr::from_bytes_with_nul(&buf).map_err(|_| EIO)?;
        Ok(CString::try_from(cstr)?)
    }

    fn setattr(dentry: &DEntry<SharedFs>, inode: &Locked<&INode<SharedFs>, inode::WriteSem>, attr: &inode::SetAttr) -> Result {
        SharedFs::setattr_impl(dentry, inode, attr)
    }
}

#[vtable]
impl dentry::Operations for SharedFs {
    type FileSystem = Self;

    /// A dentry is good while the index still has the same entry under the same name in the
    /// same directory. A positive dentry's inode attributes are refreshed from the index on the
    /// way, so path-based stats see the host's current view.
    fn revalidate(parent: &INode<Self>, name: &[u8], dentry: &DEntry<Self>, _rcu: bool) -> Result<bool> {
        let index = &parent.super_block().data().session.index;
        let parent_slot = parent.data().slot;
        let Some(inode) = dentry.inode() else {
            return Ok(index.lookup(parent_slot, name).is_none());
        };
        let node = inode.data();
        let Some(e) = index.live(node.slot) else {
            return Ok(false);
        };
        if e.generation != node.generation || e.parent != parent_slot || e.name() != name {
            return Ok(false);
        }
        inode.set_attr(&attr_of(&e));
        Ok(true)
    }
}

/// Operations of regular files: everything goes through the window.
struct FileOps;

#[vtable]
impl file::Operations for FileOps {
    type FileSystem = SharedFs;

    /// The VFS has granted the access; the daemon takes handles that keep it, and keep the
    /// file reachable if it is unlinked while open.
    fn open(inode: &INode<SharedFs>, file: &File) -> Result {
        inode.super_block().data().session.opened(inode.ino(), file.is_readable(), file.is_writable())
    }

    fn release(inode: &INode<SharedFs>, file: &File) {
        inode.super_block().data().session.released(inode.ino(), file.is_readable(), file.is_writable());
    }

    fn seek(file: &File, offset: fs::Offset, whence: file::Whence) -> Result<fs::Offset> {
        file::generic_seek(file, offset, whence)
    }

    fn read_iter(
        _file: &File,
        inode: &INode<SharedFs>,
        iocb: &mut Kiocb<'_, ()>,
        iov: &mut IovIterDest<'_>,
    ) -> Result<usize> {
        let _locked = Locked::<_, inode::ReadSem>::lock(inode);
        dax::read(iocb, iov, &IOMAP)
    }

    /// A write past the end of the file first has the host grow it, so every byte the guest is
    /// about to store has a place in the host file before the window is touched.
    fn write_iter(
        file: &File,
        inode: &INode<SharedFs>,
        iocb: &mut Kiocb<'_, ()>,
        iov: &mut IovIterSource<'_>,
    ) -> Result<usize> {
        let _locked = Locked::<_, inode::WriteSem>::lock(inode);
        // SAFETY: Both pointers are live for the call; this is the generic VFS precheck.
        let count = unsafe { bindings::generic_write_checks(iocb.as_raw(), iov.as_raw()) };
        if count <= 0 {
            return if count == 0 { Ok(0) } else { Err(Error::from_errno(count as i32)) };
        }
        // SAFETY: The file is valid for the call.
        kernel::error::to_result(unsafe { bindings::file_remove_privs(file.as_ptr()) })?;
        let end = iocb.ki_pos() + count as fs::Offset;
        if end > inode.size() {
            let session = &inode.super_block().data().session;
            session.extend(inode.ino(), end as u64)?;
            inode.set_size(end);
        }
        // SAFETY: The file is valid for the call; this stamps mtime and ctime as any write does.
        kernel::error::to_result(unsafe { bindings::file_update_time(file.as_ptr()) })?;
        let written = dax::write(iocb, iov, &IOMAP)?;
        // SAFETY: The kiocb is live; this flushes if the file was opened with O_SYNC.
        let synced = unsafe { bindings::generic_write_sync(iocb.as_raw(), written as isize) };
        if synced < 0 {
            return Err(Error::from_errno(synced as i32));
        }
        Ok(written)
    }

    fn mmap(file: &File, _inode: &INode<SharedFs>, vma: &VmaNew) -> Result {
        dax::mmap::<SharedFs>(file, vma);
        Ok(())
    }

    fn fsync(_file: &File, inode: &INode<SharedFs>, _start: fs::Offset, _end: fs::Offset, _datasync: bool) -> Result {
        inode.super_block().data().session.fsync(inode.ino())
    }
}

/// Operations of directories: listing from the index.
struct DirOps;

/// Directory position after the last entry.
const DIR_END: fs::Offset = fs::Offset::MAX;

#[vtable]
impl file::Operations for DirOps {
    type FileSystem = SharedFs;

    fn seek(file: &File, offset: fs::Offset, whence: file::Whence) -> Result<fs::Offset> {
        file::generic_seek(file, offset, whence)
    }

    /// Positions: 0 is `.`, 1 is `..`, 2 is the first child, and `3 + slot` is the child in
    /// `slot`, so a listing resumes at the entry it stopped on even if the host has changed the
    /// directory in between.
    fn read_dir(
        _file: &File,
        inode: &Locked<&INode<SharedFs>, inode::ReadSem>,
        emitter: &mut file::DirEmitter,
    ) -> Result {
        let index = &inode.super_block().data().session.index;
        let slot = inode.data().slot;
        let dir = index.live(slot).ok_or(ENOENT)?;

        if emitter.pos() == 0 {
            if !emitter.emit(b".", inode.ino(), file::DirEntryType::Dir) {
                return Ok(());
            }
            emitter.set_pos(1);
        }
        if emitter.pos() == 1 {
            let parent = if slot == ROOT_SLOT { slot } else { dir.parent };
            let parent_gen = index.live(parent).map_or(dir.generation, |p| p.generation);
            if !emitter.emit(b"..", ino_of(parent, parent_gen), file::DirEntryType::Dir) {
                return Ok(());
            }
            emitter.set_pos(2);
        }

        let mut cur = match emitter.pos() {
            2 => dir.first_child,
            DIR_END => return Ok(()),
            p => u32::try_from(p - 3).map_err(|_| EINVAL)?,
        };
        while cur != NO_SLOT {
            let Some(e) = index.live(cur) else { break };
            if e.parent != slot {
                break;
            }
            if !emitter.emit(e.name(), ino_of(cur, e.generation), dir_type_of(&e)) {
                return Ok(());
            }
            cur = e.next_sibling;
            emitter.set_pos(if cur == NO_SLOT {
                DIR_END
            } else {
                fs::Offset::from(cur) + 3
            });
        }
        emitter.set_pos(DIR_END);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The module: one device driver, one file system type.
// ---------------------------------------------------------------------------------------------

#[pin_data]
struct SharedFsModule {
    #[pin]
    driver: kernel::driver::Registration<virtio::Adapter<SharedFsDriver>>,
    #[pin]
    fs: fs::Registration,
}

impl kernel::InPlaceModule for SharedFsModule {
    fn init(module: &'static ThisModule) -> impl PinInit<Self, Error> {
        // SAFETY: Module init runs once, before anything can touch the lock.
        unsafe { SESSIONS.init() };
        try_pin_init!(Self {
            driver <- kernel::driver::Registration::new(c"sharedfs", module),
            fs <- fs::Registration::new::<SharedFs>(module),
        })
    }
}

module! {
    type: SharedFsModule,
    name: "sharedfs",
    authors: ["slomp"],
    description: "File system with host-resident metadata and data over virtio",
    license: "GPL v2",
}
