//! S-1 bearer-token auth. Token is either read from an env var
//! (`auth_token_env`) or generated once into `~/.tmuxlet/token` (0600). The
//! token file is deliberate: env delivery under launchd/systemd is this
//! project's documented failure mode, while clients already have an API-key
//! field — so `cat ~/.tmuxlet/token` is the whole client setup.

use crate::env::Env;
use std::fs;
use std::io::Read;
use std::path::Path;

/// Constant-time comparison so a mismatch's timing does not leak the token.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Validate an `Authorization` header value against the expected token.
pub fn check_bearer(header: Option<&str>, expected: &str) -> bool {
    match header.and_then(|h| h.strip_prefix("Bearer ")) {
        Some(tok) => ct_eq(tok.trim().as_bytes(), expected.as_bytes()),
        None => false,
    }
}

fn random_hex_32() -> std::io::Result<String> {
    let mut buf = [0u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Resolve the auth token per S-1. Returns `None` when auth is disabled.
/// When `token_env` is set, read that var from the captured env (it wins over
/// the file); otherwise read/generate the token file.
pub fn resolve(
    auth: bool,
    token_env: Option<&str>,
    env: &Env,
    token_file: &Path,
) -> Result<Option<String>, String> {
    if !auth {
        return Ok(None);
    }
    if let Some(name) = token_env {
        return match env.get(name) {
            Some(v) if !v.trim().is_empty() => Ok(Some(v.trim().to_string())),
            _ => Err(format!(
                "auth is on and auth_token_env = \"{name}\", but ${name} is empty or unset — export it before starting, or remove auth_token_env to use the {} file",
                token_file.display()
            )),
        };
    }
    // Reuse an existing token file so restarts don't invalidate clients.
    if let Ok(existing) = fs::read_to_string(token_file) {
        let t = existing.trim().to_string();
        if !t.is_empty() {
            // S-1: `.mode()` in write_token only secures files it *creates*, so a
            // token provisioned out-of-band (`echo tok > ~/.tmuxlet/token` under
            // umask 022) or restored from backup can be group/world-readable. The
            // reuse path never wrote it, so re-secure it here — forgiving-serve:
            // repair and warn rather than refuse a working token.
            repair_token_perms(token_file);
            return Ok(Some(t));
        }
    }
    let token = random_hex_32().map_err(|e| format!("could not generate an auth token: {e}"))?;
    if let Some(parent) = token_file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    write_token(token_file, &token)
        .map_err(|e| format!("could not write {}: {e}", token_file.display()))?;
    eprintln!(
        "[info] auth enabled: generated token at {} (mode 0600) — clients send `Authorization: Bearer $(cat {})`",
        token_file.display(),
        token_file.display()
    );
    Ok(Some(token))
}

/// S-1: force an existing token file back to 0600 if it is group/world-
/// accessible. Used on the reuse path, where `write_token`'s create-time mode
/// never applied.
#[cfg(unix)]
fn repair_token_perms(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(p) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 && fs::set_permissions(p, fs::Permissions::from_mode(0o600)).is_ok() {
            eprintln!(
                "[warn] auth: re-secured token file {} from mode {:o} to 0600 (was group/world-accessible)",
                p.display(),
                mode & 0o777
            );
        }
    }
}

#[cfg(not(unix))]
fn repair_token_perms(_p: &Path) {}

#[cfg(unix)]
fn write_token(p: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // Truncating a pre-existing file in place reuses its inode, so a process
    // holding an fd on the old world-readable file could read the freshly
    // generated secret. Remove it first, then create a NEW inode that is 0600
    // from birth — no in-place truncate, no default-umask window (S-1).
    let _ = fs::remove_file(p);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(p)?;
    f.write_all(token.as_bytes())
}

#[cfg(not(unix))]
fn write_token(p: &Path, token: &str) -> std::io::Result<()> {
    fs::write(p, token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_and_rejects() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
    }

    #[test]
    fn check_bearer_parses_header() {
        assert!(check_bearer(Some("Bearer tok123"), "tok123"));
        assert!(!check_bearer(Some("Bearer wrong"), "tok123"));
        assert!(!check_bearer(Some("tok123"), "tok123")); // missing scheme
        assert!(!check_bearer(None, "tok123"));
    }

    #[test]
    fn resolve_disabled_is_none() {
        let env = Env::capture("process", "", 5);
        let p = std::path::PathBuf::from("/nonexistent/token");
        assert_eq!(resolve(false, None, &env, &p).unwrap(), None);
    }

    #[test]
    fn resolve_env_var_wins() {
        // SAFETY: single-threaded test setup.
        unsafe { std::env::set_var("TEST_TMUXLET_TOKEN", "secret-xyz") };
        let env = Env::capture("process", "", 5);
        let p = std::path::PathBuf::from("/nonexistent/token");
        let t = resolve(true, Some("TEST_TMUXLET_TOKEN"), &env, &p).unwrap();
        assert_eq!(t, Some("secret-xyz".to_string()));
        unsafe { std::env::remove_var("TEST_TMUXLET_TOKEN") };
    }

    #[test]
    fn resolve_generates_and_reuses_token_file() {
        let dir = std::env::temp_dir().join(format!("tmuxlet-auth-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("token");
        let env = Env::capture("process", "", 5);
        let first = resolve(true, None, &env, &file).unwrap().unwrap();
        assert_eq!(first.len(), 64); // 32 bytes hex
        // second call reuses the same token
        let second = resolve(true, None, &env, &file).unwrap().unwrap();
        assert_eq!(first, second);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn resolve_repairs_perms_on_a_reused_nonempty_token_file() {
        // A1/S-1: a token provisioned out-of-band as world-readable is reused
        // verbatim (restarts must not invalidate clients) but re-secured to 0600.
        // This is the gap the old empty-file test masked — an empty file hits the
        // regenerate path, never the reuse path.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tmuxlet-auth-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("token");
        std::fs::write(&file, b"preexisting-token-abc\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let env = Env::capture("process", "", 5);
        let t = resolve(true, None, &env, &file).unwrap().unwrap();
        assert_eq!(t, "preexisting-token-abc", "reused token returned verbatim");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "reused token must be re-secured to 0600, got {mode:o}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn resolve_regenerates_empty_token_file_as_0600() {
        // A6/S-1: an empty stale file is regenerated on a fresh inode at 0600.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tmuxlet-auth-regen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("token");
        std::fs::write(&file, b"").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let env = Env::capture("process", "", 5);
        let t = resolve(true, None, &env, &file).unwrap().unwrap();
        assert_eq!(t.len(), 64);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "regenerated token must be 0600, got {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
