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
    if !enabled(level) {
        return;
    }
    // A4: eprintln! panics on EPIPE (a closed stderr) and blocks on a stalled
    // pipe; a panic here — often under the health lock — would poison that mutex
    // and brick every later request. Write to a locked handle and swallow errors.
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "{} {tag} {}", ts(), sanitize(msg));
}

/// A5 (CWE-117): strip control characters so client-controlled strings (model
/// names, upstream error bodies) can't forge a second log line via `\n` or drive
/// the operator's terminal via ANSI escapes.
fn sanitize(msg: &str) -> String {
    msg.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_control_and_ansi() {
        // A5: a model name carrying a newline + ANSI escape must collapse to one
        // clean line — no forged log entry, no terminal control.
        let dirty = "claude-3\nInjected: fake\x1b[31m red\ttab\x07bell";
        let clean = sanitize(dirty);
        assert!(
            !clean.chars().any(|c| c.is_control()),
            "no control bytes must remain: {clean:?}"
        );
        assert!(!clean.contains('\n'));
        assert!(!clean.contains('\x1b'));
        assert!(clean.contains("claude-3"));
        assert!(clean.contains("Injected: fake"));
    }
}
