//! Orchestrator library (research/15).
//!
//! Owns the multi-VM lifecycle inside a single process. Shared
//! resources (S3 client, golden-FS caches, write pool) live on
//! [`SharedContext`]; per-VM state lives in the [`VmRegistry`].
//! Both are constructed at daemon start and shared by Arc across
//! VM threads.
//!
//! The public API is intentionally transport-agnostic: the daemon
//! binary wraps this behind a Unix-socket JSON protocol, and a
//! future HTTP adapter will wrap the same API without change.

pub mod config;
pub mod registry;
pub mod shared;
pub mod vm;
pub mod bucket;
pub mod log;
pub mod ipc;
pub mod tap;

use std::sync::Arc;

use registry::VmRegistry;
use shared::SharedContext;

pub use config::DaemonConfig;
pub use ipc::proto::{Command, Response, VmSpec, VmInfo, VmState};

/// Top-level orchestrator. Owns the SharedContext and VmRegistry.
/// Wrapped by the daemon binary; accessed by IPC handlers.
pub struct Orchestrator {
    pub shared: Arc<SharedContext>,
    pub registry: Arc<VmRegistry>,
    pub config: DaemonConfig,
}

impl Orchestrator {
    /// Build an orchestrator from a daemon config. Creates the log
    /// directory, initializes the tap pool, opens the bucket-admin
    /// client. Does NOT preload any s3-rootfs state — each VM thread
    /// builds its own S3Fs via the existing `S3Fs::new` API.
    pub fn new(config: DaemonConfig) -> Result<Self, Error> {
        let shared = Arc::new(SharedContext::build(config.clone())?);
        let registry = Arc::new(VmRegistry::new());
        Ok(Self { shared, registry, config })
    }

    /// Spawn a new VM thread. Allocates bucket + ephemeral dir if
    /// the spec doesn't provide them. Returns once the VM is in the
    /// Starting state; Running state is signaled via VmRegistry.
    pub fn start_vm(&self, spec: VmSpec) -> Result<VmInfo, Error> {
        let name = spec.name.clone();
        vm::spawn(spec, Arc::clone(&self.shared), Arc::clone(&self.registry))?;
        self.registry.inspect(&name).ok_or(Error::NotFound)
    }

    /// Graceful shutdown: drain writes, stop vcpu, join thread.
    /// Preserves the bucket (bucket deletion is a separate op).
    pub fn stop_vm(&self, name: &str) -> Result<(), Error> {
        let hash = registry::VmRegistry::name_hash(name);

        // Check current state.
        let info = self.registry.inspect(name).ok_or(Error::NotFound)?;
        match info.state {
            VmState::Stopped | VmState::Crashed => return Ok(()),
            VmState::Stopping => {
                // Already stopping — just wait for the thread.
                if let Some(handle) = self.registry.take_thread(&hash) {
                    let _ = handle.join();
                }
                return Ok(());
            }
            VmState::Starting | VmState::Running => {}
        }

        self.registry.transition_state(&hash, VmState::Stopping);

        // Phase 1: graceful — send `poweroff` to the guest's hvc0
        // console. The guest's autologin root shell runs the command,
        // systemd shuts down services in order, syncs filesystems,
        // and powers off. The VM thread exits on KVM_EXIT_SHUTDOWN.
        if let Some(mut writer) = self.registry.console_writer(&hash) {
            use std::io::Write;
            let _ = writer.write_all(b"\npoweroff\n");
            let _ = writer.flush();
            drop(writer); // close write end so RX worker sees EOF after

            // Wait up to 30s for graceful exit.
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(30);
            loop {
                if let Some(info) = self.registry.inspect(name) {
                    if info.state == VmState::Stopped || info.state == VmState::Crashed {
                        break;
                    }
                }
                if std::time::Instant::now() >= deadline {
                    eprintln!("stop_vm: graceful timeout for {}, hard-stopping", name);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }

        // Phase 2: hard-stop — set the AtomicBool flag. If the guest
        // already shut down gracefully, this is a no-op (thread
        // already exited). If the guest hung, this breaks the vcpu
        // loop at the next iteration.
        // Note: stop_vm can take arbitrary time if the VM's vcpu is
        // busy and KVM_RUN doesn't return until the next exit. For
        // v0 this is acceptable; ACPI interrupt injection is a future
        // improvement that would force a faster KVM_RUN return.
        if let Some(flag) = self.registry.shutdown_flag(&hash) {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        // Join the thread.
        if let Some(handle) = self.registry.take_thread(&hash) {
            let _ = handle.join();
        }

        self.registry.transition_state(&hash, VmState::Stopped);

        // Release the tap device if we allocated it.
        if let Some(tap) = self.registry.allocated_tap(&hash) {
            tap::release(&tap);
        }

        Ok(())
    }

    /// Snapshot of every registered VM.
    pub fn list_vms(&self) -> Vec<VmInfo> {
        self.registry.list()
    }

    /// Full spec + runtime info for a single VM.
    pub fn inspect_vm(&self, name: &str) -> Result<VmInfo, Error> {
        self.registry.inspect(name).ok_or(Error::NotFound)
    }
}

#[derive(Debug)]
pub enum Error {
    NotFound,
    AlreadyRunning,
    BucketCreate(String),
    Spawn(String),
    Ipc(String),
    Config(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "VM not found"),
            Self::AlreadyRunning => write!(f, "VM already running with that name"),
            Self::BucketCreate(e) => write!(f, "bucket create: {e}"),
            Self::Spawn(e) => write!(f, "spawn: {e}"),
            Self::Ipc(e) => write!(f, "ipc: {e}"),
            Self::Config(e) => write!(f, "config: {e}"),
        }
    }
}

impl std::error::Error for Error {}
