//! Host side of sharedfs (see research/16-sharedfs-design.md).
//!
//! A project directory on the host is exposed to a guest kernel through
//! two channels: a read-only shared memory index holding every entry's
//! name, attributes and tree links, and a request queue for mutations
//! and file data. The host daemon in this crate is the only writer of
//! both the directory and the index.
//!
//! - [`proto`]: the memory layout and wire protocol the guest driver
//!   also implements. No std types in the layout structs.
//! - [`index`]: the shared index and the daemon's single-writer
//!   operations on it.
//! - [`server`]: executes requests against the host directory and keeps
//!   the index in step.
//! - [`device`]: the virtio queue worker that turns descriptor chains
//!   into [`Server`] calls. The VMM supplies the MMIO registers.
//! - `watch`: inotify on every exported directory, so changes made on
//!   the host reach the index through the same single writer.
//! - [`window`]: the data window, host address space the daemon maps file
//!   ranges into for the guest to read and write in place.

pub mod device;
pub mod index;
pub mod proto;
pub mod server;
mod watch;
pub mod window;

pub use index::{Attr, Index};
pub use proto::{Request, Response};
pub use server::Server;
pub use window::Window;
