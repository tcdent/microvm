//! microvm: a minimal KVM-based VMM for coding-agent workloads.
//!
//! The interesting code lives in `launcher.rs`. This file is just:
//!   1. set process name / log hook
//!   2. pull config from env (this binary's config source)
//!   3. put stdin in raw mode (interactive virtio-console)
//!   4. hand off to `launcher::run`
//!   5. restore stdin on exit
//!
//! An orchestrator binary would replace steps 2+3 with its own config
//! source (IPC, JSON, CLI flags) and terminal handling, and reuse
//! `launcher::run` unchanged.

#[macro_use]
mod log;

mod cpu;
mod event_loop;
mod kernel;
mod launcher;
mod memory;
mod mptable;
mod serial;
mod virtio_console;
mod virtio_fs;
mod virtio_mmio;
mod virtio_net;
mod vm;

use std::time::Duration;

use launcher::LaunchConfig;
use s3_rootfs::config::FsConfig;

fn main() {
    // Process name visible in top/htop/ps — otherwise this shows
    // up as "target/release/microvm" which is unhelpful.
    let name = std::ffi::CString::new("microvm").unwrap();
    unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr()); }

    log::init();
    s3_rootfs::set_log_fn(|msg| crate::log::write(msg));

    let cfg = config_from_env();
    log!(
        "Config: vm_id={} memory={}MB kernel={} initrd={} net={:?} s3={} read_ahead={}B cache_ttl={}s",
        cfg.vm_id,
        cfg.mem_mb,
        cfg.kernel_path,
        cfg.initrd_path,
        cfg.net_ifname,
        cfg.fs.s3.endpoint,
        cfg.fs.read_ahead_size,
        cfg.fs.cache_ttl.as_secs(),
    );

    let orig_term = setup_raw_terminal();

    if let Err(e) = launcher::run(cfg) {
        eprintln!("Error: {e}");
    }

    if let Some(ref orig) = orig_term {
        restore_terminal(orig);
    }
}

// ── Config: env vars → LaunchConfig ────────────────────────────────────
//
// Lives in `main.rs` rather than `launcher.rs` because env-var parsing
// is *this binary's* config source. An orchestrator binary wouldn't
// use env vars at all; it'd build LaunchConfig from IPC/JSON/CLI.
// Keeping the env translation here keeps the library source-agnostic.

fn config_from_env() -> LaunchConfig {
    let mut fs = FsConfig::default();
    fs.s3.endpoint = env_or("VM_S3_ENDPOINT", "http://192.168.88.15:9090");
    fs.read_ahead_size = env_or("VM_READ_AHEAD", "16")
        .parse::<usize>()
        .unwrap_or(16)
        * 1024
        * 1024;
    fs.cache_ttl = Duration::from_secs(env_or("VM_CACHE_TTL", "300").parse().unwrap_or(300));

    LaunchConfig {
        kernel_path: env_or("VM_KERNEL", "dist/vmlinux"),
        initrd_path: env_or("VM_INITRD", "dist/initramfs.cpio.gz"),
        net_ifname: std::env::var("VM_NET").ok(),
        mem_mb: env_or("VM_MEMORY", "1024").parse().unwrap_or(1024),
        vm_id: env_or("VM_ID", "default"),
        fs,
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

// ── Terminal handling ──────────────────────────────────────────────────
//
// Lives here rather than in launcher.rs because it's a `main`-binary
// concern: only the interactive `microvm` process touches stdin as a
// TTY. An orchestrator binary wouldn't run these — it has its own
// stdio handling.

fn setup_raw_terminal() -> Option<libc::termios> {
    unsafe {
        let mut orig: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
            return None;
        }
        let mut raw = orig;
        libc::cfmakeraw(&mut raw);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        Some(orig)
    }
}

fn restore_terminal(orig: &libc::termios) {
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, orig);
    }
}
