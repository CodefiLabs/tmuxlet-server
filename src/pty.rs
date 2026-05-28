use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn stringify<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Run `program args` in a PTY, write `prompt` to its stdin, and capture all
/// output until the child exits or `timeout` elapses (then it is killed).
/// Fully synchronous (plain std threads). macOS EIO-on-slave-close is treated
/// as EOF. Returns the raw captured bytes (clean with `clean_output`).
pub fn run_in_pty(
    program: &str,
    args: &[String],
    env: &[(&str, &str)],
    cwd: &str,
    prompt: &str,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(stringify)?;

    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.cwd(cwd);

    let mut reader = pair.master.try_clone_reader().map_err(stringify)?;
    let mut writer = pair.master.take_writer().map_err(stringify)?;
    let mut child = pair.slave.spawn_command(cmd).map_err(stringify)?;
    drop(pair.slave); // REQUIRED: else the master reader never sees EOF.

    let mut killer = child.clone_killer();

    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let reader_handle = thread::spawn(move || {
        let mut collected = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => collected.extend_from_slice(&buf[..n]),
                Err(e) => {
                    // macOS returns EIO (raw os error 5) when the slave closes.
                    let _ = e.raw_os_error();
                    break;
                }
            }
        }
        let _ = out_tx.send(collected);
    });

    // Arm the watchdog BEFORE writing the prompt: writing to the PTY master can
    // block if the child (e.g. a TUI that doesn't drain stdin) lets the tty
    // input buffer fill. If that write stalls past the timeout, the watchdog
    // kills the child, which closes the PTY and unblocks the write.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = thread::spawn(move || match done_rx.recv_timeout(timeout) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = killer.kill();
        }
    });

    let _ = write!(writer, "{prompt}");
    let _ = writer.flush();
    drop(writer); // send EOF on the child's stdin.

    let _ = child.wait().map_err(stringify)?;
    let _ = done_tx.send(());
    let _ = watchdog.join();
    let _ = reader_handle.join();
    let output = out_rx.recv().unwrap_or_default();
    drop(pair.master);
    Ok(output)
}

/// Strip ANSI/CSI/OSC escape sequences, carriage returns, and C0 control bytes
/// while preserving `\n` and `\t`. Dependency-free state machine.
pub fn clean_output(raw: &[u8]) -> String {
    enum St {
        Normal,
        Esc,
        Csi,
        Osc,
    }
    let mut state = St::Normal;
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    for &b in raw {
        match state {
            St::Normal => match b {
                0x1b => state = St::Esc,
                b'\r' => {}
                b'\n' | b'\t' => out.push(b),
                0x20..=0x7e => out.push(b),
                0x80.. => out.push(b),
                _ => {}
            },
            St::Esc => match b {
                b'[' => state = St::Csi,
                b']' => state = St::Osc,
                _ => state = St::Normal,
            },
            St::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    state = St::Normal;
                }
            }
            St::Osc => match b {
                0x07 => state = St::Normal,
                0x1b => state = St::Esc,
                _ => {}
            },
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_cr_and_control_keeps_newline_tab() {
        let raw = b"\x1b[1;32mHELLO\x1b[0m\r\nline2\ttab\x07\n";
        assert_eq!(clean_output(raw), "HELLO\nline2\ttab\n");
    }

    #[test]
    fn strips_osc_sequences() {
        let raw = b"\x1b]0;title\x07visible";
        assert_eq!(clean_output(raw), "visible");
    }
}
