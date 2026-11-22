//! microvm library — KVM-based VMM for coding-agent workloads.
//!
//! Re-exports the pieces an orchestrator needs to launch VMs
//! in-process as library calls. Internal modules (cpu, vm, memory,
//! serial, virtio_*) stay crate-private.
//!
//! # Usage
//!
//! ```no_run
//! use microvm::launcher::{LaunchConfig, run, launch_vm, prepare_filesystem};
//! use s3_rootfs::config::FsConfig;
//!
//! let cfg = LaunchConfig {
//!     kernel_path: "dist/vmlinux".into(),
//!     initrd_path: "dist/initramfs.cpio.gz".into(),
//!     net_ifname: Some("microvm0".into()),
//!     mem_mb: 1024,
//!     vm_id: "vm-001".into(),
//!     fs: FsConfig::default(),
//! };
//!
//! // Option A: full lifecycle (prepare_filesystem + launch_vm)
//! run(cfg.clone()).unwrap();
//!
//! // Option B: split lifecycle (orchestrator holds the Arc<S3Fs>)
//! let fs = prepare_filesystem(&cfg);
//! launch_vm(cfg, fs).unwrap();
//! ```

#[macro_use]
pub mod log;

pub(crate) mod cpu;
pub(crate) mod event_loop;
pub(crate) mod kernel;
pub mod launcher;
pub(crate) mod memory;
pub(crate) mod mptable;
pub(crate) mod serial;
pub(crate) mod virtio_console;
pub(crate) mod virtio_fs;
pub(crate) mod virtio_blk;
pub(crate) mod virtio_mmio;
pub(crate) mod virtio_net;
pub(crate) mod vm;
