//! VmRegistry: in-memory map of running VMs (research/15 §4, §7).
//!
//! Keyed by `name_hash = blake3(name)[..16]`. No disk persistence —
//! orchestrator restart wipes the registry. Every live VM also lives
//! here; the registry is the source of truth for "what's running".

use std::collections::HashMap;
use std::sync::Mutex;
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
    // TODO phase 5: shutdown channel, log file handle, etc.
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

    /// Store the thread handle on an existing entry. Called after
    /// spawn returns — the entry was inserted as Starting before
    /// the thread launched.
    pub fn set_thread(&self, name_hash: &str, handle: std::thread::JoinHandle<()>) {
        if let Some(entry) = self.entries.lock().unwrap().get_mut(name_hash) {
            entry.thread = Some(handle);
        }
    }
}
