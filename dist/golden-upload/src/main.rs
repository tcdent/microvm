// Parallel S3 uploader using the \x01 key format.
// Embeds POSIX metadata in S3 keys. Symlink targets stored as object body.
//
// Usage: s3-upload <directory> <bucket> [endpoint] [threads]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use rayon::prelude::*;
use s3_rootfs::{S3Client, S3Config, KeyMeta, encode_key};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: s3-upload <directory> <bucket> [endpoint] [threads]");
        std::process::exit(1);
    }

    let dir = PathBuf::from(&args[1]);
    let bucket = &args[2];
    let endpoint = args.get(3).map(|s| s.as_str()).unwrap_or("http://localhost:9090");
    let threads: usize = args.get(4).and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8));

    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .unwrap();

    eprintln!("Scanning {}...", dir.display());
    let entries: Vec<Entry> = collect_entries(&dir);
    let total = entries.len();
    eprintln!("{} entries to upload with {} threads", total, threads);

    let uploaded = AtomicUsize::new(0);
    let bytes = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);
    let start = Instant::now();

    entries.par_iter().for_each(|entry| {
        thread_local! {
            static CLIENT: std::cell::RefCell<Option<S3Client>> = const { std::cell::RefCell::new(None) };
        }

        CLIENT.with(|cell| {
            let mut borrow = cell.borrow_mut();
            if borrow.is_none() {
                *borrow = Some(S3Client::new(S3Config {
                    endpoint: endpoint.to_string(),
                    bucket: bucket.to_string(),
                    ..S3Config::default()
                }));
            }
            let client = borrow.as_ref().unwrap();

            // Build the S3 key with embedded metadata
            let s3_key = encode_key(&entry.path, entry.meta);

            let result = client.put(&s3_key, &entry.data);

            match result {
                Ok(()) => {
                    bytes.fetch_add(entry.data.len(), Ordering::Relaxed);
                    let n = uploaded.fetch_add(1, Ordering::Relaxed) + 1;
                    if n % 500 == 0 {
                        let elapsed = start.elapsed().as_secs_f64();
                        let rate = n as f64 / elapsed;
                        eprintln!("  {}/{} ({:.0} files/sec)", n, total, rate);
                    }
                }
                Err(e) => {
                    eprintln!("  ERROR {}: {}", entry.path, e);
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    });

    let elapsed = start.elapsed();
    let n = uploaded.load(Ordering::Relaxed);
    let b = bytes.load(Ordering::Relaxed);
    let e = errors.load(Ordering::Relaxed);
    eprintln!(
        "\nDone: {} files, {:.1} MB in {:.1}s ({:.0} files/sec, {:.1} MB/s)",
        n,
        b as f64 / 1_048_576.0,
        elapsed.as_secs_f64(),
        n as f64 / elapsed.as_secs_f64(),
        b as f64 / 1_048_576.0 / elapsed.as_secs_f64(),
    );
    if e > 0 {
        eprintln!("{} errors", e);
        std::process::exit(1);
    }
}

struct Entry {
    path: String,
    meta: KeyMeta,
    data: Vec<u8>, // file contents or symlink target
}

fn collect_entries(root: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    walk(root, root, &mut entries);
    entries
}

fn walk(root: &Path, dir: &Path, entries: &mut Vec<Entry>) {
    let readdir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    for dir_entry in readdir.flatten() {
        let path = dir_entry.path();
        let rel = path.strip_prefix(root).unwrap();
        let key = rel.to_string_lossy().to_string();

        let ft = match dir_entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if ft.is_symlink() {
            let target = match std::fs::read_link(&path) {
                Ok(t) => t.to_string_lossy().to_string(),
                Err(_) => continue,
            };
            entries.push(Entry {
                path: key,
                meta: KeyMeta::new(libc::S_IFLNK as u32 | 0o777, 0, 0),
                data: target.into_bytes(), // symlink target as body
            });
        } else if ft.is_dir() {
            let m = std::fs::metadata(&path).unwrap();
            entries.push(Entry {
                path: key.clone(),
                meta: KeyMeta::new(
                    libc::S_IFDIR as u32 | (m.mode() & 0o7777),
                    m.uid(),
                    m.gid(),
                ),
                data: Vec::new(),
            });
            walk(root, &path, entries);
        } else if ft.is_file() {
            let m = std::fs::metadata(&path).unwrap();
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            entries.push(Entry {
                path: key,
                meta: KeyMeta::new(
                    libc::S_IFREG as u32 | (m.mode() & 0o7777),
                    m.uid(),
                    m.gid(),
                ),
                data,
            });
        }
    }
}
