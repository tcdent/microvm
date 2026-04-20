//! Wire protocol types (research/15 §3).
//!
//! Serde-serialized as JSON on the Unix socket. The same types back
//! the future HTTP adapter — same commands, same responses, just
//! different transport.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::registry::VmEntry;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Start(VmSpec),
    Stop { name: String },
    List,
    Inspect { name: String },
    Logs { name: String, follow: bool },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "resp", content = "data", rename_all = "snake_case")]
pub enum Response {
    // Adjacent tagging (`tag` + `content`) rather than internal tagging
    // because `VmList(Vec<VmInfo>)` and `LogChunk(String)` / `Error(String)`
    // are newtype variants over non-map types. Internally-tagged enums
    // can't cleanly merge those into the tag object — serialization
    // produces malformed JSON (or the Vec disappears), which then
    // fails to round-trip on the client side as an EOF parse error.
    Ok,
    VmInfo(VmInfo),
    VmList(Vec<VmInfo>),
    LogChunk(String),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSpec {
    pub name: String,
    pub mem_mb: u32,
    pub kernel: Option<PathBuf>,
    pub initrd: Option<PathBuf>,
    pub bucket: Option<String>,
    pub golden_bucket: Option<String>,
    pub ruleset: Option<PathBuf>,
    pub ephemeral_dir: Option<PathBuf>,
    pub net_ifname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmInfo {
    pub name: String,
    pub name_hash: String,
    pub state: VmState,
    pub spec: VmSpec,
    pub uptime_secs: u64,
}

impl VmInfo {
    pub fn from_entry(entry: &VmEntry) -> Self {
        Self {
            name: entry.spec.name.clone(),
            name_hash: entry.name_hash.clone(),
            state: entry.state,
            spec: entry.spec.clone(),
            uptime_secs: entry.started_at.elapsed().as_secs(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    Starting,
    Running,
    Stopping,
    Stopped,
    Crashed,
}
