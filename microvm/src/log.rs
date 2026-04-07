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
