//! Per-VM log file management (research/15 §6).
//!
//! Serial console output → `<log_dir>/<name_hash>/serial.log`, append
//! only. `microctl logs --follow` reads + tails this file via IPC.
//!
//! Log rotation is out of v0 scope. SSH access (Travis's note) is a
//! later sprint.

use std::path::PathBuf;

pub fn log_path(log_dir: &PathBuf, name_hash: &str) -> PathBuf {
    log_dir.join(name_hash).join("serial.log")
}

/// Read the entire log file for a VM. Returns empty string if
/// the file doesn't exist yet (VM hasn't produced output).
pub fn read_log(log_dir: &PathBuf, name: &str) -> String {
    let hash = crate::registry::VmRegistry::name_hash(name);
    let path = log_path(log_dir, &hash);
    std::fs::read_to_string(&path).unwrap_or_default()
}
