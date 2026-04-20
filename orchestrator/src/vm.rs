//! VM thread lifecycle (research/15 §1, phases 3+4).
//!
//! Each VM runs in a dedicated thread that calls microvm::launcher::run
//! with a LaunchConfig derived from the VmSpec plus the SharedContext.
//! The thread is wrapped in panic::catch_unwind so one VM's crash
//! doesn't take the orchestrator down.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use microvm::launcher::LaunchConfig;
use s3_rootfs::config::FsConfig;

use crate::ipc::proto::{VmSpec, VmState};
use crate::registry::{VmEntry, VmRegistry};
use crate::shared::SharedContext;
use crate::Error;

/// Spawn a new VM thread. Allocates resources, inserts into registry
/// as Starting, launches the thread. Returns the name_hash for the
/// caller to look up the entry.
pub fn spawn(
    spec: VmSpec,
    shared: Arc<SharedContext>,
    registry: Arc<VmRegistry>,
) -> Result<String, Error> {
    let name_hash = VmRegistry::name_hash(&spec.name);

    // 1. Collision check — registry.insert will reject if already running.
    // (We do the insert at step 8 below, which has the same check.)

    // 2. Bucket — ensure exists via S3 admin (HEAD + PUT if missing).
    let bucket = crate::bucket::ensure(
        &name_hash,
        spec.bucket.as_deref(),
        &shared.s3_admin,
    )?;

    // 3. Ephemeral dir — on-disk, per-VM.
    let ephemeral_dir = spec.ephemeral_dir.clone()
        .unwrap_or_else(|| shared.config.ephemeral_root.join(&name_hash));
    if let Err(e) = std::fs::create_dir_all(&ephemeral_dir) {
        return Err(Error::Spawn(format!("create ephemeral dir: {e}")));
    }

    // 4. Network — allocate tap on demand, or use spec-provided name.
    let (net_ifname, allocated_tap) = match spec.net_ifname.clone() {
        Some(name) => (Some(name), None), // user-provided, skip allocation
        None => match crate::tap::allocate(&name_hash) {
            Ok(tap) => (Some(tap.clone()), Some(tap)),
            Err(e) => {
                eprintln!("vm::spawn: tap allocate failed ({}), proceeding without net", e);
                (None, None)
            }
        },
    };

    // 5. Log dir — per-VM log directory.
    let log_dir = shared.config.log_dir.join(&name_hash);
    let _ = std::fs::create_dir_all(&log_dir);

    // 6. Build FsConfig.
    let mut fs = FsConfig::default();
    fs.s3.endpoint = shared.config.s3_endpoint.clone();
    fs.s3.bucket = bucket;
    fs.ephemeral_dir = Some(ephemeral_dir);

    if let Some(ref ruleset) = spec.ruleset.as_ref()
        .or(shared.config.default_ruleset.as_ref())
    {
        if ruleset.exists() {
            fs.ruleset_path = Some(ruleset.to_path_buf());
        }
    }

    // Golden: use shared pre-loaded caches if the spec doesn't
    // override with a different golden bucket. If spec has a
    // different golden_bucket, fall back to per-VM load via FsConfig.
    let use_shared_golden = spec.golden_bucket.is_none()
        && shared.golden_inodes.is_some()
        && shared.golden_blobs.is_some();

    if !use_shared_golden {
        if let Some(ref golden) = spec.golden_bucket.as_ref()
            .or(shared.config.default_golden_bucket.as_ref())
        {
            fs.golden = Some(s3_rootfs::GoldenConfig {
                bucket: golden.to_string(),
                manifest: s3_rootfs::ManifestSource::S3Key(".manifest".to_string()),
            });
        }
    }

    // 7. Build LaunchConfig with shared shutdown flag.
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let cfg = LaunchConfig {
        kernel_path: spec.kernel.as_ref()
            .unwrap_or(&shared.config.default_kernel)
            .to_string_lossy()
            .into_owned(),
        initrd_path: spec.initrd.as_ref()
            .unwrap_or(&shared.config.default_initrd)
            .to_string_lossy()
            .into_owned(),
        net_ifname,
        mem_mb: spec.mem_mb as usize,
        vm_id: name_hash.clone(),
        fs,
        shutdown: Some(Arc::clone(&shutdown_flag)),
        log_file: Some(log_dir.join("serial.log")),
    };

    // 8. Insert into registry as Starting.
    let entry = VmEntry {
        name_hash: name_hash.clone(),
        spec,
        state: VmState::Starting,
        started_at: Instant::now(),
        thread: None,
        shutdown_flag,
        allocated_tap: allocated_tap.clone(),
    };
    registry.insert(entry)?;

    // 9. Spawn the VM thread.
    let reg = Arc::clone(&registry);
    let hash = name_hash.clone();
    // Clone shared golden Arcs for the thread (cheap Arc::clone).
    let shared_golden_inodes = if use_shared_golden {
        shared.golden_inodes.clone()
    } else { None };
    let shared_golden_blobs = if use_shared_golden {
        shared.golden_blobs.clone()
    } else { None };
    let handle = std::thread::Builder::new()
        .name(format!("vm-{}", name_hash))
        .spawn(move || {
            reg.transition_state(&hash, VmState::Running);

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Use shared golden caches if available, else per-VM load.
                if let (Some(inodes), Some(blobs)) = (shared_golden_inodes, shared_golden_blobs) {
                    let fs = std::sync::Arc::new(
                        s3_rootfs::S3Fs::new_with_golden(cfg.fs.clone(), inodes, blobs)
                    );
                    microvm::launcher::launch_vm(cfg, fs)
                } else {
                    microvm::launcher::run(cfg)
                }
            }));

            let final_state = match result {
                Ok(Ok(())) => VmState::Stopped,
                Ok(Err(e)) => {
                    eprintln!("vm-{}: exited with error: {}", hash, e);
                    VmState::Crashed
                }
                Err(_) => {
                    eprintln!("vm-{}: thread panicked", hash);
                    VmState::Crashed
                }
            };
            reg.transition_state(&hash, final_state);
        })
        .map_err(|e| Error::Spawn(format!("thread spawn: {e}")))?;

    // 10. Store the handle on the registry entry.
    registry.set_thread(&name_hash, handle);

    Ok(name_hash)
}
