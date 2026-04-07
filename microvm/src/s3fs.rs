// S3-backed filesystem — implements fuse-backend-rs FileSystem trait.
// Maps FUSE operations to S3 API calls via rust-s3.
//
// Each file is an S3 object. Directories are zero-byte objects with trailing '/'.
// POSIX metadata (uid, gid, mode) stored in S3 object user metadata.

use std::collections::HashMap;
use std::ffi::CStr;
use std::io;
use std::sync::Mutex;
use std::time::Duration;

use fuse_backend_rs::abi::fuse_abi::{stat64, FsOptions, OpenOptions};
use fuse_backend_rs::api::filesystem::{Context, DirEntry, Entry, FileSystem};
use s3::Bucket;
use s3::creds::Credentials;
use s3::Region;
use tokio::runtime::Runtime;

const ROOT_INODE: u64 = 1;
const TTL: Duration = Duration::from_secs(1);

struct InodeEntry {
    key: String,   // S3 key (e.g., "etc/hosts", "home/agent/")
    is_dir: bool,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
}

pub struct S3Fs {
    bucket: Bucket,
    rt: Runtime,
    inodes: Mutex<InodeTable>,
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
            next_ino: 2, // 1 is reserved for root
        };
        // Root directory
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
    pub fn new(endpoint: &str, bucket_name: &str, access_key: &str, secret_key: &str) -> Self {
        let region = Region::Custom {
            region: "us-east-1".to_string(),
            endpoint: endpoint.to_string(),
        };
        let credentials = Credentials::new(
            Some(access_key),
            Some(secret_key),
            None, None, None,
        ).expect("Failed to create S3 credentials");

        let mut bucket = Bucket::new(bucket_name, region, credentials)
            .expect("Failed to create S3 bucket handle")
            .with_path_style();
        bucket.set_request_timeout(Some(std::time::Duration::from_secs(10)));

        let rt = Runtime::new().expect("Failed to create tokio runtime");

        S3Fs {
            bucket: *bucket,
            rt,
            inodes: Mutex::new(InodeTable::new()),
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

        // Build the S3 key for this child
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

        // Try as file first (HEAD object)
        let result = self.rt.block_on(async {
            self.bucket.head_object(&child_key).await
        });

        if let Ok((head, code)) = result {
            log!("S3Fs: lookup HEAD {} -> code={}", child_key, code);
            if code == 200 {
            let size = head.content_length.unwrap_or(0) as u64;
            let mut inodes = self.inodes.lock().unwrap();
            let ino = inodes.get_or_create(&child_key, false, size);
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
        } // code == 200
        } // head_object Ok

        // Try as directory (list with prefix)
        let dir_key = format!("{}/", child_key);
        log!("S3Fs: lookup trying dir list prefix={}", dir_key);
        let result = self.rt.block_on(async {
            self.bucket.list(dir_key.clone(), Some("/".to_string())).await
        });

        if let Ok(results) = result {
            log!("S3Fs: lookup dir list results: {} pages", results.len());
            let has_contents = results.iter().any(|r| {
                !r.contents.is_empty() || !r.common_prefixes.as_ref().map_or(true, |p| p.is_empty())
            });
            // Also check if the directory marker object exists
            let dir_exists = has_contents || self.rt.block_on(async {
                self.bucket.head_object(&dir_key).await.is_ok()
            });

            if dir_exists {
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

        let result = self.rt.block_on(async {
            self.bucket.list(prefix.clone(), Some("/".to_string())).await
        });

        let results = result.map_err(|e| {
            log!("S3Fs: readdir list error: {}", e);
            io::Error::from_raw_os_error(libc::EIO)
        })?;

        let mut idx: u64 = 0;
        for page in &results {
            // Files
            for obj in &page.contents {
                let name = obj.key.strip_prefix(&prefix).unwrap_or(&obj.key);
                if name.is_empty() || name == "/" { continue; }
                let name = name.trim_end_matches('/');

                if idx >= offset {
                    let is_dir = obj.key.ends_with('/');
                    let d_type = if is_dir { libc::DT_DIR } else { libc::DT_REG };

                    let mut inodes = self.inodes.lock().unwrap();
                    let ino = inodes.get_or_create(&obj.key, is_dir, obj.size as u64);
                    drop(inodes);

                    match add_entry(DirEntry {
                        ino,
                        offset: idx + 1,
                        type_: d_type as u32,
                        name: name.as_bytes(),
                    }) {
                        Ok(0) => return Ok(()),  // buffer full
                        Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                }
                idx += 1;
            }

            // Subdirectories (common prefixes)
            if let Some(prefixes) = &page.common_prefixes {
                for prefix_entry in prefixes {
                    let name = prefix_entry.prefix
                        .strip_prefix(&prefix).unwrap_or(&prefix_entry.prefix)
                        .trim_end_matches('/');
                    if name.is_empty() { continue; }

                    if idx >= offset {
                        let mut inodes = self.inodes.lock().unwrap();
                        let ino = inodes.get_or_create(&prefix_entry.prefix, true, 0);
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
            }
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

        let result = self.rt.block_on(async {
            self.bucket.get_object_range(
                &key,
                offset,
                Some(offset + size as u64 - 1),
            ).await
        });

        match result {
            Ok(response) => {
                let data = response.bytes();
                w.write(data).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
            }
            Err(e) => {
                log!("S3Fs: read error for {}: {}", key, e);
                Err(io::Error::from_raw_os_error(libc::EIO))
            }
        }
    }

    fn open(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<(Option<Self::Handle>, fuse_backend_rs::abi::fuse_abi::OpenOptions, Option<u32>)> {
        // Stateless — no file handles needed
        Ok((Some(0), OpenOptions::empty(), None))
    }

    fn opendir(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _flags: u32,
    ) -> io::Result<(Option<Self::Handle>, fuse_backend_rs::abi::fuse_abi::OpenOptions)> {
        Ok((Some(0), OpenOptions::empty()))
    }
}
