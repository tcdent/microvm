use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

static LOG: Mutex<Option<File>> = Mutex::new(None);

const LOG_PATH: &str = "/tmp/microvm.log";

/// Opens the log: `$VM_STATE/microvm.log` when the VM has a state directory, so each VM keeps
/// its own and a later launch cannot truncate a running one's; `/tmp/microvm.log` otherwise.
/// Appends, so a restart keeps the previous run's lines.
pub fn init() {
    let path = std::env::var("VM_STATE").map(|d| format!("{d}/microvm.log")).unwrap_or_else(|_| LOG_PATH.to_owned());
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).expect("Failed to create log file");
    *LOG.lock().unwrap() = Some(file);
}

/// UTC wall-clock time of day, to line log entries up with what a guest reports.
fn stamp() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let (secs, ms) = (now.as_secs(), now.subsec_millis());
    let days = secs / 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let t = secs % 86_400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z", t / 3600, t / 60 % 60, t % 60)
}

pub fn write(msg: &str) {
    if let Ok(mut guard) = LOG.lock() {
        if let Some(ref mut f) = *guard {
            writeln!(f, "{} {}", stamp(), msg).ok();
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
