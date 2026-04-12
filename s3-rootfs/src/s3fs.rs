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

use crate::debug;

use fuse_backend_rs::abi::fuse_abi::{stat64, statvfs64, FsOptions, OpenOptions};
use fuse_backend_rs::api::filesystem::{Context, DirEntry, Entry, FileSystem};

use crate::config::FsConfig;
use crate::inode_cache::{CachedInode, InodeCache};
use crate::keys::{self, KeyMeta};
use crate::s3::{S3Client, S3ResultExt};
use crate::storage::{self, ObjectBody};
use crate::log;

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
/// Maximum write buffer per file handle (512MB).
const WRITE_BUFFER_MAX: usize = 512 * 1024 * 1024;
// Default read cache / read-ahead sizes live in `FsConfig::default` —
// 16MB / 16MB. Benchmarks (research/07): 4MB=123MB/s, 16MB=176MB/s,
// 64MB=172MB/s, 128MB=243MB/s — 16MB is the knee.
//
// Hardlink key/body helpers and the UUID generator live in
// `crate::storage`. This file just calls them.

/// Reported filesystem size (1TB). S3 is effectively unlimited;
/// this just needs to be large enough that tools like apt don't
/// refuse to write.
const FS_TOTAL_BLOCKS: u64 = 1024 * 1024 * 1024; // 1G blocks × 4 KB = 4 TB

pub struct S3Fs {
    client: S3Client,
    cache: Mutex<InodeCache>,
    handles: Mutex<HandleTable>,
    /// Files at or below this size are fully cached on first read.
    /// Set to 0 to disable read caching.
    read_cache_max: u64,
    /// Read-ahead chunk size for files larger than read_cache_max.
    read_ahead_size: usize,
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
    /// Read-ahead buffer for large files (> read_cache_max).
    /// Holds a chunk of file data starting at read_ahead_offset.
    read_ahead: Option<Vec<u8>>,
    read_ahead_offset: u64,
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
            read_ahead: None,
            read_ahead_offset: 0,
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
    /// Construct from an [`FsConfig`].
    ///
    /// This is the only constructor. Callers who want defaults use
    /// `S3Fs::new(FsConfig::default())`; callers who want to tweak a
    /// single knob use struct-update syntax, e.g.
    /// `S3Fs::new(FsConfig { read_cache_max: 8 * MB, ..Default::default() })`.
    pub fn new(cfg: FsConfig) -> Self {
        S3Fs {
            client: S3Client::new(cfg.s3),
            cache: Mutex::new(InodeCache::with_ttl(cfg.cache_ttl)),
            handles: Mutex::new(HandleTable::new()),
            read_cache_max: cfg.read_cache_max,
            read_ahead_size: cfg.read_ahead_size,
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
        // SAFETY: stat64 is a C struct of integers + padding. Zeroing is
        // the idiomatic, portable initialization; we explicitly set every
        // field we care about below. The remaining padding/reserved fields
        // stay zero, which is the convention FUSE consumers expect.
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

    /// `name.to_str()` + `child_path` in one step. Called from every
    /// entry-creating / entry-mutating op (create, mkdir, unlink, rename,
    /// symlink, link, rmdir, mknod), which otherwise duplicate the same
    /// two-line preamble and diverge subtly in error mapping.
    fn resolve_child(&self, parent: u64, name: &CStr) -> io::Result<String> {
        let name_str = name.to_str()
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        self.child_path(parent, name_str)
    }

    /// Look up an inode and return its `(path, meta)`. ENOENT if missing.
    /// Used by readlink / read / open / release / link / rename etc., all
    /// of which would otherwise lock the cache, call `get_by_ino`, clone
    /// out the path, read `meta`, drop the guard — seven copies of the
    /// same four lines.
    fn path_meta_for_inode(&self, ino: u64) -> io::Result<(String, KeyMeta)> {
        let cache = self.cache.lock().unwrap();
        let (p, e) = cache.get_by_ino(ino)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        Ok((p.to_string(), e.meta))
    }

    /// Variant of `path_meta_for_inode` that also returns the hardlink
    /// target UUID if set. read / open / release need all three to pick
    /// between `keys::encode(path, meta)` and `storage::hardlink_key(uuid, meta)`.
    fn path_meta_hl_for_inode(&self, ino: u64) -> io::Result<(String, KeyMeta, Option<String>)> {
        let cache = self.cache.lock().unwrap();
        let (p, e) = cache.get_by_ino(ino)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        Ok((p.to_string(), e.meta, e.hardlink_target.clone()))
    }

    /// Insert `(path, meta, size)` into the cache and build the FUSE
    /// `Entry` for the caller in one shot. Collapses the `insert_from_key`
    /// + `get_by_ino(ino).unwrap()` + `entry_for` dance that appears four
    /// times (create/mkdir/symlink/link). The unwrap is safe: we just
    /// inserted `ino` under the cache lock, and the guard is still held,
    /// so nothing can have removed it.
    fn insert_and_entry(
        cache: &mut InodeCache,
        path: &str,
        meta: KeyMeta,
        size: u64,
    ) -> (u64, Entry) {
        let ino = cache.insert_from_key(path, meta, size);
        let cached = cache.get_by_ino(ino)
            .expect("just-inserted ino missing under held cache lock");
        (ino, Self::entry_for(ino, cached.1))
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

        debug!("S3 LIST prefill prefix={} max-keys={}", parent_prefix, LIST_PROBE_MAX_KEYS);
        let result = self.client.list_probe(parent_prefix, Some("/"), LIST_PROBE_MAX_KEYS)
            .io_err(&format!("list_probe prefill for {}", path))?;

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
            debug!("  lookup HIT in prefill: {} (cached {} siblings)", path, prefill_count - 1);
            return Ok(ino);
        }

        // If the LIST was not truncated, we saw the entire directory.
        // The target doesn't exist — no need for a precise probe.
        if !result.is_truncated {
            debug!("  lookup ENOENT: {} (dir complete, {} entries, negative cached)", path, prefill_count);
            cache.insert_negative(path);
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }

        debug!("  lookup MISS in prefill ({} results), falling back to precise probe", prefill_count);
        drop(cache);

        // Phase 2: precise probe — target wasn't in first 12 results (dir has >12 entries)
        let precise_prefix = keys::list_prefix(path);
        debug!("S3 LIST precise prefix={}\\x01 max-keys=1", path);
        let result = self.client.list_probe(&precise_prefix, None, 1)
            .io_err(&format!("list_probe precise for {}", path))?;

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
            debug!("  lookup ENOENT: {} (negative cached)", path);
            cache.insert_negative(path);
        } else {
            debug!("  lookup HIT in precise probe: {}", path);
        }

        found_ino.ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
    }
}

impl FileSystem for S3Fs {
    type Inode = u64;
    type Handle = u64;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        // Enable features that improve throughput:
        // - BIG_WRITES: allows max_write up to 1MB (vs ~8KB default)
        // - MAX_PAGES: allows max_pages=256 for 1MB FUSE requests
        // Only enable what the kernel offered (intersection).
        let want = FsOptions::BIG_WRITES
            | FsOptions::MAX_PAGES;
        let enabled = capable & want;
        log!("S3Fs: init capable={:#x} enabled={:#x}", capable.bits(), enabled.bits());
        Ok(enabled)
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
        let path = self.resolve_child(parent, name)?;

        // Check cache first — readdir or a previous lookup may have warmed it
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(entry) = cache.get(&path) {
                debug!("lookup CACHE HIT: {}", path);
                return Ok(Self::entry_for(entry.ino, entry));
            }
            if cache.is_negative(&path) {
                debug!("lookup NEGATIVE HIT: {}", path);
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
        }

        // Cache miss — probe S3 with LIST, pre-fill neighbors
        debug!("lookup CACHE MISS: {}", path);
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

        debug!("S3 LIST readdir prefix={} delimiter=/", list_prefix);
        let result = self.client.list(&list_prefix, Some("/"))
            .io_err(&format!("readdir list {}", list_prefix))?;
        debug!("  readdir got {} entries for {}", result.objects.len(), list_prefix);

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
                // Hide storage-layer reserved names from guest listings
                if storage::is_reserved_name(child_name) {
                    continue;
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
                    debug!("readlink CACHE HIT: {} -> {}", path, target);
                    return Ok(target.as_bytes().to_vec());
                }
            }
        }

        // Cache miss — fetch symlink target from S3 (stored as object body)
        let (path, meta) = self.path_meta_for_inode(inode)?;

        let key = keys::encode(&path, meta);
        log!("S3 GET readlink {}", path);
        let data = self.client.get(&key)
            .io_err(&format!("readlink GET {}", path))?;

        // By symlink convention the body IS the target path — parse_body
        // with S_IFLNK encodes that rule in one place. Pattern-matches
        // only Symlink because our file_type is S_IFLNK; the other
        // variants aren't reachable here but we handle them defensively
        // so future changes to parse_body can't silently break us.
        let target = match storage::parse_body(data, libc::S_IFLNK as u32) {
            ObjectBody::Symlink(t) => t,
            _ => return Err(io::Error::from_raw_os_error(libc::EIO)),
        };

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

        let (path, meta, file_size, hl_uuid) = {
            let cache = self.cache.lock().unwrap();
            let (p, e) = cache.get_by_ino(inode)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            // Not using path_meta_hl_for_inode here because we also need
            // the size, which that helper doesn't expose.
            (p.to_string(), e.meta, e.size, e.hardlink_target.clone())
        };

        // Resolve data key — follows hardlink redirect if present
        let key = match hl_uuid {
            Some(ref uuid) => storage::hardlink_key(uuid, meta),
            None => keys::encode(&path, meta),
        };

        // For files within the read cache limit, fetch whole and cache in handle
        if self.read_cache_max > 0 && file_size <= self.read_cache_max {
            log!("S3 GET read-cache {} ({} bytes, full file)", path, file_size);
            let raw = self.client.get(&key)
                .io_err(&format!("read GET {}", path))?;

            // If we already knew this was a hardlink, `key` pointed at the
            // real data and `raw` IS the file content — no body parsing
            // needed. If this was a cold read via the visible path, the
            // body might be a hardlink ref; parse it to find out.
            let data = if hl_uuid.is_some() {
                raw
            } else {
                match storage::parse_body(raw, libc::S_IFREG as u32) {
                    ObjectBody::Direct(bytes) => bytes,
                    ObjectBody::Hardlink(uuid) => {
                        // First-time hardlink detection: re-fetch from the
                        // real data object and record the UUID so subsequent
                        // reads skip this double-hop.
                        log!("  detected hardlink ref: {} -> .hardlinks/{}", path, uuid);
                        let hl_key = storage::hardlink_key(&uuid, meta);
                        let actual = self.client.get(&hl_key)
                            .io_err(&format!("read GET hardlink data for {}", path))?;
                        let mut cache = self.cache.lock().unwrap();
                        cache.set_hardlink_target(&path, uuid);
                        actual
                    }
                    // parse_body only returns Symlink when file_type=S_IFLNK;
                    // we pass S_IFREG here so this branch is unreachable.
                    ObjectBody::Symlink(_) => unreachable!("parse_body(_, S_IFREG) returned Symlink"),
                    // GoldenRef bodies aren't produced by any code path
                    // today — reserved for research/10. Treat as data
                    // unavailable if one ever turns up in the wild.
                    ObjectBody::GoldenRef(_) => {
                        log!("S3Fs: unexpected GoldenRef body at {} (feature not wired up)", path);
                        return Err(io::Error::from_raw_os_error(libc::EIO));
                    }
                }
            };

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

        // Large file — read-ahead: fetch self.read_ahead_size chunk, serve from buffer
        {
            let handles = self.handles.lock().unwrap();
            if let Some(entry) = handles.by_handle.get(&handle) {
                if let Some(ref ra) = entry.read_ahead {
                    let ra_start = entry.read_ahead_offset;
                    let ra_end = ra_start + ra.len() as u64;
                    if offset >= ra_start && offset + size as u64 <= ra_end {
                        // Cache hit — serve from read-ahead buffer
                        let buf_offset = (offset - ra_start) as usize;
                        let buf_end = buf_offset + size as usize;
                        return w.write(&ra[buf_offset..buf_end.min(ra.len())])
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e));
                    }
                }
            }
        }

        // Read-ahead miss — fetch a large chunk
        let chunk_start = offset;
        let chunk_end = std::cmp::min(chunk_start + self.read_ahead_size as u64, file_size);
        log!("S3 GET read-ahead {} offset={} size={}", path, chunk_start, chunk_end - chunk_start);
        let ra_data = self.client.get_range(&key, chunk_start, chunk_end - 1)
            .io_err(&format!("read-ahead GET {}", path))?;

        // Serve the requested slice from the fetched chunk
        let buf_end = std::cmp::min(size as usize, ra_data.len());
        let result = w.write(&ra_data[..buf_end])
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        // Stash the read-ahead buffer for subsequent reads
        let mut handles = self.handles.lock().unwrap();
        if let Some(entry) = handles.get_mut(handle) {
            entry.read_ahead = Some(ra_data);
            entry.read_ahead_offset = chunk_start;
        }

        Ok(result)
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
            // Missing-inode is tolerated here: if the cache doesn't know
            // this inode (it should, but defensive), we just skip the
            // preload. `.ok().unwrap_or_default()` preserves that legacy
            // semantics while sharing the lookup helper with other ops.
            let (path, meta, hl_uuid) = self.path_meta_hl_for_inode(inode)
                .ok()
                .unwrap_or_default();

            if !path.is_empty() {
                let key = match hl_uuid {
                    Some(ref uuid) => storage::hardlink_key(uuid, meta),
                    None => keys::encode(&path, meta),
                };
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
        let path = self.resolve_child(parent, name)?;
        let meta = KeyMeta::new(libc::S_IFREG as u32 | 0o755, 0, 0);
        let key = keys::encode(&path, meta);

        log!("S3 PUT create {}", path);
        self.client.put(&key, &[])
            .io_err(&format!("create PUT {}", path))?;

        let (ino, fuse_entry) = {
            let mut cache = self.cache.lock().unwrap();
            Self::insert_and_entry(&mut cache, &path, meta, 0)
        };

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
        if end > WRITE_BUFFER_MAX {
            return Err(io::Error::from_raw_os_error(libc::EFBIG));
        }
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
        // Snapshot path + inode under one borrow so the subsequent
        // cache.insert() doesn't need a second get_by_ino+unwrap. Cloning
        // the CachedInode is cheap (a few fields, no heavy allocs).
        let snapshot = cache.get_by_ino(inode)
            .map(|(p, c)| (p.to_string(), c.clone()));
        if let Some((path, mut cached)) = snapshot {
            cached.size = new_size;
            cache.insert(&path, cached);
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
                    // Missing-inode tolerated — if cache has lost the
                    // mapping (shouldn't happen) we silently skip the PUT.
                    let (path, meta, hl_uuid) = self.path_meta_hl_for_inode(entry.inode)
                        .ok()
                        .unwrap_or_default();

                    if !path.is_empty() {
                        // Write to hardlink data object if this is a hardlink reference
                        let key = match hl_uuid {
                            Some(ref uuid) => {
                                log!("S3 PUT release hardlink {} -> .hardlinks/{} ({} bytes)", path, uuid, data.len());
                                storage::hardlink_key(uuid, meta)
                            }
                            None => {
                                log!("S3 PUT release {} ({} bytes)", path, data.len());
                                keys::encode(&path, meta)
                            }
                        };
                        self.client.put(&key, data)
                            .io_err(&format!("release PUT {}", path))?;
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
        let path = self.resolve_child(parent, name)?;
        let meta = KeyMeta::new(libc::S_IFDIR as u32 | 0o755, 0, 0);
        let key = keys::encode(&path, meta);

        log!("S3 PUT mkdir {}", path);
        self.client.put(&key, &[])
            .io_err(&format!("mkdir PUT {}", path))?;

        let mut cache = self.cache.lock().unwrap();
        let (_, entry) = Self::insert_and_entry(&mut cache, &path, meta, 0);
        Ok(entry)
    }

    fn unlink(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<()> {
        let path = self.resolve_child(parent, name)?;

        // Need the meta to build the S3 key
        let meta = {
            let mut cache = self.cache.lock().unwrap();
            cache.get(&path).map(|e| e.meta)
        };

        if let Some(meta) = meta {
            let key = keys::encode(&path, meta);
            log!("S3 DELETE unlink {}", path);
            self.client.delete(&key)
                .io_err(&format!("unlink DELETE {}", path))?;
        }

        let mut cache = self.cache.lock().unwrap();
        cache.remove(&path);
        Ok(())
    }

    fn rename(
        &self,
        _ctx: &Context,
        olddir: Self::Inode,
        oldname: &CStr,
        newdir: Self::Inode,
        newname: &CStr,
        _flags: u32,
    ) -> io::Result<()> {
        let old_path = self.resolve_child(olddir, oldname)?;
        let new_path = self.resolve_child(newdir, newname)?;

        log!("S3 rename {} -> {}", old_path, new_path);

        // Get the old entry's metadata
        let (meta, is_dir) = {
            let mut cache = self.cache.lock().unwrap();
            match cache.get(&old_path) {
                Some(e) => (e.meta, e.meta.file_type() == libc::S_IFDIR as u32),
                None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        };

        if is_dir {
            // Directory rename: LIST all children, copy+delete each
            let old_prefix = format!("{}/", old_path);
            let result = self.client.list(&old_prefix, None)
                .io_err(&format!("rename LIST {}", old_prefix))?;

            for obj in &result.objects {
                if let Some((decoded_path, child_meta)) = keys::decode(&obj.key) {
                    let suffix = &decoded_path[old_prefix.len()..];
                    let new_child_path = format!("{}/{}", new_path, suffix);
                    let old_key = keys::encode(decoded_path, child_meta);
                    let new_key = keys::encode(&new_child_path, child_meta);

                    self.client.copy(&old_key, &new_key)
                        .io_err(&format!("rename child COPY {} -> {}", decoded_path, new_child_path))?;
                    self.client.delete(&old_key)
                        .io_err(&format!("rename child DELETE {}", decoded_path))?;

                    let mut cache = self.cache.lock().unwrap();
                    if let Some(entry) = cache.get(decoded_path).cloned() {
                        cache.remove(decoded_path);
                        cache.insert(&new_child_path, entry);
                    }
                }
            }
        }

        // Rename the entry itself — server-side copy, no data through client
        let old_key = keys::encode(&old_path, meta);
        let new_key = keys::encode(&new_path, meta);

        self.client.copy(&old_key, &new_key)
            .io_err(&format!("rename COPY {} -> {}", old_path, new_path))?;
        self.client.delete(&old_key)
            .io_err(&format!("rename DELETE {}", old_path))?;

        // Update cache
        let mut cache = self.cache.lock().unwrap();
        if let Some(entry) = cache.get(&old_path).cloned() {
            cache.remove(&old_path);
            cache.insert(&new_path, entry);
        }

        Ok(())
    }

    fn setattr(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        attr: stat64,
        _handle: Option<Self::Handle>,
        valid: fuse_backend_rs::abi::fuse_abi::SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        // Only SIZE is honored. MODE / UID / GID changes would require
        // rewriting the S3 key (metadata is embedded in the key, not in
        // HTTP headers), which is a copy+delete pair — deferred; see
        // research/07 on the metadata-in-keys design. ATIME/MTIME/CTIME
        // are silently ignored because S3 has no equivalent concept and
        // callers (chmod, touch) don't hard-fail when they're dropped.
        // If a caller actually needs these, the ENOSYS return would be
        // the POSIX-correct answer — but FUSE mount tools today treat
        // that as a hard error on common commands (apt install, etc.),
        // so we pretend success.
        if valid.contains(fuse_backend_rs::abi::fuse_abi::SetattrValid::SIZE) {
            let new_size = attr.st_size as usize;
            log!("setattr truncate ino={} size={}", inode, new_size);

            // Update cached size. Same pattern as write() — snapshot
            // path+inode once, mutate the clone, write it back; avoids
            // a second get_by_ino().unwrap() under the same lock.
            let mut cache = self.cache.lock().unwrap();
            let snapshot = cache.get_by_ino(inode)
                .map(|(p, c)| (p.to_string(), c.clone()));
            if let Some((path, mut cached)) = snapshot {
                cached.size = new_size as u64;
                cache.insert(&path, cached);
            }
            drop(cache);

            // Only truncate handle buffers for explicit ftruncate, not O_TRUNC
            // (O_TRUNC skips the open preload, so the buffer starts empty)
            let mut handles = self.handles.lock().unwrap();
            for entry in handles.by_handle.values_mut() {
                if entry.inode == inode {
                    if let Some(ref mut data) = entry.data {
                        if !data.is_empty() {
                            data.resize(new_size, 0);
                            entry.dirty = true;
                        }
                    }
                }
            }
        }

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

    fn statfs(&self, _ctx: &Context, _inode: Self::Inode) -> io::Result<statvfs64> {
        // SAFETY: statvfs64 is a C struct of integers + padding; zeroed
        // initialization is idiomatic and every meaningful field is set
        // explicitly below.
        let mut st: statvfs64 = unsafe { std::mem::zeroed() };
        st.f_bsize = 4096;
        st.f_frsize = 4096;
        st.f_blocks = FS_TOTAL_BLOCKS;
        st.f_bfree = FS_TOTAL_BLOCKS;
        st.f_bavail = FS_TOTAL_BLOCKS;
        st.f_files = 1_000_000;
        st.f_ffree = 1_000_000;
        st.f_namemax = 255;
        Ok(st)
    }

    fn symlink(
        &self,
        _ctx: &Context,
        linkname: &CStr,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<Entry> {
        let target = linkname.to_str()
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let path = self.resolve_child(parent, name)?;
        let meta = KeyMeta::new(libc::S_IFLNK as u32 | 0o777, 0, 0);
        let key = keys::encode(&path, meta);

        log!("S3 PUT symlink {} -> {}", path, target);
        self.client.put(&key, target.as_bytes())
            .io_err(&format!("symlink PUT {}", path))?;

        let mut cache = self.cache.lock().unwrap();
        let ino = cache.insert_from_key(&path, meta, target.len() as u64);
        // Cache the symlink target on the just-inserted inode so readlink
        // doesn't need a follow-up GET. Not using `insert_and_entry`
        // because that helper doesn't know about the target field.
        cache.set_symlink_target(&path, target.to_string());
        let entry = cache.get_by_ino(ino)
            .expect("just-inserted ino missing under held cache lock")
            .1;
        Ok(Self::entry_for(ino, entry))
    }

    fn rmdir(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
    ) -> io::Result<()> {
        let path = self.resolve_child(parent, name)?;

        // Check it's a directory and get its meta for the S3 key
        let meta = {
            let mut cache = self.cache.lock().unwrap();
            match cache.get(&path) {
                Some(e) if e.meta.file_type() == libc::S_IFDIR as u32 => e.meta,
                Some(_) => return Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
                None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        };

        // Verify directory is empty — LIST with max-keys=1
        let prefix = format!("{}/", path);
        let result = self.client.list_probe(&prefix, None, 1)
            .io_err(&format!("rmdir empty-check {}", prefix))?;
        if !result.objects.is_empty() {
            return Err(io::Error::from_raw_os_error(libc::ENOTEMPTY));
        }

        let key = keys::encode(&path, meta);
        log!("S3 DELETE rmdir {}", path);
        self.client.delete(&key)
            .io_err(&format!("rmdir DELETE {}", path))?;

        let mut cache = self.cache.lock().unwrap();
        cache.remove(&path);
        Ok(())
    }

    fn link(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        newparent: Self::Inode,
        newname: &CStr,
    ) -> io::Result<Entry> {
        // Hardlink emulation via .hardlinks/ directory.
        // File data moves to .hardlinks/<uuid>, both paths become references.
        // TODO: hardlink cleanup — when all references to a UUID are deleted,
        // the .hardlinks/<uuid> object becomes orphaned. Need refcount or GC.
        let new_path = self.resolve_child(newparent, newname)?;

        let (old_path, meta, size, existing_uuid) = {
            let cache = self.cache.lock().unwrap();
            let (p, e) = cache.get_by_ino(inode)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            (p.to_string(), e.meta, e.size, e.hardlink_target.clone())
        };

        let uuid = if let Some(ref uuid) = existing_uuid {
            // Already a hardlink — reuse the same UUID
            uuid.clone()
        } else {
            // First hardlink — move data to .hardlinks/<uuid>
            let uuid = storage::new_hardlink_uuid();
            let old_key = keys::encode(&old_path, meta);
            let hl_key = storage::hardlink_key(&uuid, meta);

            log!("S3 hardlink: moving data {} -> .hardlinks/{}", old_path, uuid);
            self.client.copy(&old_key, &hl_key)
                .io_err(&format!("hardlink COPY {} -> .hardlinks/{}", old_path, uuid))?;

            // Overwrite old path with reference
            self.client.put(&old_key, &storage::encode_hardlink_ref(&uuid))
                .io_err(&format!("hardlink ref PUT {}", old_path))?;

            // Update cache for old path
            let mut cache = self.cache.lock().unwrap();
            cache.set_hardlink_target(&old_path, uuid.clone());
            drop(cache);

            uuid
        };

        // Create new path as reference
        let new_key = keys::encode(&new_path, meta);

        log!("S3 PUT hardlink ref {} -> .hardlinks/{}", new_path, uuid);
        self.client.put(&new_key, &storage::encode_hardlink_ref(&uuid))
            .io_err(&format!("hardlink ref PUT {}", new_path))?;

        // Cache new path with hardlink UUID. Can't use insert_and_entry
        // because we need to set the hardlink target on the inode before
        // handing the Entry back — otherwise the first read would pay an
        // extra GET to rediscover it.
        let mut cache = self.cache.lock().unwrap();
        let ino = cache.insert_from_key(&new_path, meta, size);
        cache.set_hardlink_target(&new_path, uuid);
        let entry = cache.get_by_ino(ino)
            .expect("just-inserted ino missing under held cache lock")
            .1;
        Ok(Self::entry_for(ino, entry))
    }

    fn access(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _mask: u32,
    ) -> io::Result<()> {
        // Guest runs as root, S3 doesn't enforce POSIX perms
        Ok(())
    }

    fn flush(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        handle: Self::Handle,
        _lock_owner: u64,
    ) -> io::Result<()> {
        // Flush dirty handle to S3 (same as release but keeps the handle open)
        let (path, meta, data) = {
            let handles = self.handles.lock().unwrap();
            match handles.by_handle.get(&handle) {
                Some(entry) if entry.dirty => {
                    if let Some(ref data) = entry.data {
                        let cache = self.cache.lock().unwrap();
                        match cache.get_by_ino(entry.inode) {
                            Some((p, e)) => (p.to_string(), e.meta, data.clone()),
                            None => return Ok(()),
                        }
                    } else {
                        return Ok(());
                    }
                }
                _ => return Ok(()),
            }
        };

        if !path.is_empty() {
            let key = keys::encode(&path, meta);
            log!("S3 PUT flush {} ({} bytes)", path, data.len());
            self.client.put(&key, &data)
                .io_err(&format!("flush PUT {}", path))?;
        }
        Ok(())
    }

    fn fsync(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _datasync: bool,
        handle: Self::Handle,
    ) -> io::Result<()> {
        // Same as flush — write dirty data to S3
        self.flush(_ctx, inode, handle, 0)
    }

    fn mknod(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
        mode: u32,
        _rdev: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        // Only support regular files — devices/fifos not applicable for S3
        if (mode & libc::S_IFMT as u32) != libc::S_IFREG as u32 && (mode & libc::S_IFMT as u32) != 0 {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }
        let path = self.resolve_child(parent, name)?;
        let file_mode = if (mode & libc::S_IFMT as u32) == 0 {
            libc::S_IFREG as u32 | (mode & 0o7777)
        } else {
            mode
        };
        let meta = KeyMeta::new(file_mode, 0, 0);
        let key = keys::encode(&path, meta);

        log!("S3 PUT mknod {}", path);
        self.client.put(&key, &[])
            .io_err(&format!("mknod PUT {}", path))?;

        let mut cache = self.cache.lock().unwrap();
        let (_, entry) = Self::insert_and_entry(&mut cache, &path, meta, 0);
        Ok(entry)
    }

    fn fallocate(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _handle: Self::Handle,
        _mode: u32,
        _offset: u64,
        _length: u64,
    ) -> io::Result<()> {
        // No-op — S3 doesn't support preallocation
        Ok(())
    }

    fn getxattr(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _name: &CStr,
        _size: u32,
    ) -> io::Result<fuse_backend_rs::api::filesystem::GetxattrReply> {
        // We don't support xattrs at all (unlike ext4 where specific attrs
        // may be missing). Return ENOTSUP (aka EOPNOTSUPP) — the POSIX
        // signal for "filesystem doesn't support this feature".
        //
        // Returning ENODATA here (per-attr miss) causes coreutils `ls -la`
        // to log spurious EIO when probing system.posix_acl_access/default
        // before rendering permission bits.
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }

    fn listxattr(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _size: u32,
    ) -> io::Result<fuse_backend_rs::api::filesystem::ListxattrReply> {
        // Same rationale — ENOTSUP tells callers not to expect xattrs.
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }
}
