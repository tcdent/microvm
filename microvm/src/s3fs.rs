// S3-backed filesystem — implements fuse-backend-rs FileSystem trait.
// Maps FUSE operations to S3 API calls via our s3 client module.
//
// Each file is an S3 object. Directories are zero-byte objects with trailing '/'.
// POSIX metadata (uid, gid, mode) will be stored in S3 object user metadata.
//
// Write strategy: data is buffered in memory per file handle and flushed to S3
// as a single PUT on file close (release). This means:
// - Small writes are coalesced naturally (editor save = one PUT)
// - The guest sees writes complete instantly (buffered locally)
// - Data hits S3 on close, not on every write syscall
//
// TODO: Evaluate whether we need an explicit debounce/flush timer for
// long-lived files that stay open. Current approach flushes on close only,
// which is correct for most workflows (editors, compilers, scripts). Files
// that stay open for writes indefinitely (logs, databases) would lose data
// on crash. Options:
// - Periodic flush timer (e.g., every 5s if dirty)
// - fsync/fdatasync handler that triggers immediate PUT
// - Accept the risk for now since agent workloads are short-lived writes
// For the initial implementation, flush-on-close is sufficient.

use std::collections::HashMap;
use std::ffi::CStr;
use std::io;
use std::sync::Mutex;
use std::time::Duration;

use fuse_backend_rs::abi::fuse_abi::{stat64, FsOptions, OpenOptions};
use fuse_backend_rs::api::filesystem::{Context, DirEntry, Entry, FileSystem};

use crate::s3::{S3Client, S3Config};

const ROOT_INODE: u64 = 1;
const TTL: Duration = Duration::from_secs(1);

struct InodeEntry {
    key: String,
    is_dir: bool,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
}

pub struct S3Fs {
    client: S3Client,
    inodes: Mutex<InodeTable>,
    /// Open file handles with write buffers — flushed to S3 on release
    handles: Mutex<HandleTable>,
}

struct HandleTable {
    by_handle: HashMap<u64, HandleEntry>,
    next_handle: u64,
}

struct HandleEntry {
    inode: u64,
    dirty: bool,
    data: Option<Vec<u8>>, // None = read-only, Some = buffered for write
}

impl HandleTable {
    fn new() -> Self {
        Self { by_handle: HashMap::new(), next_handle: 1 }
    }

    fn open(&mut self, inode: u64, writing: bool) -> u64 {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.by_handle.insert(handle, HandleEntry {
            inode,
            dirty: false,
            data: if writing { Some(Vec::new()) } else { None },
        });
        handle
    }

    fn get_mut(&mut self, handle: u64) -> Option<&mut HandleEntry> {
        self.by_handle.get_mut(&handle)
    }

    fn remove(&mut self, handle: u64) -> Option<HandleEntry> {
        self.by_handle.remove(&handle)
    }
}

struct InodeTable {
    by_ino: HashMap<u64, InodeEntry>,
    by_key: HashMap<String, u64>,
    next_ino: u64,
}

impl InodeTable {
    fn new() -> Self {
        let mut t = Self {
            by_ino: HashMap::new(),
            by_key: HashMap::new(),
            next_ino: 2,
        };
        t.by_ino.insert(ROOT_INODE, InodeEntry {
            key: String::new(),
            is_dir: true,
            size: 0,
            mode: libc::S_IFDIR as u32 | 0o755,
            uid: 0,
            gid: 0,
        });
        t.by_key.insert(String::new(), ROOT_INODE);
        t
    }

    fn get_or_create(&mut self, key: &str, is_dir: bool, size: u64) -> u64 {
        if let Some(&ino) = self.by_key.get(key) {
            // Update size if changed
            if let Some(entry) = self.by_ino.get_mut(&ino) {
                entry.size = size;
            }
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        let mode = if is_dir {
            libc::S_IFDIR as u32 | 0o755
        } else {
            libc::S_IFREG as u32 | 0o755
        };
        self.by_ino.insert(ino, InodeEntry {
            key: key.to_string(),
            is_dir,
            size,
            mode,
            uid: 0,
            gid: 0,
        });
        self.by_key.insert(key.to_string(), ino);
        ino
    }

    fn get(&self, ino: u64) -> Option<&InodeEntry> {
        self.by_ino.get(&ino)
    }
}

impl S3Fs {
    pub fn new(config: S3Config) -> Self {
        let client = S3Client::new(config);
        S3Fs {
            client,
            inodes: Mutex::new(InodeTable::new()),
            handles: Mutex::new(HandleTable::new()),
        }
    }

    fn stat_for(&self, ino: u64, entry: &InodeEntry) -> stat64 {
        let mut st: stat64 = unsafe { std::mem::zeroed() };
        st.st_ino = ino;
        st.st_mode = entry.mode;
        st.st_nlink = if entry.is_dir { 2 } else { 1 };
        st.st_size = entry.size as i64;
        st.st_uid = entry.uid;
        st.st_gid = entry.gid;
        st.st_blksize = 4096;
        st.st_blocks = ((entry.size + 511) / 512) as i64;
        st
    }
}

impl FileSystem for S3Fs {
    type Inode = u64;
    type Handle = u64;

    fn init(&self, _capable: FsOptions) -> io::Result<FsOptions> {
        log!("S3Fs: init");
        Ok(FsOptions::empty())
    }

    fn getattr(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Option<Self::Handle>,
    ) -> io::Result<(stat64, Duration)> {
        let inodes = self.inodes.lock().unwrap();
        match inodes.get(inode) {
            Some(entry) => Ok((self.stat_for(inode, entry), TTL)),
            None => Err(io::Error::from_raw_os_error(libc::ENOENT)),
        }
    }

    fn lookup(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<Entry> {
        let name_str = name.to_str().map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;

        let parent_key = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get(parent) {
                Some(e) => e.key.clone(),
                None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        };

        let child_key = if parent_key.is_empty() {
            name_str.to_string()
        } else {
            format!("{}{}", parent_key, name_str)
        };

        log!("S3Fs: lookup parent={} name={} key={}", parent, name_str, child_key);

        // Try as file (HEAD object)
        match self.client.head(&child_key) {
            Ok(meta) if meta.exists => {
                log!("S3Fs: lookup {} -> file, size={}", child_key, meta.content_length);
                let mut inodes = self.inodes.lock().unwrap();
                let ino = inodes.get_or_create(&child_key, false, meta.content_length);
                let entry_data = inodes.get(ino).unwrap();
                let st = self.stat_for(ino, entry_data);
                return Ok(Entry {
                    inode: ino,
                    generation: 0,
                    attr: st,
                    attr_flags: 0,
                    attr_timeout: TTL,
                    entry_timeout: TTL,
                });
            }
            _ => {}
        }

        // Try as directory (list prefix/)
        let dir_key = format!("{}/", child_key);
        log!("S3Fs: lookup trying dir prefix={}", dir_key);

        match self.client.list(&dir_key, Some("/")) {
            Ok(result) => {
                let has_contents = !result.objects.is_empty() || !result.prefixes.is_empty();
                let dir_marker_exists = self.client.head(&dir_key)
                    .map(|m| m.exists)
                    .unwrap_or(false);

                if has_contents || dir_marker_exists {
                    log!("S3Fs: lookup {} -> dir", dir_key);
                    let mut inodes = self.inodes.lock().unwrap();
                    let ino = inodes.get_or_create(&dir_key, true, 0);
                    let entry_data = inodes.get(ino).unwrap();
                    let st = self.stat_for(ino, entry_data);
                    return Ok(Entry {
                        inode: ino,
                        generation: 0,
                        attr: st,
                        attr_flags: 0,
                        attr_timeout: TTL,
                        entry_timeout: TTL,
                    });
                }
            }
            Err(e) => {
                log!("S3Fs: lookup list error: {}", e);
            }
        }

        Err(io::Error::from_raw_os_error(libc::ENOENT))
    }

    fn readdir(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Self::Handle,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let prefix = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get(inode) {
                Some(e) => e.key.clone(),
                None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        };

        let result = self.client.list(&prefix, Some("/"))
            .map_err(|e| {
                log!("S3Fs: readdir list error: {}", e);
                io::Error::from_raw_os_error(libc::EIO)
            })?;

        let mut idx: u64 = 0;

        // Files
        for obj in &result.objects {
            let name = obj.key.strip_prefix(&prefix).unwrap_or(&obj.key);
            if name.is_empty() || name == "/" { continue; }
            let name = name.trim_end_matches('/');

            if idx >= offset {
                let is_dir = obj.key.ends_with('/');
                let d_type = if is_dir { libc::DT_DIR } else { libc::DT_REG };

                let mut inodes = self.inodes.lock().unwrap();
                let ino = inodes.get_or_create(&obj.key, is_dir, obj.size);
                drop(inodes);

                match add_entry(DirEntry {
                    ino,
                    offset: idx + 1,
                    type_: d_type as u32,
                    name: name.as_bytes(),
                }) {
                    Ok(0) => return Ok(()),
                    Ok(_) => {}
                    Err(e) => return Err(e),
                }
            }
            idx += 1;
        }

        // Subdirectories (common prefixes)
        for prefix_str in &result.prefixes {
            let name = prefix_str
                .strip_prefix(&prefix).unwrap_or(prefix_str)
                .trim_end_matches('/');
            if name.is_empty() { continue; }

            if idx >= offset {
                let mut inodes = self.inodes.lock().unwrap();
                let ino = inodes.get_or_create(prefix_str, true, 0);
                drop(inodes);

                match add_entry(DirEntry {
                    ino,
                    offset: idx + 1,
                    type_: libc::DT_DIR as u32,
                    name: name.as_bytes(),
                }) {
                    Ok(0) => return Ok(()),
                    Ok(_) => {}
                    Err(e) => return Err(e),
                }
            }
            idx += 1;
        }

        Ok(())
    }

    fn read(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Self::Handle,
        w: &mut dyn fuse_backend_rs::api::filesystem::ZeroCopyWriter,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let key = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get(inode) {
                Some(e) => e.key.clone(),
                None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        };

        let data = self.client.get_range(&key, offset, offset + size as u64 - 1)
            .map_err(|e| {
                log!("S3Fs: read error for {}: {}", key, e);
                io::Error::from_raw_os_error(libc::EIO)
            })?;

        w.write(&data).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    fn open(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions, Option<u32>)> {
        let writing = (flags & libc::O_ACCMODE as u32) != libc::O_RDONLY as u32;
        let mut handles = self.handles.lock().unwrap();
        let handle = handles.open(inode, writing);

        // If opening for write, preload existing content so writes can modify it
        if writing {
            let key = {
                let inodes = self.inodes.lock().unwrap();
                inodes.get(inode).map(|e| e.key.clone())
            };
            if let Some(key) = key {
                if let Ok(data) = self.client.get(&key) {
                    if let Some(entry) = handles.get_mut(handle) {
                        entry.data = Some(data);
                    }
                }
            }
        }

        Ok((Some(handle), OpenOptions::empty(), None))
    }

    fn create(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
        _args: fuse_backend_rs::abi::fuse_abi::CreateIn,
    ) -> io::Result<(Entry, Option<Self::Handle>, OpenOptions, Option<u32>)> {
        let name_str = name.to_str().map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;

        let parent_key = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get(parent) {
                Some(e) => e.key.clone(),
                None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        };

        let child_key = if parent_key.is_empty() {
            name_str.to_string()
        } else {
            format!("{}{}", parent_key, name_str)
        };

        log!("S3Fs: create {}", child_key);

        // PUT empty object
        self.client.put(&child_key, &[])
            .map_err(|e| {
                log!("S3Fs: create PUT error: {}", e);
                io::Error::from_raw_os_error(libc::EIO)
            })?;

        let mut inodes = self.inodes.lock().unwrap();
        let ino = inodes.get_or_create(&child_key, false, 0);
        let entry_data = inodes.get(ino).unwrap();
        let st = self.stat_for(ino, entry_data);
        drop(inodes);

        let mut handles = self.handles.lock().unwrap();
        let handle = handles.open(ino, true);

        Ok((
            Entry {
                inode: ino,
                generation: 0,
                attr: st,
                attr_flags: 0,
                attr_timeout: TTL,
                entry_timeout: TTL,
            },
            Some(handle),
            OpenOptions::empty(),
            None,
        ))
    }

    fn write(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        handle: Self::Handle,
        r: &mut dyn fuse_backend_rs::api::filesystem::ZeroCopyReader,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<usize> {
        let mut buf = vec![0u8; size as usize];
        let n = r.read(&mut buf)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut handles = self.handles.lock().unwrap();
        let entry = handles.get_mut(handle)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;

        let data = entry.data.get_or_insert_with(Vec::new);

        // Extend buffer if write goes past current end
        let end = offset as usize + n;
        if end > data.len() {
            data.resize(end, 0);
        }
        data[offset as usize..end].copy_from_slice(&buf[..n]);
        entry.dirty = true;

        // Update size in inode table
        {
            let mut inodes = self.inodes.lock().unwrap();
            if let Some(inode_entry) = inodes.by_ino.get_mut(&entry.inode) {
                inode_entry.size = data.len() as u64;
            }
        }

        Ok(n)
    }

    fn release(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _flags: u32,
        handle: Self::Handle,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        let entry = {
            let mut handles = self.handles.lock().unwrap();
            handles.remove(handle)
        };

        if let Some(entry) = entry {
            if entry.dirty {
                if let Some(data) = &entry.data {
                    let key = {
                        let inodes = self.inodes.lock().unwrap();
                        inodes.get(entry.inode).map(|e| e.key.clone())
                    };
                    if let Some(key) = key {
                        log!("S3Fs: release flush {} ({} bytes)", key, data.len());
                        self.client.put(&key, data)
                            .map_err(|e| {
                                log!("S3Fs: release PUT error: {}", e);
                                io::Error::from_raw_os_error(libc::EIO)
                            })?;
                    }
                }
            }
        }
        Ok(())
    }

    fn mkdir(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
        _mode: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        let name_str = name.to_str().map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;

        let parent_key = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get(parent) {
                Some(e) => e.key.clone(),
                None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        };

        let dir_key = if parent_key.is_empty() {
            format!("{}/", name_str)
        } else {
            format!("{}{}/", parent_key, name_str)
        };

        log!("S3Fs: mkdir {}", dir_key);

        self.client.put(&dir_key, &[])
            .map_err(|e| {
                log!("S3Fs: mkdir PUT error: {}", e);
                io::Error::from_raw_os_error(libc::EIO)
            })?;

        let mut inodes = self.inodes.lock().unwrap();
        let ino = inodes.get_or_create(&dir_key, true, 0);
        let entry_data = inodes.get(ino).unwrap();
        let st = self.stat_for(ino, entry_data);

        Ok(Entry {
            inode: ino,
            generation: 0,
            attr: st,
            attr_flags: 0,
            attr_timeout: TTL,
            entry_timeout: TTL,
        })
    }

    fn unlink(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<()> {
        let name_str = name.to_str().map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;

        let parent_key = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get(parent) {
                Some(e) => e.key.clone(),
                None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        };

        let child_key = if parent_key.is_empty() {
            name_str.to_string()
        } else {
            format!("{}{}", parent_key, name_str)
        };

        log!("S3Fs: unlink {}", child_key);

        self.client.delete(&child_key)
            .map_err(|e| {
                log!("S3Fs: unlink DELETE error: {}", e);
                io::Error::from_raw_os_error(libc::EIO)
            })?;

        Ok(())
    }

    fn setattr(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _attr: stat64,
        _handle: Option<Self::Handle>,
        _valid: fuse_backend_rs::abi::fuse_abi::SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        // Stub: accept the setattr but don't persist to S3 metadata yet
        let inodes = self.inodes.lock().unwrap();
        match inodes.get(inode) {
            Some(entry) => Ok((self.stat_for(inode, entry), TTL)),
            None => Err(io::Error::from_raw_os_error(libc::ENOENT)),
        }
    }

    fn opendir(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        Ok((Some(0), OpenOptions::empty()))
    }
}
