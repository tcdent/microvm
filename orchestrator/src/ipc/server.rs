//! Unix-socket JSON server (research/15 §3).
//!
//! Newline-delimited JSON. One command per line, one response per
//! line. Log streaming (`Logs { follow: true }`) breaks that pattern —
//! handled specially in phase 8.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::ipc::handler;
use crate::ipc::proto::{Command, Response};
use crate::Orchestrator;

/// Start the IPC server. Blocks until the shutdown flag is set.
pub fn serve(orch: Arc<Orchestrator>, shutdown: Arc<AtomicBool>) -> std::io::Result<()> {
    let path = &orch.config.socket_path;

    // Unlink stale socket from a previous run.
    let _ = std::fs::remove_file(path);

    let listener = UnixListener::bind(path)?;
    // Non-blocking so we can check the shutdown flag between accepts.
    listener.set_nonblocking(true)?;

    eprintln!("microd: listening on {}", path.display());

    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let orch = Arc::clone(&orch);
                std::thread::Builder::new()
                    .name("ipc-conn".into())
                    .spawn(move || handle_connection(orch, stream))
                    .ok();
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No pending connection — sleep briefly, re-check shutdown.
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("microd: accept error: {e}");
            }
        }
    }

    // Clean up socket file.
    let _ = std::fs::remove_file(path);
    eprintln!("microd: server shut down");
    Ok(())
}

fn handle_connection(orch: Arc<Orchestrator>, stream: std::os::unix::net::UnixStream) {
    // Set blocking for the per-connection handler.
    let _ = stream.set_nonblocking(false);
    let reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // client disconnected
        };
        if line.is_empty() {
            continue;
        }

        let cmd: Command = match serde_json::from_str(&line) {
            Ok(c) => c,
            Err(e) => {
                let resp = Response::Error(format!("parse error: {e}"));
                let _ = writeln!(writer, "{}", serde_json::to_string(&resp).unwrap());
                continue;
            }
        };

        let resp = handler::dispatch(&orch, cmd);
        let json = serde_json::to_string(&resp).unwrap();
        if writeln!(writer, "{}", json).is_err() {
            break; // client disconnected
        }
    }
}
