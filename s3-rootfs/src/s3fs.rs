// S3-backed filesystem — implements fuse-backend-rs FileSystem trait.
//
// Key format: path\x01<mode_hex><uid_hex><gid_hex>
// Metadata is embedded in the key, so a single LIST tells us type + perms.
// Symlink targets stored as object body.
//
// Write strategy: data buffered per file handle, flushed to S3 on close.
// TODO: periodic flush for long-lived files, fsync support.

use std::collections::HashMap;
use std::ffi::CStr;
use std::io;
use std::sync::Mutex;
use std::time::Duration;

use fuse_backend_rs::abi::fuse_abi::{stat64, FsOptions, OpenOptions};
use fuse_backend_rs::api::filesystem::{Context, DirEntry, Entry, FileSystem};

use crate::cache::{CachedInode, InodeCache};
use crate::keys::{self, KeyMeta};
use crate::s3::{S3Client, S3Config};
use crate::log;

const ROOT_INODE: u64 = 1;

/// Cache statistics for observability and testing.
pub struct CacheStats {
    pub inode_count: usize,
    pub negative_count: usize,
    pub hits: u64,
    pub misses: u64,
    pub negative_hits: u64,
}
const TTL: Duration = Duration::from_secs(1);
/// Number of S3 keys to fetch per lookup LIST probe.
/// Includes the target entry + neighbors for cache pre-fill.
pub const LIST_PROBE_MAX_KEYS: u32 = 12;
/// Default max file size for read caching (0 = disabled).
const DEFAULT_READ_CACHE_MAX: u64 = 2 * 1024 * 1024;

pub struct S3Fs {
    client: S3Client,
    cache: Mutex<InodeCache>,
    handles: Mutex<HandleTable>,
    /// Files at or below this size are fully cached on first read.
    /// Set to 0 to disable read caching.
    read_cache_max: u64,
}

struct HandleTable {
    by_handle: HashMap<u64, HandleEntry>,
    next_handle: u64,
}

struct HandleEntry {
    inode: u64,
    dirty: bool,
    data: Option<Vec<u8>>,
    read_cached: bool,
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
            read_cached: false,
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

impl S3Fs {
    pub fn new(config: S3Config) -> Self {
        Self::with_read_cache(config, DEFAULT_READ_CACHE_MAX)
    }

    pub fn with_read_cache(config: S3Config, read_cache_max: u64) -> Self {
        S3Fs {
            client: S3Client::new(config),
            cache: Mutex::new(InodeCache::new()),
            handles: Mutex::new(HandleTable::new()),
            read_cache_max,
        }
    }

    /// Cache statistics for observability and testing.
    pub fn cache_stats(&self) -> CacheStats {
        let cache = self.cache.lock().unwrap();
        CacheStats {
            inode_count: cache.inode_count(),
            negative_count: cache.negative_count(),
            hits: cache.hits,
            misses: cache.misses,
            negative_hits: cache.negative_hits,
        }
    }

    fn stat_for(ino: u64, entry: &CachedInode) -> stat64 {
        let mut st: stat64 = unsafe { std::mem::zeroed() };
        st.st_ino = ino;
        st.st_mode = entry.meta.mode;
        st.st_nlink = if entry.meta.file_type() == libc::S_IFDIR as u32 { 2 } else { 1 };
        st.st_size = entry.size as i64;
        st.st_uid = entry.meta.uid;
        st.st_gid = entry.meta.gid;
        st.st_blksize = 4096;
        st.st_blocks = ((entry.size + 511) / 512) as i64;
        st
    }

    /// Build the Entry response for a cached inode
    fn entry_for(ino: u64, cached: &CachedInode) -> Entry {
        Entry {
            inode: ino,
            generation: 0,
            attr: Self::stat_for(ino, cached),
            attr_flags: 0,
            attr_timeout: TTL,
            entry_timeout: TTL,
        }
    }

    /// Resolve path from parent inode + child name
    fn child_path(&self, parent: u64, name: &str) -> io::Result<String> {
        let cache = self.cache.lock().unwrap();
        let (parent_path, _) = cache.get_by_ino(parent)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        if parent_path.is_empty() {
            Ok(name.to_string())
        } else {
            Ok(format!("{}/{}", parent_path, name))
        }
    }

    /// S3 key for a cached inode (path + encoded metadata)
    fn s3_key_for(&self, path: &str, meta: KeyMeta) -> String {
        keys::encode(path, meta)
    }

    /// Lookup a path via LIST probe, populate cache with results.
    ///
    /// Strategy: LIST the parent directory with max-keys=12 to pre-fill
    /// siblings. If the target isn't in the first 12 results (large dir,
    /// target sorts late), fall back to a precise single-key probe.
    fn probe_and_cache(&self, path: &str) -> io::Result<u64> {
        // Phase 1: broad LIST at parent directory level for pre-fill
        let parent_prefix = match path.rfind('/') {
            Some(pos) => &path[..=pos], // "usr/bin/" for "usr/bin/bash"
            None => "",                  // root children have no slash
        };

        log!("S3 LIST prefill prefix={} max-keys={}", parent_prefix, LIST_PROBE_MAX_KEYS);
        let result = self.client.list_probe(parent_prefix, Some("/"), LIST_PROBE_MAX_KEYS)
            .map_err(|e| {
                log!("S3Fs: list_probe error for {}: {}", path, e);
                io::Error::from_raw_os_error(libc::EIO)
            })?;

        let mut cache = self.cache.lock().unwrap();
        let mut found_ino = None;
        let prefill_count = result.objects.len();

        for obj in &result.objects {
            if let Some((decoded_path, meta)) = keys::decode(&obj.key) {
                let ino = cache.insert_from_key(decoded_path, meta, obj.size);
                if decoded_path == path {
                    found_ino = Some(ino);
                }
            }
        }

        if let Some(ino) = found_ino {
            log!("  lookup HIT in prefill: {} (cached {} siblings)", path, prefill_count - 1);
            return Ok(ino);
        }

        // If the LIST was not truncated, we saw the entire directory.
        // The target doesn't exist — no need for a precise probe.
        if !result.is_truncated {
            log!("  lookup ENOENT: {} (dir complete, {} entries, negative cached)", path, prefill_count);
            cache.insert_negative(path);
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }

        log!("  lookup MISS in prefill ({} results), falling back to precise probe", prefill_count);
        drop(cache);

        // Phase 2: precise probe — target wasn't in first 12 results (dir has >12 entries)
        let precise_prefix = keys::list_prefix(path);
        log!("S3 LIST precise prefix={}\\x01 max-keys=1", path);
        let result = self.client.list_probe(&precise_prefix, None, 1)
            .map_err(|e| {
                log!("S3Fs: precise probe error for {}: {}", path, e);
                io::Error::from_raw_os_error(libc::EIO)
            })?;

        let mut cache = self.cache.lock().unwrap();
        for obj in &result.objects {
            if let Some((decoded_path, meta)) = keys::decode(&obj.key) {
                let ino = cache.insert_from_key(decoded_path, meta, obj.size);
                if decoded_path == path {
                    found_ino = Some(ino);
                }
            }
        }

        if found_ino.is_none() {
            log!("  lookup ENOENT: {} (negative cached)", path);
            cache.insert_negative(path);
        } else {
            log!("  lookup HIT in precise probe: {}", path);
        }

        found_ino.ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
    }
}

impl FileSystem for S3Fs {
    type Inode = u64;
    type Handle = u64;

    fn init(&self, _capable: FsOptions) -> io::Result<FsOptions> {
        log!("S3Fs: init (key format v2)");
        Ok(FsOptions::empty())
    }

    fn getattr(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Option<Self::Handle>,
    ) -> io::Result<(stat64, Duration)> {
        let cache = self.cache.lock().unwrap();
        let (_, entry) = cache.get_by_ino(inode)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        Ok((Self::stat_for(inode, entry), TTL))
    }

    fn lookup(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<Entry> {
        let name_str = name.to_str()
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let path = self.child_path(parent, name_str)?;

        // Check cache first — readdir or a previous lookup may have warmed it
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(entry) = cache.get(&path) {
                log!("lookup CACHE HIT: {}", path);
                return Ok(Self::entry_for(entry.ino, entry));
            }
            if cache.is_negative(&path) {
                log!("lookup NEGATIVE HIT: {}", path);
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
        }

        // Cache miss — probe S3 with LIST, pre-fill neighbors
        log!("lookup CACHE MISS: {}", path);
        let ino = self.probe_and_cache(&path)?;

        let cache = self.cache.lock().unwrap();
        let entry = cache.get_by_ino(ino)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?
            .1;
        Ok(Self::entry_for(ino, entry))
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
        let parent_path = {
            let cache = self.cache.lock().unwrap();
            let (p, _) = cache.get_by_ino(inode)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            p.to_string()
        };

        // LIST all children: prefix = "parent_path/" (no \x01, just path prefix)
        // We use the raw path prefix + "/" to get direct children
        let list_prefix = if parent_path.is_empty() {
            String::new()
        } else {
            format!("{}/", parent_path)
        };

        log!("S3 LIST readdir prefix={} delimiter=/", list_prefix);
        let result = self.client.list(&list_prefix, Some("/"))
            .map_err(|e| {
                log!("S3Fs: readdir list error: {}", e);
                io::Error::from_raw_os_error(libc::EIO)
            })?;
        log!("  readdir got {} entries for {}", result.objects.len(), list_prefix);

        let mut idx: u64 = 0;
        let mut cache = self.cache.lock().unwrap();

        for obj in &result.objects {
            if let Some((decoded_path, meta)) = keys::decode(&obj.key) {
                // Only include direct children (one level deep)
                let child_name = decoded_path.strip_prefix(&list_prefix)
                    .unwrap_or(decoded_path);
                if child_name.is_empty() || child_name.contains('/') {
                    continue; // skip self or grandchildren
                }

                // Populate cache
                let ino = cache.insert_from_key(decoded_path, meta, obj.size);

                if idx >= offset {
                    let d_type = match meta.file_type() {
                        t if t == libc::S_IFDIR as u32 => libc::DT_DIR,
                        t if t == libc::S_IFLNK as u32 => libc::DT_LNK,
                        _ => libc::DT_REG,
                    };

                    match add_entry(DirEntry {
                        ino,
                        offset: idx + 1,
                        type_: d_type as u32,
                        name: child_name.as_bytes(),
                    }) {
                        Ok(0) => return Ok(()),
                        Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                }
                idx += 1;
            }
        }

        Ok(())
    }

    fn readlink(&self, _ctx: &Context, inode: Self::Inode) -> io::Result<Vec<u8>> {
        // Check cache for symlink target
        {
            let cache = self.cache.lock().unwrap();
            if let Some((path, entry)) = cache.get_by_ino(inode) {
                if let Some(ref target) = entry.symlink_target {
                    log!("readlink CACHE HIT: {} -> {}", path, target);
                    return Ok(target.as_bytes().to_vec());
                }
            }
        }

        // Cache miss — fetch symlink target from S3 (stored as object body)
        let (path, meta) = {
            let cache = self.cache.lock().unwrap();
            let (p, e) = cache.get_by_ino(inode)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            (p.to_string(), e.meta)
        };

        let key = keys::encode(&path, meta);
        log!("S3 GET readlink {}", path);
        let data = self.client.get(&key)
            .map_err(|e| {
                log!("S3Fs: readlink GET error for {}: {}", path, e);
                io::Error::from_raw_os_error(libc::EIO)
            })?;

        let target = String::from_utf8_lossy(&data).to_string();

        // Cache it
        {
            let mut cache = self.cache.lock().unwrap();
            cache.set_symlink_target(&path, target.clone());
        }

        Ok(target.into_bytes())
    }

    fn read(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        handle: Self::Handle,
        w: &mut dyn fuse_backend_rs::api::filesystem::ZeroCopyWriter,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        // Try serving from the handle's read cache
        {
            let handles = self.handles.lock().unwrap();
            if let Some(entry) = handles.by_handle.get(&handle) {
                if entry.read_cached {
                    if let Some(ref data) = entry.data {
                        let start = (offset as usize).min(data.len());
                        let end = (offset as usize + size as usize).min(data.len());
                        if start >= data.len() {
                            return Ok(0); // EOF
                        }
                        return w.write(&data[start..end])
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e));
                    }
                }
            }
        }

        let (path, meta, file_size) = {
            let cache = self.cache.lock().unwrap();
            let (p, e) = cache.get_by_ino(inode)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            (p.to_string(), e.meta, e.size)
        };

        let key = keys::encode(&path, meta);

        // For files within the read cache limit, fetch whole and cache in handle
        if self.read_cache_max > 0 && file_size <= self.read_cache_max {
            log!("S3 GET read-cache {} ({} bytes, full file)", path, file_size);
            let data = self.client.get(&key)
                .map_err(|e| {
                    log!("S3Fs: read error for {}: {}", path, e);
                    io::Error::from_raw_os_error(libc::EIO)
                })?;

            let start = (offset as usize).min(data.len());
            let end = (offset as usize + size as usize).min(data.len());
            let result = if start < data.len() {
                w.write(&data[start..end])
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
            } else {
                0
            };

            // Stash in handle for subsequent reads
            let mut handles = self.handles.lock().unwrap();
            if let Some(entry) = handles.get_mut(handle) {
                entry.data = Some(data);
                entry.read_cached = true;
            }

            return Ok(result);
        }

        // Large file — range read, no caching
        log!("S3 GET read {} offset={} size={}", path, offset, size);
        let data = self.client.get_range(&key, offset, offset + size as u64 - 1)
            .map_err(|e| {
                log!("S3Fs: read error for {}: {}", path, e);
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

        let truncating = (flags & libc::O_TRUNC as u32) != 0;
        if writing && !truncating {
            let (path, meta) = {
                let cache = self.cache.lock().unwrap();
                cache.get_by_ino(inode).map(|(p, e)| (p.to_string(), e.meta))
            }.unwrap_or_default();

            if !path.is_empty() {
                let key = keys::encode(&path, meta);
                log!("S3 GET open-preload {}", path);
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
        let name_str = name.to_str()
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let path = self.child_path(parent, name_str)?;
        let meta = KeyMeta::new(libc::S_IFREG as u32 | 0o755, 0, 0);
        let key = keys::encode(&path, meta);

        log!("S3 PUT create {}", path);
        self.client.put(&key, &[])
            .map_err(|e| {
                log!("S3Fs: create error: {}", e);
                io::Error::from_raw_os_error(libc::EIO)
            })?;

        let mut cache = self.cache.lock().unwrap();
        let ino = cache.insert_from_key(&path, meta, 0);
        let entry = cache.get_by_ino(ino).unwrap().1;
        let fuse_entry = Self::entry_for(ino, entry);
        drop(cache);

        let mut handles = self.handles.lock().unwrap();
        let handle = handles.open(ino, true);

        Ok((fuse_entry, Some(handle), OpenOptions::empty(), None))
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
        let mut handles = self.handles.lock().unwrap();
        let entry = handles.get_mut(handle)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;

        let data = entry.data.get_or_insert_with(Vec::new);
        let end = offset as usize + size as usize;
        if end > data.len() {
            data.resize(end, 0);
        }

        let n = r.read(&mut data[offset as usize..end])
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        entry.dirty = true;

        // Update size in cache
        let inode = entry.inode;
        let new_size = data.len() as u64;
        drop(handles);

        let mut cache = self.cache.lock().unwrap();
        if let Some((_, cached)) = cache.get_by_ino(inode) {
            let mut updated = cached.clone();
            updated.size = new_size;
            let path = cache.get_by_ino(inode).unwrap().0.to_string();
            cache.insert(&path, updated);
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
                    let (path, meta) = {
                        let cache = self.cache.lock().unwrap();
                        cache.get_by_ino(entry.inode)
                            .map(|(p, e)| (p.to_string(), e.meta))
                    }.unwrap_or_default();

                    if !path.is_empty() {
                        let key = keys::encode(&path, meta);
                        log!("S3 PUT release {} ({} bytes)", path, data.len());
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
        let name_str = name.to_str()
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let path = self.child_path(parent, name_str)?;
        let meta = KeyMeta::new(libc::S_IFDIR as u32 | 0o755, 0, 0);
        let key = keys::encode(&path, meta);

        log!("S3 PUT mkdir {}", path);
        self.client.put(&key, &[])
            .map_err(|e| {
                log!("S3Fs: mkdir error: {}", e);
                io::Error::from_raw_os_error(libc::EIO)
            })?;

        let mut cache = self.cache.lock().unwrap();
        let ino = cache.insert_from_key(&path, meta, 0);
        let entry = cache.get_by_ino(ino).unwrap().1;
        Ok(Self::entry_for(ino, entry))
    }

    fn unlink(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<()> {
        let name_str = name.to_str()
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let path = self.child_path(parent, name_str)?;

        // Need the meta to build the S3 key
        let meta = {
            let mut cache = self.cache.lock().unwrap();
            cache.get(&path).map(|e| e.meta)
        };

        if let Some(meta) = meta {
            let key = keys::encode(&path, meta);
            log!("S3 DELETE unlink {}", path);
            self.client.delete(&key)
                .map_err(|e| {
                    log!("S3Fs: unlink error: {}", e);
                    io::Error::from_raw_os_error(libc::EIO)
                })?;
        }

        let mut cache = self.cache.lock().unwrap();
        cache.remove(&path);
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
        let cache = self.cache.lock().unwrap();
        let (_, entry) = cache.get_by_ino(inode)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        Ok((Self::stat_for(inode, entry), TTL))
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
