use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

static LOG: Mutex<Option<File>> = Mutex::new(None);

const LOG_PATH: &str = "/tmp/microvm.log";

pub fn init() {
    let file = File::create(LOG_PATH).expect("Failed to create log file");
    *LOG.lock().unwrap() = Some(file);
}

pub fn write(msg: &str) {
    if let Ok(mut guard) = LOG.lock() {
        if let Some(ref mut f) = *guard {
            writeln!(f, "{}", msg).ok();
            f.flush().ok();
        }
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::log::write(&format!($($arg)*))
    };
}

/// Debug log — compiles to nothing in release builds.
/// Use for hot-path tracing (FUSE ops, MMIO, packet I/O).
/// Keep `log!` for lifecycle events (init, shutdown, errors).
#[cfg(debug_assertions)]
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::log::write(&format!($($arg)*))
    };
}

#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {};
}
