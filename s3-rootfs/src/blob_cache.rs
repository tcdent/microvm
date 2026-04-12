//! Golden blob cache — placeholder.
//!
//! Phase 2 lands `GoldenBlobCache` here: a flat
//! `Arc<HashMap<String, Arc<Vec<u8>>>>` populated once at orchestrator
//! init from the golden S3 bucket, immutable thereafter, shared
//! lock-free across every VM on the host.
//!
//! # History
//!
//! Previously held a multi-tier scaffold (`BlobCache` trait,
//! `RamBlobCache` with LFU+sampling eviction, `DiskBlobCache` stub,
//! `TieredBlobCache` composer — commit `586c815`). That machinery was
//! removed when `research/12` settled on pinning the entire bounded
//! golden blob pool in RAM: eviction has no job when the working set
//! is a known ~120 MB on a multi-GB host. The rotating-offset
//! probabilistic sampling pattern is preserved in git history at
//! `586c815` for any future workload that actually has memory
//! pressure on a content-addressed store.
