//! Deferred writes (research/13 §2).
//!
//! `release()` on a dirty handle wraps the bytes in `Arc<Vec<u8>>`,
//! parks them in an inflight map keyed by visible path, and signals
//! a worker pool to PUT to S3 in the background. The FUSE handler
//! thread never blocks on S3 during `release`.
//!
//! Read-your-writes: `read()` consults the inflight map before
//! falling through to the cache and S3, so a guest that writes and
//! immediately reads back a file always sees its own bytes.
//!
//! Backpressure: when the aggregate inflight bytes would exceed the
//! configured budget, the release path falls back to a synchronous
//! inline PUT — the caller waits, the inflight map is untouched for
//! that call. No special error surface, just the pre-deferred
//! behavior re-engaged until the queue drains.
//!
//! Durability: workers retry failed PUTs with exponential backoff
//! unbounded (100 ms → 500 ms → 2 s → 8 s → 30 s, then steady). A
//! sustained S3 outage manifests as the inflight queue growing,
//! which shows up in host RAM consumption — that's the operator
//! signal. `fsync()` is the opt-in durability point: blocks until
//! the named path's inflight entry clears.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::log;
use crate::ruleset::Durability;
use crate::s3::S3Client;

/// One pending write.
#[derive(Clone)]
pub struct InflightWrite {
    /// Data to PUT. Shared via `Arc` so the release path can hand
    /// the buffer off without copying; workers clone the `Arc`, not
    /// the bytes.
    pub bytes: Arc<Vec<u8>>,
    /// Pre-computed S3 key. Stored on the entry so workers don't
    /// need to know about key encoding.
    pub key: String,
    /// Diagnostic only — tracks how long the write has been waiting
    /// for a worker. Not used in the control path.
    #[allow(dead_code)]
    pub submitted_at: Instant,
    /// Ephemeral entries live in the inflight map for read-your-writes
    /// but never queue for S3 PUT. Workers skip them; wait_for_path
    /// and wait_for_all ignore them.
    pub ephemeral: bool,
}

/// Shared state between `S3Fs`, the writer threads, and any thread
/// calling `fsync`. All three mutate through this one struct, owned
/// by an `Arc` so the worker threads can outlive `S3Fs` if needed
/// (they don't today, but the shape is the right default).
pub struct WritePool {
    /// path → in-flight write. Key is the VISIBLE path (what the
    /// guest sees), not the S3 key — that way `read()` can match by
    /// the same path it would use to fetch from S3, and the inflight
    /// entry carries the actual S3 key on the side.
    inflight: Mutex<HashMap<String, InflightWrite>>,

    /// Worker wake signal — paired with `queue`. Workers wait here
    /// for "queue non-empty OR shutdown". `release()` pushes a path
    /// and notifies one worker.
    queue: Mutex<VecDeque<String>>,
    queue_cvar: Condvar,

    /// `fsync` waiters block on this — each PUT completion
    /// `notify_all`s and each waiter re-checks `inflight`. The
    /// critical section is tied to `inflight`'s mutex (waiters lock
    /// `inflight`, wait on this cvar, wake re-checking membership).
    inflight_cvar: Condvar,

    /// Sum of `bytes.len()` across inflight entries. Atomic so
    /// `release` can consult it to decide sync-fallback without
    /// locking the inflight mutex.
    current_inflight_bytes: AtomicUsize,

    /// Upper cap on aggregate durable inflight bytes. When a new
    /// durable write would push past this, release falls back to a
    /// synchronous PUT inline. Prevents unbounded memory growth
    /// under burst write load.
    budget_bytes: usize,

    /// Local directory for ephemeral writes. When set, ephemeral
    /// files are written to disk here instead of held in the inflight
    /// map. Reads serve from the local file. No RAM pressure, no
    /// eviction — disk is effectively unbounded. Cleaned up on Drop.
    /// `None` means ephemeral falls back to durable behavior.
    ephemeral_dir: Option<std::path::PathBuf>,

    /// Workers check this on every loop iteration. `Drop` sets it
    /// true, notifies all waiters, and joins.
    shutdown: AtomicBool,

    /// S3 client for PUTs. `Arc` so workers can hold their own
    /// reference; `S3Client` is `Sync`.
    client: Arc<S3Client>,

    /// Coalesce (debounce) state for writes with `coalesce_ms` rules
    /// (research/14 §3). Writes enter the inflight map immediately
    /// but don't queue for PUT until the debounce timer expires.
    /// Subsequent writes to the same path restart the timer.
    coalesce: Mutex<CoalesceState>,
    /// Wakes the timer thread when a new coalesced write arrives or
    /// an existing timer is flushed. Also woken on shutdown.
    coalesce_cvar: Condvar,
}

/// Debounce timer state. Two indexes for O(log n) insert/remove:
/// - `by_path`: quick check "is this path pending?" + retrieve its
///   expiry for removal from the time-sorted index.
/// - `by_time`: sorted by `(expiry, path)` — the timer thread pops
///   from the front.
struct CoalesceState {
    by_path: HashMap<String, Instant>,
    by_time: BTreeMap<(Instant, String), ()>,
}

/// Knobs for constructing a pool. Caller supplies the S3 client and
/// the budget; `S3Fs::new` fills both in from `FsConfig`.
pub struct WritePoolConfig {
    pub workers: usize,
    pub budget_bytes: usize,
    /// Local directory for ephemeral writes. `None` means ephemeral
    /// falls back to durable (no silent data loss).
    pub ephemeral_dir: Option<std::path::PathBuf>,
    pub client: Arc<S3Client>,
}

impl WritePool {
    /// Construct a pool and spawn `cfg.workers` background threads
    /// bound to its shared state. Returns the shared `Arc` plus the
    /// spawned `JoinHandle`s. Callers typically hold both — the
    /// Arc for `release`/`fsync` access, the handles for clean
    /// shutdown in `Drop`.
    pub fn start(cfg: WritePoolConfig) -> (Arc<WritePool>, Vec<JoinHandle<()>>) {
        let pool = Arc::new(WritePool {
            inflight: Mutex::new(HashMap::new()),
            queue: Mutex::new(VecDeque::new()),
            queue_cvar: Condvar::new(),
            inflight_cvar: Condvar::new(),
            current_inflight_bytes: AtomicUsize::new(0),
            budget_bytes: cfg.budget_bytes,
            ephemeral_dir: cfg.ephemeral_dir,
            shutdown: AtomicBool::new(false),
            client: cfg.client,
            coalesce: Mutex::new(CoalesceState {
                by_path: HashMap::new(),
                by_time: BTreeMap::new(),
            }),
            coalesce_cvar: Condvar::new(),
        });

        // +1 for the coalesce timer thread.
        let mut handles = Vec::with_capacity(cfg.workers + 1);
        for i in 0..cfg.workers {
            let pool_ref = Arc::clone(&pool);
            let h = thread::Builder::new()
                .name(format!("s3fs-write-{i}"))
                .spawn(move || worker_loop(pool_ref))
                .expect("spawn write worker");
            handles.push(h);
        }

        // Timer thread: wakes on the earliest coalesce expiry,
        // pops expired entries, pushes them to the worker queue.
        {
            let pool_ref = Arc::clone(&pool);
            let h = thread::Builder::new()
                .name("s3fs-coalesce".into())
                .spawn(move || coalesce_loop(pool_ref))
                .expect("spawn coalesce timer");
            handles.push(h);
        }

        (pool, handles)
    }

    /// Current inflight bytes. For tests and observability.
    pub fn current_bytes(&self) -> usize {
        self.current_inflight_bytes.load(Ordering::Relaxed)
    }

    /// The ephemeral directory path, if configured.
    pub fn ephemeral_dir(&self) -> Option<&std::path::Path> {
        self.ephemeral_dir.as_deref()
    }

    /// Number of paths currently awaiting PUT. For tests and
    /// observability.
    pub fn inflight_count(&self) -> usize {
        self.inflight.lock().unwrap().len()
    }

    /// Move an inflight entry from old_path to new_path with a new
    /// S3 key. Used by rename() for ephemeral files that were never
    /// PUT to S3 — the inflight map IS their storage, so the entry
    /// must follow the rename. No-op if old_path has no inflight entry.
    pub fn rename_path(&self, old_path: &str, new_path: &str, new_key: String) {
        // Move inflight entry if present.
        let mut inflight = self.inflight.lock().unwrap();
        if let Some(mut entry) = inflight.remove(old_path) {
            entry.key = new_key;
            inflight.insert(new_path.to_string(), entry);
        }
        // Also update any pending coalesce timer.
        let mut cs = self.coalesce.lock().unwrap();
        if let Some(expiry) = cs.by_path.remove(old_path) {
            cs.by_time.remove(&(expiry, old_path.to_string()));
            cs.by_path.insert(new_path.to_string(), expiry);
            cs.by_time.insert((expiry, new_path.to_string()), ());
        }
        // Move ephemeral file on disk if present.
        if let Some(ref dir) = self.ephemeral_dir {
            let old_file = dir.join(old_path);
            if old_file.exists() {
                let new_file = dir.join(new_path);
                if let Some(parent) = new_file.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::rename(&old_file, &new_file);
            }
        }
    }

    /// Look up a pending write by visible path. Used by
    /// `read()` for read-your-writes. Checks the inflight map first,
    /// then the ephemeral disk directory (if configured).
    pub fn get(&self, path: &str) -> Option<Arc<Vec<u8>>> {
        // Check inflight map (durable writes in transit).
        let inflight_hit = self.inflight
            .lock()
            .unwrap()
            .get(path)
            .map(|entry| Arc::clone(&entry.bytes));
        if inflight_hit.is_some() {
            return inflight_hit;
        }
        // Check ephemeral disk.
        if let Some(ref dir) = self.ephemeral_dir {
            return self.read_ephemeral_file(dir, path);
        }
        None
    }

    /// Try to enqueue a write for deferred PUT. Returns `Ok(true)`
    /// if accepted into the inflight queue, `Ok(false)` if over
    /// budget (caller must fall back to synchronous PUT).
    ///
    /// If a pending write already exists for `path`, it is replaced
    /// — the newer write supersedes. The old Arc's refcount drops to
    /// whatever readers still hold it; its bytes free when those
    /// drop too.
    pub fn enqueue(
        &self,
        path: String,
        key: String,
        bytes: Arc<Vec<u8>>,
        durability: Durability,
        coalesce: Option<Duration>,
    ) -> io::Result<bool> {
        let ephemeral = durability == Durability::Ephemeral;
        let size = bytes.len();

        // Budget check — cheap atomic load, no lock. Racy: two
        // concurrent enqueues might both see "room" and both
        // commit, exceeding budget by up to one buffer. Acceptable
        // slack; the next enqueue after that correctly sync-falls-
        // back. Tightening would need a Mutex around the decision,
        // which doesn't pay.
        //
        // Ephemeral writes skip the budget gate — they never queue
        // for S3 PUT and the inflight map IS their storage. Over-
        // budget for ephemeral means RAM pressure; a future eviction
        // pass will handle that (research/14 §2).
        if !ephemeral {
            let current = self.current_inflight_bytes.load(Ordering::Relaxed);
            if current.saturating_add(size) > self.budget_bytes {
                return Ok(false);
            }
        }

        // Ephemeral: write to local disk, not the inflight map.
        // No RAM pressure, no eviction — disk handles the storage.
        // If no ephemeral_dir is configured, fall through to durable.
        if ephemeral {
            if let Some(ref dir) = self.ephemeral_dir {
                self.write_ephemeral_file(dir, &path, &bytes)?;
                return Ok(true);
            }
            // No ephemeral_dir → fall through to durable behavior.
        }

        // Insert (or replace) in inflight map, adjust counter.
        {
            let mut inflight = self.inflight.lock().unwrap();
            if let Some(old) = inflight.remove(&path) {
                self.current_inflight_bytes
                    .fetch_sub(old.bytes.len(), Ordering::Relaxed);
            }
            inflight.insert(
                path.clone(),
                InflightWrite {
                    bytes,
                    key,
                    submitted_at: Instant::now(),
                    ephemeral: false,
                },
            );
            self.current_inflight_bytes
                .fetch_add(size, Ordering::Relaxed);
        }

        // Coalesced writes enter the debounce timer instead of the
        // worker queue. The timer thread pushes to queue on expiry.
        // Subsequent writes to the same path restart the timer.
        if let Some(window) = coalesce {
            let expiry = Instant::now() + window;
            let mut cs = self.coalesce.lock().unwrap();
            // Remove any existing timer for this path (debounce restart).
            if let Some(old_expiry) = cs.by_path.remove(&path) {
                cs.by_time.remove(&(old_expiry, path.clone()));
            }
            cs.by_path.insert(path.clone(), expiry);
            cs.by_time.insert((expiry, path), ());
            self.coalesce_cvar.notify_one();
            return Ok(true);
        }

        // Push onto queue and wake one worker. Duplicate queue
        // entries for the same path are harmless: workers handle
        // "inflight entry gone by the time we got here" by skipping.
        {
            let mut q = self.queue.lock().unwrap();
            q.push_back(path);
        }
        self.queue_cvar.notify_one();
        Ok(true)
    }

    /// Block until the named path has no inflight write. Used by
    /// `fsync`. If the path isn't inflight, returns immediately.
    pub fn wait_for_path(&self, path: &str) {
        // Flush any pending coalesce timer for this path first —
        // force immediate enqueue so the worker can drain it.
        self.flush_coalesce(path);

        let mut inflight = self.inflight.lock().unwrap();
        // Ephemeral entries have no pending PUT — they're "done"
        // from the durability perspective the moment they're in
        // the inflight map.
        while inflight.get(path).is_some_and(|e| !e.ephemeral) {
            inflight = self.inflight_cvar.wait(inflight).unwrap();
        }
    }

    /// Block until all non-ephemeral inflight writes have drained.
    /// Used by `syncfs`-style whole-filesystem fsync, rename, and
    /// clean shutdown. Ephemeral entries are excluded — they never
    /// queue for PUT and would block forever.
    pub fn wait_for_all(&self) {
        // Flush all pending coalesce timers — force immediate enqueue.
        self.flush_all_coalesce();

        let mut inflight = self.inflight.lock().unwrap();
        while inflight.values().any(|e| !e.ephemeral) {
            inflight = self.inflight_cvar.wait(inflight).unwrap();
        }
    }

    /// Write an ephemeral file to the local disk directory.
    /// Creates parent directories as needed. Path on disk mirrors
    /// the S3 key path so reads can find it by the same name.
    fn write_ephemeral_file(
        &self,
        dir: &std::path::Path,
        path: &str,
        data: &[u8],
    ) -> io::Result<()> {
        let file_path = dir.join(path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&file_path, data)?;
        Ok(())
    }

    /// Read an ephemeral file from the local disk directory.
    /// Returns None if the file doesn't exist.
    fn read_ephemeral_file(
        &self,
        dir: &std::path::Path,
        path: &str,
    ) -> Option<Arc<Vec<u8>>> {
        let file_path = dir.join(path);
        std::fs::read(&file_path).ok().map(|v| Arc::new(v))
    }

    /// Force a coalesced path into the worker queue immediately,
    /// canceling its debounce timer. No-op if the path has no
    /// pending timer. Used by fsync (durability opt-in).
    fn flush_coalesce(&self, path: &str) {
        let mut cs = self.coalesce.lock().unwrap();
        if let Some(expiry) = cs.by_path.remove(path) {
            cs.by_time.remove(&(expiry, path.to_string()));
            let mut q = self.queue.lock().unwrap();
            q.push_back(path.to_string());
            self.queue_cvar.notify_one();
        }
    }

    /// Force all coalesced paths into the worker queue. Used by
    /// wait_for_all (rename drain, shutdown).
    fn flush_all_coalesce(&self) {
        let mut cs = self.coalesce.lock().unwrap();
        if cs.by_path.is_empty() {
            return;
        }
        let paths: Vec<String> = cs.by_path.drain().map(|(p, _)| p).collect();
        cs.by_time.clear();
        let mut q = self.queue.lock().unwrap();
        for p in paths {
            q.push_back(p);
        }
        self.queue_cvar.notify_all();
    }

    /// Request graceful shutdown: stop accepting work, wake all
    /// waiters, let workers drain what's already in the queue.
    /// Called by `Drop`. Doesn't join threads — caller does that.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.queue_cvar.notify_all();
        self.inflight_cvar.notify_all();
        self.coalesce_cvar.notify_all();
    }
}

/// Worker loop. Pops a path from the queue, reads the latest
/// inflight entry for that path (handles dedup races — always PUTs
/// the freshest version), PUTs with exponential-backoff retry,
/// removes from inflight on success, notifies fsync waiters.
fn worker_loop(pool: Arc<WritePool>) {
    loop {
        // Wait for work or shutdown.
        let path = {
            let mut q = pool.queue.lock().unwrap();
            loop {
                if let Some(p) = q.pop_front() {
                    break p;
                }
                if pool.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                q = pool.queue_cvar.wait(q).unwrap();
            }
        };

        // Snapshot the latest inflight entry for this path. If it's
        // gone (another worker already PUT it, or release replaced
        // it and this queue entry is stale), skip.
        let entry = match pool.inflight.lock().unwrap().get(&path).cloned() {
            Some(e) => e,
            None => continue,
        };

        // PUT with exponential backoff, unbounded. Each retry logs
        // so sustained failure is visible in logs. Successful PUT
        // ends the inner loop.
        let mut backoff = Duration::from_millis(100);
        loop {
            match pool.client.put(&entry.key, &entry.bytes) {
                Ok(()) => break,
                Err(e) => {
                    log!(
                        "S3Fs: deferred PUT failed for {} (key={}): {} — retrying in {:?}",
                        path,
                        entry.key,
                        e,
                        backoff
                    );
                    thread::sleep(backoff);
                    backoff = std::cmp::min(backoff * 4, Duration::from_secs(30));

                    // Honor shutdown even mid-retry loop. Data for
                    // this entry will be lost (spec §2.4): the
                    // process is going down.
                    if pool.shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                }
            }
        }

        // Success: remove from inflight ONLY IF it's still the same
        // entry we just PUT. A concurrent `release()` could have
        // replaced the entry with a newer write between our snapshot
        // and now; removing unconditionally would lose the new write.
        // We compare by Arc identity — `Arc::ptr_eq` on the bytes.
        {
            let mut inflight = pool.inflight.lock().unwrap();
            if let Some(current) = inflight.get(&path) {
                if Arc::ptr_eq(&current.bytes, &entry.bytes) {
                    let removed = inflight.remove(&path).unwrap();
                    pool.current_inflight_bytes
                        .fetch_sub(removed.bytes.len(), Ordering::Relaxed);
                } else {
                    // Newer write appeared — don't touch; another
                    // worker will pick it up from its own queue entry.
                }
            }
            // If the entry's entirely gone, nothing to do.
        }
        pool.inflight_cvar.notify_all();
    }
}

/// Timer thread for coalesced writes (research/14 §3). Sleeps until
/// the earliest timer expiry, pops expired entries, pushes them to
/// the worker queue. Woken by `coalesce_cvar` when a new timer is
/// set or flushed, or on shutdown.
fn coalesce_loop(pool: Arc<WritePool>) {
    loop {
        let mut cs = pool.coalesce.lock().unwrap();
        loop {
            if pool.shutdown.load(Ordering::Relaxed) {
                return;
            }

            // Pop any expired timers.
            let now = Instant::now();
            let mut fired = Vec::new();
            while let Some((&(expiry, ref path), _)) = cs.by_time.first_key_value() {
                if expiry > now {
                    break;
                }
                let path = path.clone();
                cs.by_time.pop_first();
                cs.by_path.remove(&path);
                fired.push(path);
            }

            if !fired.is_empty() {
                let mut q = pool.queue.lock().unwrap();
                for p in fired {
                    q.push_back(p);
                }
                pool.queue_cvar.notify_all();
                continue; // re-check for more expired
            }

            // Sleep until the next expiry or until woken.
            if let Some((&(expiry, _), _)) = cs.by_time.first_key_value() {
                let wait = expiry.saturating_duration_since(now);
                let (guard, _) = pool.coalesce_cvar.wait_timeout(cs, wait).unwrap();
                cs = guard;
            } else {
                // No pending timers — sleep until woken.
                cs = pool.coalesce_cvar.wait(cs).unwrap();
            }
        }
    }
}

// `io` usage inside this module is confined to one return type.
use std::io;

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a pool with a mock `S3Client` that returns Ok(()) on
    /// every PUT. Uses the real default config since we don't actually
    /// talk to a server — and the build_agent call is cheap.
    fn make_pool(workers: usize, budget: usize) -> (Arc<WritePool>, Vec<JoinHandle<()>>) {
        let cfg = crate::s3::S3Config::default();
        let agent = Arc::new(S3Client::build_agent(&cfg));
        let client = Arc::new(S3Client::with_shared_agent(cfg, agent));
        WritePool::start(WritePoolConfig {
            workers,
            budget_bytes: budget,
            ephemeral_dir: None,
            client,
        })
    }

    #[test]
    fn enqueue_over_budget_returns_false() {
        let (pool, handles) = make_pool(1, 100);
        let big = Arc::new(vec![0u8; 200]);
        let accepted = pool
            .enqueue("big.txt".into(), "big.txt\x01".into(), big, Durability::Durable, None)
            .unwrap();
        assert!(
            !accepted,
            "enqueue past budget must return false (backpressure signal)"
        );
        assert_eq!(pool.current_bytes(), 0);
        assert_eq!(pool.inflight_count(), 0);

        pool.shutdown.store(true, Ordering::Relaxed);
        pool.queue_cvar.notify_all();
        for h in handles {
            let _ = h.join();
        }
    }

    #[test]
    fn get_reads_inflight_value() {
        // Pool won't actually PUT successfully against a real S3,
        // but the enqueue path is synchronous — we can observe
        // inflight membership before the worker gets scheduled.
        let (pool, handles) = make_pool(0, 10_000); // zero workers → nothing drains
        let payload = Arc::new(b"hello world".to_vec());
        let accepted = pool
            .enqueue(
                "greet.txt".into(),
                "greet.txt\x01meta".into(),
                Arc::clone(&payload),
                Durability::Durable,
                None,
            )
            .unwrap();
        assert!(accepted);
        assert_eq!(pool.inflight_count(), 1);

        let got = pool.get("greet.txt").expect("must see our own write");
        assert!(Arc::ptr_eq(&got, &payload));

        pool.shutdown.store(true, Ordering::Relaxed);
        pool.queue_cvar.notify_all();
        for h in handles {
            let _ = h.join();
        }
    }

    #[test]
    fn enqueue_replaces_pending_write_for_same_path() {
        let (pool, handles) = make_pool(0, 10_000);
        let v1 = Arc::new(vec![0x11u8; 100]);
        let v2 = Arc::new(vec![0x22u8; 200]);
        pool.enqueue("f".into(), "f\x01".into(), Arc::clone(&v1), Durability::Durable, None).unwrap();
        pool.enqueue("f".into(), "f\x01".into(), Arc::clone(&v2), Durability::Durable, None).unwrap();
        assert_eq!(pool.inflight_count(), 1);
        // Total bytes = only the newer write (100 replaced, 200 now).
        assert_eq!(pool.current_bytes(), 200);
        let got = pool.get("f").unwrap();
        assert!(Arc::ptr_eq(&got, &v2));

        pool.shutdown.store(true, Ordering::Relaxed);
        pool.queue_cvar.notify_all();
        for h in handles {
            let _ = h.join();
        }
    }

    #[test]
    fn wait_for_all_returns_when_empty() {
        let (pool, handles) = make_pool(0, 10_000);
        // No inflight writes → wait_for_all returns immediately.
        pool.wait_for_all();

        pool.shutdown.store(true, Ordering::Relaxed);
        pool.queue_cvar.notify_all();
        for h in handles {
            let _ = h.join();
        }
    }

    #[test]
    fn ephemeral_writes_to_disk() {
        // Ephemeral with ephemeral_dir: writes to local disk,
        // readable via get(), never touches inflight map or S3.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::s3::S3Config::default();
        let agent = Arc::new(S3Client::build_agent(&cfg));
        let client = Arc::new(S3Client::with_shared_agent(cfg, agent));
        let (pool, handles) = WritePool::start(WritePoolConfig {
            workers: 0,
            budget_bytes: 100,
            ephemeral_dir: Some(tmp.path().to_path_buf()),
            client,
        });

        let data = Arc::new(b"ephemeral content on disk".to_vec());
        let accepted = pool
            .enqueue("cache/pkg.tgz".into(), "cache/pkg.tgz\x01".into(),
                      Arc::clone(&data), Durability::Ephemeral, None)
            .unwrap();
        assert!(accepted, "ephemeral must always be accepted");

        // Not in inflight map (written to disk instead).
        assert_eq!(pool.inflight_count(), 0);

        // Readable via get() (reads from disk).
        let got = pool.get("cache/pkg.tgz").expect("must read from disk");
        assert_eq!(&*got, &*data);

        // File exists on disk.
        assert!(tmp.path().join("cache/pkg.tgz").exists());

        pool.shutdown.store(true, Ordering::Relaxed);
        pool.queue_cvar.notify_all();
        pool.coalesce_cvar.notify_all();
        for h in handles {
            let _ = h.join();
        }
    }

    #[test]
    fn coalesce_debounces_then_drains() {
        // Coalesced writes don't queue immediately; they wait for the
        // debounce timer. Two rapid writes to the same path should
        // produce exactly one entry when the timer fires.
        let (pool, handles) = make_pool(0, 100_000); // 0 workers — manual drain

        let v1 = Arc::new(b"first".to_vec());
        let v2 = Arc::new(b"second".to_vec());
        let window = Duration::from_millis(50);

        pool.enqueue("c.txt".into(), "c\x01".into(), Arc::clone(&v1),
                      Durability::Durable, Some(window)).unwrap();
        pool.enqueue("c.txt".into(), "c\x01".into(), Arc::clone(&v2),
                      Durability::Durable, Some(window)).unwrap();

        // Queue should be empty (coalesced, waiting for timer)
        assert!(pool.queue.lock().unwrap().is_empty(),
            "coalesced writes must not appear in queue before timer");

        // Inflight has the latest bytes
        let got = pool.get("c.txt").unwrap();
        assert!(Arc::ptr_eq(&got, &v2), "inflight must hold latest write");

        // Wait for timer to fire (50ms + slack)
        thread::sleep(Duration::from_millis(100));

        // Queue should now have one entry
        let q = pool.queue.lock().unwrap();
        assert_eq!(q.len(), 1, "timer must have pushed exactly one entry");

        pool.shutdown.store(true, Ordering::Relaxed);
        pool.queue_cvar.notify_all();
        pool.coalesce_cvar.notify_all();
        drop(q);
        for h in handles {
            let _ = h.join();
        }
    }

    #[test]
    fn flush_coalesce_forces_immediate_queue() {
        // fsync must flush the coalesce timer and push to queue
        // immediately, not wait for the debounce window.
        let (pool, handles) = make_pool(0, 100_000);

        let data = Arc::new(b"fsync me".to_vec());
        let window = Duration::from_secs(60); // very long — won't fire naturally

        pool.enqueue("s.txt".into(), "s\x01".into(), data,
                      Durability::Durable, Some(window)).unwrap();
        assert!(pool.queue.lock().unwrap().is_empty());

        // flush_coalesce should push immediately
        pool.flush_coalesce("s.txt");
        assert_eq!(pool.queue.lock().unwrap().len(), 1);

        pool.shutdown.store(true, Ordering::Relaxed);
        pool.queue_cvar.notify_all();
        pool.coalesce_cvar.notify_all();
        for h in handles {
            let _ = h.join();
        }
    }
}
