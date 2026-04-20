//! microd — the orchestrator daemon (research/15).
//!
//! One process, many VMs-as-threads, shared state via Arc.
//! Listens on a Unix socket for commands from microctl.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use orchestrator::{DaemonConfig, Orchestrator};
use orchestrator::ipc::server;

fn main() {
    let config = match DaemonConfig::from_args() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("microd: config error: {e}");
            std::process::exit(1);
        }
    };

    let orch = match Orchestrator::new(config) {
        Ok(o) => Arc::new(o),
        Err(e) => {
            eprintln!("microd: init error: {e}");
            std::process::exit(1);
        }
    };

    // Shutdown flag — checked by the IPC accept loop.
    let shutdown = Arc::new(AtomicBool::new(false));

    // SIGTERM/SIGINT handler: set the flag.
    {
        let flag = Arc::clone(&shutdown);
        let _ = unsafe {
            libc::signal(libc::SIGTERM, signal_handler as libc::sighandler_t);
            libc::signal(libc::SIGINT, signal_handler as libc::sighandler_t);
        };
        // Store the flag in a static for the signal handler.
        SHUTDOWN_FLAG.store(Box::into_raw(Box::new(flag)) as usize, Ordering::SeqCst);
    }

    eprintln!("microd: starting");
    if let Err(e) = server::serve(orch, shutdown) {
        eprintln!("microd: server error: {e}");
        std::process::exit(1);
    }
}

// Signal handler: set the shutdown flag. Crude but sufficient for v0.
static SHUTDOWN_FLAG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

extern "C" fn signal_handler(_sig: libc::c_int) {
    let ptr = SHUTDOWN_FLAG.load(Ordering::SeqCst);
    if ptr != 0 {
        let flag = unsafe { &*(ptr as *const Arc<AtomicBool>) };
        flag.store(true, Ordering::Relaxed);
    }
}
