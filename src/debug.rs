// debug.rs
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

static DEBUG_LOGGER: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

/// Call this once early in main()
pub fn init_debug_logger() {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("nova_debug.log")
        .expect("Failed to open debug log file");

    let _ = DEBUG_LOGGER.set(Mutex::new(file));
}

/// Log a message (appends to nova_debug.log)
pub fn debug_log(msg: &str) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let line = format!("[{}] {}\n", timestamp, msg);

    if let Some(logger) = DEBUG_LOGGER.get() {
        if let Ok(mut file) = logger.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    } else {
        // Fallback during early startup
        println!("{}", line.trim());
    }
}