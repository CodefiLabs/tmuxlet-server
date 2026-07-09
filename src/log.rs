//! U-8: minimal leveled logger (no new deps). Emits `ts level msg` to stderr,
//! honoring `server.log_level`. Request correlation via a monotonic reqid.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static LEVEL: AtomicU8 = AtomicU8::new(2); // info
static REQ_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn set_level(l: &str) {
    let v = match l.to_ascii_lowercase().as_str() {
        "error" => 0,
        "warn" => 1,
        "info" => 2,
        "debug" => 3,
        _ => 2,
    };
    LEVEL.store(v, Ordering::Relaxed);
}

fn enabled(level: u8) -> bool {
    level <= LEVEL.load(Ordering::Relaxed)
}

fn ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn line(level: u8, tag: &str, msg: &str) {
    if enabled(level) {
        eprintln!("{} {tag} {msg}", ts());
    }
}

pub fn error(msg: &str) {
    line(0, "error", msg);
}
pub fn warn(msg: &str) {
    line(1, "warn", msg);
}
pub fn info(msg: &str) {
    line(2, "info", msg);
}
pub fn debug(msg: &str) {
    line(3, "debug", msg);
}

/// Short request id for log correlation (U-8).
pub fn next_reqid() -> String {
    let n = REQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{n:06x}")
}
