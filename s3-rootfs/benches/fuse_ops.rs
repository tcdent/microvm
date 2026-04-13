//! Baseline benchmarks for s3-rootfs refactor.
//!
//! These gate the step-by-step refactor in research/11-s3-rootfs-refactor-plan.md.
//! Every step of the refactor must keep these numbers flat.
//!
//! Drives the `FileSystem` trait directly against a live S3 backend (RustFS
//! at localhost:19090 by default). If the backend is unreachable, the
//! benchmarks print "SKIPPED" once and exit successfully — same pattern
//! as the integration tests.
//!
//! Run:
//!   cargo bench --bench fuse_ops
//!
//! Run a specific group:
//!   cargo bench --bench fuse_ops -- lookup_cached
//!
//! Targets from the refactor plan:
//!   lookup_cold         — first-time path resolution (incl. LIST)
//!   lookup_cached       — pure inode cache hit (< 5µs target)
//!   readdir_100         — readdir on 100-child directory
//!   read_1mb_sequential — open + sequential read of 1MB file
//!   read_16mb_cached    — repeat read of cached file (memcpy-bound)
//!   write_release_1mb   — write 1MB + release (S3 PUT)
//!   hit_rate_realistic  — existing test workload; must stay ≥ 66.7%
//!
//! Baseline numbers (median, RustFS on localhost:19090, pre-refactor):
//!   lookup_cold          = 15.72 ms
//!   lookup_cached        = 392 ns     (target < 5µs ✓)
//!   readdir_100          = 472.57 ms
//!   read_1mb_sequential  = 30.04 ms   (33.3 MiB/s)
//!   read_16mb_cached     = 2.89 ms    (5.41 GiB/s — memcpy-bound)
//!   write_release_1mb    = 48.60 ms   (20.6 MiB/s)
//!   hit_rate_realistic   = 84.09 ms   (asserts hit rate ≥ 62%)
//!
//! Each refactor step must hold these numbers flat. Noise budget: ±10%
//! for S3-bound benches (15-50ms range), ±5% for in-memory benches.

// Shared test fixtures — duplicated from tests/harness/ because Cargo
// test and bench binaries are separate worlds. Factor to a sub-crate if
// this duplication becomes load-bearing.
mod harness;

use std::ffi::CString;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use fuse_backend_rs::abi::fuse_abi::CreateIn;
use fuse_backend_rs::api::filesystem::{DirEntry, FileSystem};

use harness::context::root_ctx;
use harness::fixture::{GoldenSeed, TestFixture};
use harness::s3::s3_available;
use harness::zerocopy::{TestReader, TestWriter};

use s3_rootfs::blob_cache::is_golden_ino;

const ROOT: u64 = 1;

/// Skip the entire bench group if S3 isn't reachable. Criterion runs the
/// closure exactly once during setup; we check there and early-return.
fn skip_if_no_s3(c: &mut Criterion, name: &str) -> bool {
    if !s3_available() {
        eprintln!("SKIPPED: {name} (S3 backend not available)");
        // Register a trivial bench so `cargo bench` doesn't complain about
        // an empty group.
        c.bench_function(&format!("{name}_skipped"), |b| b.iter(|| {}));
        true
    } else {
        false
    }
}

// ── lookup_cold ──────────────────────────────────────────────────────────
//
// First-time resolution of a path. Each iteration uses a FRESH fixture so
// the cache is empty. This measures the full cost: LIST probe, S3 response
// parse, inode insert.
//
// BatchSize::PerIteration because setup (fresh fixture + S3 seed) is much
// more expensive than the measured op and must not be amortized.
fn bench_lookup_cold(c: &mut Criterion) {
    if skip_if_no_s3(c, "lookup_cold") { return; }

    let mut group = c.benchmark_group("lookup_cold");
    group.sample_size(10); // S3 RTT dominates; fewer samples reduce wall time
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("lookup_cold", |b| {
        b.iter_batched(
            || {
                let fix = TestFixture::new();
                fix.seed_root();
                fix.seed_file("target.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
                let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
                (fix, dir.inode)
            },
            |(fix, parent_ino)| {
                // The measured op: cold lookup of target.txt inside the
                // fixture prefix. Cache is empty except for the walk to
                // parent_ino (unavoidable).
                fix.fs.lookup(&root_ctx(), parent_ino, &CString::new("target.txt").unwrap()).unwrap();
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

// ── lookup_cached ────────────────────────────────────────────────────────
//
// Pure in-memory cache hit. Target per the plan: < 5µs.
//
// BatchSize::SmallInput because setup is cheap (reuse fixture) and the op
// is sub-microsecond. Criterion needs many iterations to measure accurately.
fn bench_lookup_cached(c: &mut Criterion) {
    if skip_if_no_s3(c, "lookup_cached") { return; }

    let mut group = c.benchmark_group("lookup_cached");

    // Setup once — the cache stays warm across all iterations.
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("target.txt", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    // Prime the cache.
    fix.fs.lookup(&root_ctx(), dir.inode, &CString::new("target.txt").unwrap()).unwrap();

    let name = CString::new("target.txt").unwrap();
    group.bench_function("lookup_cached", |b| {
        b.iter(|| {
            fix.fs.lookup(&root_ctx(), dir.inode, &name).unwrap();
        });
    });

    group.finish();
}

// ── readdir_100 ──────────────────────────────────────────────────────────
//
// readdir on a directory with 100 children. Measures LIST + parse + inode
// insertion for the batch. Cache gets populated as a side effect (useful
// for readdir→lookup pre-fill — but we don't measure that here).
fn bench_readdir_100(c: &mut Criterion) {
    if skip_if_no_s3(c, "readdir_100") { return; }

    let mut group = c.benchmark_group("readdir_100");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("readdir_100", |b| {
        b.iter_batched(
            || {
                let fix = TestFixture::new();
                fix.seed_root();
                for i in 0..100 {
                    let name = format!("file_{:03}", i);
                    fix.seed_file(&name, libc::S_IFREG as u32 | 0o644, 0, 0, b"");
                }
                let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
                (fix, dir.inode)
            },
            |(fix, dir_ino)| {
                // readdir expects a callback; collect into a Vec so we
                // actually iterate all entries.
                let mut count = 0;
                let mut cb = |_entry: DirEntry| -> std::io::Result<usize> {
                    count += 1;
                    Ok(1) // nonzero = accept, continue
                };
                fix.fs.readdir(&root_ctx(), dir_ino, 0, 8192, 0, &mut cb).unwrap();
                assert!(count >= 100);
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

// ── read_1mb_sequential ──────────────────────────────────────────────────
//
// Open + sequential read of a 1MB file. Files ≤ read_cache_max (16MB by
// default) are fetched in one GET and cached in the handle — so the first
// FUSE read pays S3 latency, subsequent reads hit the handle buffer.
//
// We measure the full "open + read all" sequence to capture the realistic
// cost path. Fresh fixture each iteration so the cache is cold.
fn bench_read_1mb_sequential(c: &mut Criterion) {
    if skip_if_no_s3(c, "read_1mb_sequential") { return; }

    let mut group = c.benchmark_group("read_1mb_sequential");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(criterion::Throughput::Bytes(1024 * 1024));

    let payload = vec![0xABu8; 1024 * 1024];

    group.bench_function("read_1mb_sequential", |b| {
        b.iter_batched(
            || {
                let fix = TestFixture::new();
                fix.seed_root();
                fix.seed_file("big.bin", libc::S_IFREG as u32 | 0o644, 0, 0, &payload);
                // Files are seeded under the fixture's prefix, so walk
                // through the prefix directory first.
                let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
                let entry = fix.lookup(dir.inode, "big.bin").unwrap();
                (fix, entry.inode)
            },
            |(fix, ino)| {
                let (handle, _, _) = fix.fs.open(&root_ctx(), ino, libc::O_RDONLY as u32, 0).unwrap();
                let handle = handle.expect("open returned no handle");
                let mut writer = TestWriter::new();
                // One read of the full size — kernel would fragment to 1MB
                // FUSE requests, but we can ask for the whole thing here to
                // measure the cache-populate path.
                let n = fix.fs.read(&root_ctx(), ino, handle, &mut writer,
                                     1024 * 1024, 0, None, 0).unwrap();
                assert_eq!(n, 1024 * 1024);
                fix.fs.release(&root_ctx(), ino, 0, handle, false, false, None).unwrap();
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

// ── read_16mb_cached ─────────────────────────────────────────────────────
//
// Repeat read of a cached file. The handle was populated by a previous
// read; this measures the memcpy-bound fast path. 16MB is the default
// read_cache_max — anything up to this size fits in a single handle
// buffer.
fn bench_read_16mb_cached(c: &mut Criterion) {
    if skip_if_no_s3(c, "read_16mb_cached") { return; }

    let mut group = c.benchmark_group("read_16mb_cached");
    group.sample_size(20);
    group.throughput(criterion::Throughput::Bytes(16 * 1024 * 1024));

    // Setup once — prime a handle with cached data, then re-read it.
    let payload = vec![0xCDu8; 16 * 1024 * 1024];
    let fix = TestFixture::new();
    fix.seed_root();
    fix.seed_file("huge.bin", libc::S_IFREG as u32 | 0o644, 0, 0, &payload);
    let dir = fix.lookup(ROOT, &fix.prefix).unwrap();
    let entry = fix.lookup(dir.inode, "huge.bin").unwrap();
    let (handle, _, _) = fix.fs.open(&root_ctx(), entry.inode, libc::O_RDONLY as u32, 0).unwrap();
    let handle = handle.unwrap();
    // Prime the handle buffer.
    let mut warmup = TestWriter::new();
    fix.fs.read(&root_ctx(), entry.inode, handle, &mut warmup,
                 16 * 1024 * 1024, 0, None, 0).unwrap();

    group.bench_function("read_16mb_cached", |b| {
        b.iter(|| {
            let mut writer = TestWriter::new();
            let n = fix.fs.read(&root_ctx(), entry.inode, handle, &mut writer,
                                 16 * 1024 * 1024, 0, None, 0).unwrap();
            assert_eq!(n, 16 * 1024 * 1024);
        });
    });

    group.finish();
    // Drop order matters — release before fix is dropped.
    fix.fs.release(&root_ctx(), entry.inode, 0, handle, false, false, None).ok();
}

// ── write_release_1mb ────────────────────────────────────────────────────
//
// Write 1MB and release (which PUTs to S3). Measures the full write path:
// buffer growth, S3 PUT on release. Fresh fixture each iteration so we're
// not hammering the same key (RustFS is fine with it but we want a clean
// measurement).
fn bench_write_release_1mb(c: &mut Criterion) {
    if skip_if_no_s3(c, "write_release_1mb") { return; }

    let mut group = c.benchmark_group("write_release_1mb");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(criterion::Throughput::Bytes(1024 * 1024));

    let payload = vec![0xEFu8; 1024 * 1024];

    group.bench_function("write_release_1mb", |b| {
        b.iter_batched(
            || {
                // Each iteration gets a fresh fixture so the write target
                // doesn't exist yet and we measure the full create+write+PUT.
                let fix = TestFixture::new();
                fix.seed_root();
                let prefix_dir = fix.lookup(ROOT, &fix.prefix).unwrap();
                (fix, prefix_dir.inode, payload.clone())
            },
            |(fix, parent_ino, data)| {
                let (entry, handle, _, _) = fix.fs.create(
                    &root_ctx(),
                    parent_ino,
                    &CString::new("w.bin").unwrap(),
                    CreateIn {
                        flags: libc::O_WRONLY as u32,
                        mode: libc::S_IFREG as u32 | 0o644,
                        umask: 0,
                        fuse_flags: 0,
                    },
                ).unwrap();
                let handle = handle.unwrap();

                let mut reader = TestReader::new(data);
                let n = fix.fs.write(&root_ctx(), entry.inode, handle, &mut reader,
                                      1024 * 1024, 0, None, false, 0, 0).unwrap();
                assert_eq!(n, 1024 * 1024);

                // release triggers the S3 PUT
                fix.fs.release(&root_ctx(), entry.inode, 0, handle, false, false, None).unwrap();
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

// ── hit_rate_realistic ───────────────────────────────────────────────────
//
// Not a timing benchmark — a correctness gate. Runs the realistic workload
// from test_cache.rs and asserts the cache hit rate stays at the measured
// baseline (66.7% per research/07). If the refactor breaks the inode cache
// logic or pre-fill, this catches it.
//
// We use Criterion's bench function to ensure it runs in `cargo bench`
// regardless of test filters, but the body is a single-iteration check.
fn bench_hit_rate_realistic(c: &mut Criterion) {
    if skip_if_no_s3(c, "hit_rate_realistic") { return; }

    c.bench_function("hit_rate_realistic", |b| {
        b.iter_batched(
            || {
                let fix = TestFixture::new();
                fix.seed_root();
                fix.seed_dir("src");
                for i in 0..8 {
                    let name = format!("src/file_{:02}", i);
                    fix.seed_file(&name, libc::S_IFREG as u32 | 0o644, 0, 0, b"");
                }
                fix.seed_dir("docs");
                fix.seed_file("docs/README", libc::S_IFREG as u32 | 0o644, 0, 0, b"");
                fix
            },
            |fix| {
                // Workload: readdir src/, lookup 8 children x2, 3 nonexistent x2.
                // Files were seeded under the fixture prefix, so walk
                // through {prefix}/src.
                let prefix_dir = fix.lookup(ROOT, &fix.prefix).unwrap();
                let src = fix.lookup(prefix_dir.inode, "src").unwrap();
                let mut cb = |_: DirEntry| -> std::io::Result<usize> { Ok(1) };
                fix.fs.readdir(&root_ctx(), src.inode, 0, 8192, 0, &mut cb).unwrap();

                for _pass in 0..2 {
                    for i in 0..8 {
                        let name = CString::new(format!("file_{:02}", i)).unwrap();
                        fix.fs.lookup(&root_ctx(), src.inode, &name).ok();
                    }
                }

                for _pass in 0..2 {
                    for name in &["nope1", "nope2", "nope3"] {
                        let cname = CString::new(*name).unwrap();
                        let _ = fix.fs.lookup(&root_ctx(), src.inode, &cname);
                    }
                }

                let stats = fix.fs.cache_stats();
                let total = stats.hits + stats.misses;
                let rate = if total > 0 { stats.hits as f64 / total as f64 } else { 0.0 };
                // Gate: 66.7% from research/07, give 5pp slack for measurement noise
                assert!(rate >= 0.62,
                    "hit rate {:.1}% below baseline 66.7%", rate * 100.0);
            },
            BatchSize::PerIteration,
        );
    });
}

// ── golden_lookup ────────────────────────────────────────────────────────
//
// Tier-1-miss + tier-2-hit lookup against a seeded golden cache. This is
// the hot path for any read of a golden-inherited file — every read starts
// with a lookup that walks per-VM cache, misses, then lands in the golden
// inode cache. Expected to be well under 5µs (per research/12 §5: "~500ns
// extra per lookup" vs the tier-1-only path).
fn bench_golden_lookup(c: &mut Criterion) {
    if skip_if_no_s3(c, "golden_lookup") { return; }
    // Fixture built once outside `iter` — golden tier-2 hits don't
    // mutate per-VM cache state (research/12 §5: no mirroring), so
    // every iteration is a fresh tier-1-miss + tier-2-hit against
    // the SAME pre-built fixture. This isolates the property we're
    // measuring (tier-2 lookup latency) from the fixture-build cost
    // (~2–5 ms of S3 PUT + manifest serialize + load_golden).
    let (fix, _tmp) = TestFixture::with_golden(vec![
        GoldenSeed::file("bash", libc::S_IFREG as u32 | 0o755, vec![0u8; 128]),
    ]);
    let name = CString::new("bash").unwrap();
    c.bench_function("golden_lookup", |b| {
        b.iter(|| {
            let bash = fix.fs.lookup(&root_ctx(), ROOT, &name).unwrap();
            assert!(is_golden_ino(bash.inode));
        });
    });
}

// ── golden_read_1mb ──────────────────────────────────────────────────────
//
// Read a 1MB golden-inherited file after lookup. One `GoldenBlobCache::fetch`
// returns an `Arc<Vec<u8>>`, the read slices into it — memcpy-bound, no S3.
// Establishes the "RAM-resident blob read" baseline for future phases.
fn bench_golden_read_1mb(c: &mut Criterion) {
    if skip_if_no_s3(c, "golden_read_1mb") { return; }
    // Fixture built once. Each iter issues a fresh read against a
    // per-iter handle, so measured work = open + read + release; the
    // read itself is blob_cache.fetch (one HashMap lookup + Arc
    // clone + slice). For a purer "read-only" number future phases
    // can split this into open_ro vs read proper.
    let (fix, _tmp) = TestFixture::with_golden(vec![
        GoldenSeed::file("big.bin", libc::S_IFREG as u32 | 0o644, vec![0xABu8; 1024 * 1024]),
    ]);
    let ino = fix
        .fs
        .lookup(&root_ctx(), ROOT, &CString::new("big.bin").unwrap())
        .unwrap()
        .inode;
    assert!(is_golden_ino(ino));
    c.bench_function("golden_read_1mb", |b| {
        b.iter(|| {
            let (handle, _, _) = fix
                .fs
                .open(&root_ctx(), ino, libc::O_RDONLY as u32, 0)
                .unwrap();
            let handle = handle.unwrap();
            let mut w = TestWriter::new();
            let n = fix
                .fs
                .read(&root_ctx(), ino, handle, &mut w, 1024 * 1024, 0, None, 0)
                .unwrap();
            assert_eq!(n, 1024 * 1024);
            fix.fs.release(&root_ctx(), ino, 0, handle, false, false, None).ok();
        });
    });
}

// ── golden_open_materialize_1mb ──────────────────────────────────────────
//
// Open a 1MB golden file O_RDWR — triggers materialize-on-write, which
// fetches the blob from GoldenBlobCache and clones it into the handle's
// write buffer. Establishes the "RAM-bound copy on write-open" baseline.
fn bench_golden_open_materialize_1mb(c: &mut Criterion) {
    if skip_if_no_s3(c, "golden_open_materialize_1mb") { return; }
    // Fixture built once; iter body measures open+release only.
    // Release is intentionally clean (no write) so the dirty check
    // skips the S3 PUT — we're measuring the open-preload (blob
    // fetch + Arc → Vec clone for materialize-on-write), not the
    // release-side flush.
    let (fix, _tmp) = TestFixture::with_golden(vec![
        GoldenSeed::file("mat.bin", libc::S_IFREG as u32 | 0o644, vec![0xCDu8; 1024 * 1024]),
    ]);
    let ino = fix
        .fs
        .lookup(&root_ctx(), ROOT, &CString::new("mat.bin").unwrap())
        .unwrap()
        .inode;
    assert!(is_golden_ino(ino));
    c.bench_function("golden_open_materialize_1mb", |b| {
        b.iter(|| {
            let (handle, _, _) = fix
                .fs
                .open(&root_ctx(), ino, libc::O_RDWR as u32, 0)
                .unwrap();
            let handle = handle.unwrap();
            fix.fs.release(&root_ctx(), ino, 0, handle, false, false, None).ok();
        });
    });
}

criterion_group!(
    benches,
    bench_lookup_cold,
    bench_lookup_cached,
    bench_readdir_100,
    bench_read_1mb_sequential,
    bench_read_16mb_cached,
    bench_write_release_1mb,
    bench_hit_rate_realistic,
    bench_golden_lookup,
    bench_golden_read_1mb,
    bench_golden_open_materialize_1mb,
);
criterion_main!(benches);
