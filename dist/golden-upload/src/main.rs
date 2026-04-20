// Populate a golden bucket from a chroot.
//
// G2 bootstrap tool for the two-bucket golden-FS model
// (research/12). Given a source directory (the rootfs tree produced
// by debootstrap) and a destination bucket, this binary:
//
//   1. Walks the source tree, collecting files, directories, and
//      symlinks. Paths in the manifest are root-relative with no
//      leading slash — matches the per-VM key convention so future
//      lookups line up.
//   2. For each regular file: computes BLAKE3(content) as a
//      64-hex-char string (the object key inside the golden bucket).
//      HEAD-then-PUT: if golden/<hash> already exists this run does
//      nothing for that blob. Dedup is content-addressed — two
//      paths with identical bytes share one blob object.
//   3. Directories and symlinks do not produce blobs. Their
//      metadata (mode/uid/gid) and, for symlinks, the inline target
//      string, go into the manifest directly.
//   4. All entries collected into a `GoldenManifest` (schema version
//      `GOLDEN_SCHEMA_VERSION`), bincode-serialized, uploaded as
//      `.manifest`. `load_golden` in the runtime reads this exact
//      object at orchestrator init.
//
// Usage: golden-upload <directory> <golden-bucket> [endpoint] [threads]
//
// Exits nonzero on any failure — a partial golden bucket is worse
// than none (runtime `load_golden` treats a missing blob as an
// invariant violation). If this returns cleanly the bucket is
// complete.
//
// The former direct-to-vm-<id> upload mode is gone; per-VM buckets
// start empty under the golden model.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use s3_rootfs::{GoldenManifest, GoldenManifestEntry, S3Client, S3Config};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "Usage: golden-upload <directory> <golden-bucket> [endpoint] [threads]\n\
             \n\
             Populates the golden bucket from the source directory. Emits\n\
             a bincode manifest at <bucket>/.manifest."
        );
        std::process::exit(1);
    }

    let root = PathBuf::from(&args[1]);
    let bucket = args[2].clone();
    let endpoint = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| "http://localhost:9090".to_string());
    let threads: usize = args
        .get(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8)
                .min(16)
        });

    if let Err(e) = run(&root, &bucket, &endpoint, threads) {
        eprintln!("golden-upload: {e}");
        std::process::exit(1);
    }
}

fn run(root: &Path, bucket: &str, endpoint: &str, threads: usize) -> io::Result<()> {
    // One client with a shared Agent. Everything talks to this Agent
    // — the bucket identity is per-call via put_to / exists_in.
    // Primary bucket is unused here (we always go through
    // bucket-overrides), so we just pass `bucket` as the primary.
    //
    // Credentials: honor standard AWS env vars if set. Without this,
    // S3Config::default() silently sends `minioadmin/minioadmin`
    // against whatever endpoint the caller pointed us at — which
    // 403s against any real deployment. bootstrap-golden.sh exports
    // these env vars from /etc/microd/config.ini.
    let mut cfg = S3Config {
        endpoint: endpoint.to_string(),
        bucket: bucket.to_string(),
        ..S3Config::default()
    };
    if let Ok(v) = std::env::var("AWS_ACCESS_KEY_ID") {
        cfg.access_key = v;
    }
    if let Ok(v) = std::env::var("AWS_SECRET_ACCESS_KEY") {
        cfg.secret_key = v;
    }
    let agent = Arc::new(S3Client::build_agent(&cfg));
    let client = Arc::new(S3Client::with_shared_agent(cfg, Arc::clone(&agent)));

    eprintln!("==> Scanning {}", root.display());
    let entries = collect_entries(root)?;
    eprintln!(
        "    {} entries ({} files, {} dirs, {} symlinks) — {} threads",
        entries.len(),
        entries.iter().filter(|e| matches!(e.kind, EntryKind::File)).count(),
        entries.iter().filter(|e| matches!(e.kind, EntryKind::Dir)).count(),
        entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Symlink(_)))
            .count(),
        threads,
    );

    eprintln!("==> Hashing + uploading to bucket '{bucket}'");
    let (manifest_entries, stats) = upload_entries(&entries, bucket, &client, threads)?;

    eprintln!(
        "    {} files hashed, {} new blobs uploaded ({:.1} MB), {} existing blobs skipped",
        stats.files_hashed.load(Ordering::Relaxed),
        stats.blobs_put.load(Ordering::Relaxed),
        stats.bytes_put.load(Ordering::Relaxed) as f64 / 1_048_576.0,
        stats.blobs_skipped.load(Ordering::Relaxed),
    );

    eprintln!("==> Writing .manifest");
    let manifest = GoldenManifest { entries: manifest_entries };
    let manifest_bytes = bincode::serialize(&manifest).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialize manifest: {e}"),
        )
    })?;
    client
        .put_to(bucket, ".manifest", &manifest_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("PUT .manifest: {e}")))?;
    eprintln!(
        "    manifest: {} entries, {} bytes",
        manifest.entries.len(),
        manifest_bytes.len()
    );

    let elapsed = stats.start.elapsed();
    eprintln!(
        "==> Done in {:.1}s ({:.1} MB/s effective new-blob upload)",
        elapsed.as_secs_f64(),
        stats.bytes_put.load(Ordering::Relaxed) as f64
            / 1_048_576.0
            / elapsed.as_secs_f64().max(0.001),
    );
    Ok(())
}

// ── Tree walk ─────────────────────────────────────────────────────────

enum EntryKind {
    File,
    Dir,
    Symlink(String),
}

struct Entry {
    rel_path: String, // root-relative, no leading slash
    abs_path: PathBuf,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    kind: EntryKind,
}

fn collect_entries(root: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    walk(root, root, &mut entries)?;
    Ok(entries)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> io::Result<()> {
    for dirent in std::fs::read_dir(dir)?.flatten() {
        let abs = dirent.path();
        let rel = abs
            .strip_prefix(root)
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "strip_prefix failed"))?
            .to_string_lossy()
            .into_owned();
        let ft = dirent.file_type()?;

        if ft.is_symlink() {
            let target = std::fs::read_link(&abs)?.to_string_lossy().into_owned();
            // symlink_metadata so we don't accidentally stat the target.
            let m = std::fs::symlink_metadata(&abs)?;
            out.push(Entry {
                rel_path: rel,
                abs_path: abs,
                mode: libc::S_IFLNK as u32 | 0o777,
                uid: m.uid(),
                gid: m.gid(),
                size: target.len() as u64,
                kind: EntryKind::Symlink(target),
            });
        } else if ft.is_dir() {
            let m = std::fs::metadata(&abs)?;
            out.push(Entry {
                rel_path: rel.clone(),
                abs_path: abs.clone(),
                mode: libc::S_IFDIR as u32 | (m.mode() & 0o7777),
                uid: m.uid(),
                gid: m.gid(),
                size: 0,
                kind: EntryKind::Dir,
            });
            walk(root, &abs, out)?;
        } else if ft.is_file() {
            let m = std::fs::metadata(&abs)?;
            out.push(Entry {
                rel_path: rel,
                abs_path: abs,
                mode: libc::S_IFREG as u32 | (m.mode() & 0o7777),
                uid: m.uid(),
                gid: m.gid(),
                size: m.size(),
                kind: EntryKind::File,
            });
        }
        // Ignoring sockets / fifos / device nodes — a golden rootfs
        // shouldn't contain them. If one slips in, silent skip here
        // means the entry never lands in the manifest; downstream
        // sanity-check could add a log.
    }
    Ok(())
}

// ── Parallel hash + upload ────────────────────────────────────────────

struct Stats {
    start: Instant,
    files_hashed: AtomicUsize,
    blobs_put: AtomicUsize,
    blobs_skipped: AtomicUsize,
    bytes_put: AtomicUsize,
}

fn upload_entries(
    entries: &[Entry],
    bucket: &str,
    client: &Arc<S3Client>,
    threads: usize,
) -> io::Result<(Vec<GoldenManifestEntry>, Stats)> {
    let stats = Stats {
        start: Instant::now(),
        files_hashed: AtomicUsize::new(0),
        blobs_put: AtomicUsize::new(0),
        blobs_skipped: AtomicUsize::new(0),
        bytes_put: AtomicUsize::new(0),
    };

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("golden-upload-{i}"))
        .build()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("rayon pool: {e}")))?;

    let total = entries.len();
    let last_progress = std::sync::Mutex::new(Instant::now());

    let manifest_entries: Vec<GoldenManifestEntry> = pool.install(|| {
        entries
            .par_iter()
            .enumerate()
            .map(|(idx, entry)| -> io::Result<GoldenManifestEntry> {
                let mut manifest_entry = GoldenManifestEntry {
                    path: entry.rel_path.clone(),
                    hash: None,
                    mode: entry.mode,
                    uid: entry.uid,
                    gid: entry.gid,
                    size: entry.size,
                    symlink_target: None,
                };

                match &entry.kind {
                    EntryKind::Dir => {}
                    EntryKind::Symlink(target) => {
                        manifest_entry.symlink_target = Some(target.clone());
                    }
                    EntryKind::File => {
                        let data = std::fs::read(&entry.abs_path).map_err(|e| {
                            io::Error::new(
                                e.kind(),
                                format!("read {}: {}", entry.abs_path.display(), e),
                            )
                        })?;
                        let hash = blake3::hash(&data).to_hex().to_string();
                        stats.files_hashed.fetch_add(1, Ordering::Relaxed);

                        // HEAD-then-PUT: dedup against existing blobs.
                        // Portable across S3 impls; PutIfNoneMatch on
                        // rustfs / minio is unverified territory.
                        let existed = client.exists_in(bucket, &hash).map_err(|e| {
                            io::Error::new(
                                io::ErrorKind::Other,
                                format!("HEAD {}: {}", hash, e),
                            )
                        })?;
                        if !existed {
                            client.put_to(bucket, &hash, &data).map_err(|e| {
                                io::Error::new(
                                    io::ErrorKind::Other,
                                    format!("PUT {}: {}", hash, e),
                                )
                            })?;
                            stats.blobs_put.fetch_add(1, Ordering::Relaxed);
                            stats.bytes_put.fetch_add(data.len(), Ordering::Relaxed);
                        } else {
                            stats.blobs_skipped.fetch_add(1, Ordering::Relaxed);
                        }

                        manifest_entry.hash = Some(hash);
                    }
                }

                // Progress heartbeat: every 500 entries OR every 3
                // seconds, whichever first. Mutex cost is negligible
                // at this cadence; operators want to see forward
                // motion on a 10k-file tree.
                let should_print = (idx + 1) % 500 == 0 || {
                    let mut last = last_progress.lock().unwrap();
                    if last.elapsed() > Duration::from_secs(3) {
                        *last = Instant::now();
                        true
                    } else {
                        false
                    }
                };
                if should_print {
                    eprintln!(
                        "    {}/{} entries ({} new blobs, {} MB uploaded)",
                        idx + 1,
                        total,
                        stats.blobs_put.load(Ordering::Relaxed),
                        stats.bytes_put.load(Ordering::Relaxed) / 1_048_576,
                    );
                }

                Ok(manifest_entry)
            })
            .collect::<io::Result<Vec<_>>>()
    })?;

    Ok((manifest_entries, stats))
}
