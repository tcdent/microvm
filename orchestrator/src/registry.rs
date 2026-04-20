//! VmRegistry: in-memory map of running VMs (research/15 §4, §7).
//!
//! Keyed by `name_hash = blake3(name)[..16]`. No disk persistence —
//! orchestrator restart wipes the registry. Every live VM also lives
//! here; the registry is the source of truth for "what's running".

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::ipc::proto::{VmInfo, VmSpec, VmState};

pub struct VmRegistry {
    entries: Mutex<HashMap<String, VmEntry>>,
}

pub struct VmEntry {
    pub name_hash: String,
    pub spec: VmSpec,
    pub state: VmState,
    pub started_at: Instant,
    pub thread: Option<std::thread::JoinHandle<()>>,
    /// Shared with the VM's vcpu loop. Setting this to true causes
    /// the vcpu loop to exit at the next iteration.
    pub shutdown_flag: Arc<AtomicBool>,
    /// Tap device name allocated by tap::allocate. None if the user
    /// provided an explicit net_ifname (skip release on stop).
    pub allocated_tap: Option<String>,
}

impl VmRegistry {
    pub fn new() -> Self {
        Self { entries: Mutex::new(HashMap::new()) }
    }

    pub fn name_hash(name: &str) -> String {
        blake3::hash(name.as_bytes()).to_hex().to_string()[..16].to_string()
    }

    pub fn list(&self) -> Vec<VmInfo> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .map(VmInfo::from_entry)
            .collect()
    }

    pub fn inspect(&self, name: &str) -> Option<VmInfo> {
        let hash = Self::name_hash(name);
        self.entries
            .lock()
            .unwrap()
            .get(&hash)
            .map(VmInfo::from_entry)
    }

    /// Insert a new VM entry. Returns Err if a VM with the same
    /// name_hash is already registered and not Stopped/Crashed.
    pub fn insert(&self, entry: VmEntry) -> Result<(), crate::Error> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(existing) = entries.get(&entry.name_hash) {
            if existing.state != VmState::Stopped && existing.state != VmState::Crashed {
                return Err(crate::Error::AlreadyRunning);
            }
        }
        entries.insert(entry.name_hash.clone(), entry);
        Ok(())
    }

    /// Remove a VM entry by name_hash.
    pub fn remove(&self, name_hash: &str) {
        self.entries.lock().unwrap().remove(name_hash);
    }

    /// Transition a VM's state. No-op if the name_hash isn't found.
    pub fn transition_state(&self, name_hash: &str, new_state: VmState) {
        if let Some(entry) = self.entries.lock().unwrap().get_mut(name_hash) {
            entry.state = new_state;
        }
    }

    /// Store the thread handle on an existing entry.
    pub fn set_thread(&self, name_hash: &str, handle: std::thread::JoinHandle<()>) {
        if let Some(entry) = self.entries.lock().unwrap().get_mut(name_hash) {
            entry.thread = Some(handle);
        }
    }

    /// Get the shutdown flag for a VM. Returns None if not found.
    pub fn shutdown_flag(&self, name_hash: &str) -> Option<Arc<AtomicBool>> {
        self.entries.lock().unwrap()
            .get(name_hash)
            .map(|e| Arc::clone(&e.shutdown_flag))
    }

    /// Take the thread handle out for joining. Returns None if
    /// already taken or not found.
    pub fn take_thread(&self, name_hash: &str) -> Option<std::thread::JoinHandle<()>> {
        self.entries.lock().unwrap()
            .get_mut(name_hash)
            .and_then(|e| e.thread.take())
    }

    /// Get the allocated tap name for cleanup on stop.
    pub fn allocated_tap(&self, name_hash: &str) -> Option<String> {
        self.entries.lock().unwrap()
            .get(name_hash)
            .and_then(|e| e.allocated_tap.clone())
    }
}
