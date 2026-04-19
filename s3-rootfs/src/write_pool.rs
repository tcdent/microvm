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

use std::collections::{HashMap, VecDeque};
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

    /// Upper cap on aggregate inflight bytes. When a new write
    /// would push past this, release falls back to a synchronous
    /// PUT inline. Prevents unbounded memory growth under burst
    /// write load.
    budget_bytes: usize,

    /// Workers check this on every loop iteration. `Drop` sets it
    /// true, notifies all waiters, and joins.
    shutdown: AtomicBool,

    /// S3 client for PUTs. `Arc` so workers can hold their own
    /// reference; `S3Client` is `Sync`.
    client: Arc<S3Client>,
}

/// Knobs for constructing a pool. Caller supplies the S3 client and
/// the budget; `S3Fs::new` fills both in from `FsConfig`.
pub struct WritePoolConfig {
    pub workers: usize,
    pub budget_bytes: usize,
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
            shutdown: AtomicBool::new(false),
            client: cfg.client,
        });

        let mut handles = Vec::with_capacity(cfg.workers);
        for i in 0..cfg.workers {
            let pool_ref = Arc::clone(&pool);
            let h = thread::Builder::new()
                .name(format!("s3fs-write-{i}"))
                .spawn(move || worker_loop(pool_ref))
                .expect("spawn write worker");
            handles.push(h);
        }
        (pool, handles)
    }

    /// Current inflight bytes. For tests and observability.
    pub fn current_bytes(&self) -> usize {
        self.current_inflight_bytes.load(Ordering::Relaxed)
    }

    /// Number of paths currently awaiting PUT. For tests and
    /// observability.
    pub fn inflight_count(&self) -> usize {
        self.inflight.lock().unwrap().len()
    }

    /// Look up a pending write by visible path. Used by
    /// `read()` for read-your-writes. Returns the `Arc<Vec<u8>>`
    /// so the caller can slice without holding the lock.
    pub fn get(&self, path: &str) -> Option<Arc<Vec<u8>>> {
        self.inflight
            .lock()
            .unwrap()
            .get(path)
            .map(|entry| Arc::clone(&entry.bytes))
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
                    ephemeral,
                },
            );
            self.current_inflight_bytes
                .fetch_add(size, Ordering::Relaxed);
        }

        // Ephemeral entries stay in the inflight map for reads but
        // never queue for S3 PUT — the inflight map IS their storage.
        if ephemeral {
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
        let mut inflight = self.inflight.lock().unwrap();
        while inflight.values().any(|e| !e.ephemeral) {
            inflight = self.inflight_cvar.wait(inflight).unwrap();
        }
    }

    /// Request graceful shutdown: stop accepting work, wake all
    /// waiters, let workers drain what's already in the queue.
    /// Called by `Drop`. Doesn't join threads — caller does that.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.queue_cvar.notify_all();
        self.inflight_cvar.notify_all();
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
            client,
        })
    }

    #[test]
    fn enqueue_over_budget_returns_false() {
        let (pool, handles) = make_pool(1, 100);
        let big = Arc::new(vec![0u8; 200]);
        let accepted = pool
            .enqueue("big.txt".into(), "big.txt\x01".into(), big, Durability::Durable)
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
        pool.enqueue("f".into(), "f\x01".into(), Arc::clone(&v1), Durability::Durable).unwrap();
        pool.enqueue("f".into(), "f\x01".into(), Arc::clone(&v2), Durability::Durable).unwrap();
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
    fn ephemeral_skips_queue_and_budget() {
        // Ephemeral entries: visible via get(), but never queue for
        // PUT and don't trigger budget rejection.
        let (pool, handles) = make_pool(0, 100); // 100-byte budget
        let big = Arc::new(vec![0u8; 200]);
        // Durable over-budget → rejected
        let accepted = pool
            .enqueue("d.txt".into(), "d.txt\x01".into(), Arc::clone(&big), Durability::Durable)
            .unwrap();
        assert!(!accepted, "durable over budget must be rejected");

        // Ephemeral over-budget → accepted (bypasses budget gate)
        let accepted = pool
            .enqueue("e.txt".into(), "e.txt\x01".into(), Arc::clone(&big), Durability::Ephemeral)
            .unwrap();
        assert!(accepted, "ephemeral must always be accepted");
        assert_eq!(pool.inflight_count(), 1);

        // Readable via get()
        let got = pool.get("e.txt").expect("ephemeral must be readable");
        assert_eq!(got.len(), 200);

        // wait_for_all returns immediately (only ephemeral in map)
        pool.wait_for_all();

        // wait_for_path returns immediately for ephemeral path
        pool.wait_for_path("e.txt");

        pool.shutdown.store(true, Ordering::Relaxed);
        pool.queue_cvar.notify_all();
        for h in handles {
            let _ = h.join();
        }
    }
}
