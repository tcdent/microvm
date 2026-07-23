//! IPC: Unix socket server + JSON command dispatch (research/15 §3).
//!
//! Transport-agnostic surface lives in `proto`; the dispatch layer
//! in `handler` maps commands to [`crate::Orchestrator`] methods.
//! `server` is the Unix-socket accept loop; the future HTTP adapter
//! lives alongside it and reuses `handler`.

pub mod proto;
pub mod handler;
pub mod server;
