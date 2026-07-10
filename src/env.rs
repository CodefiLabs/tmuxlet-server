use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Env(Arc<HashMap<String, String>>);

impl Env {
    /// `source` is "shell" (run `$SHELL -ilc env`) or "process" (inherit current
    /// env). `timeout_secs` bounds the shell capture (S-9); 0 means no watchdog.
    pub fn capture(source: &str, shell: &str, timeout_secs: u64) -> Env {
        let map: HashMap<String, String> = match source {
            "process" => std::env::vars().collect(),
            _ => match capture_shell(shell, Duration::from_secs(timeout_secs)) {
                Ok(m) => m,
                Err(e) => {
                    // U-8a / S-9: the fallback must not be silent, and the
                    // message names the one-line fix (UX invariant 3).
                    eprintln!(
                        "[warn] shell env capture failed ({e}); using process env instead — set env_source = \"process\" to make this the intended path"
                    );
                    std::env::vars().collect()
                }
            },
        };
        Env(Arc::new(map))
    }

    /// Test-only constructor from an explicit map (e.g. a crafted PATH).
    #[cfg(test)]
    pub(crate) fn from_map(map: HashMap<String, String>) -> Env {
        Env(Arc::new(map))
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(String::as_str)
    }

    pub fn as_pairs(&self) -> Vec<(&str, &str)> {
        self.0
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    /// S-3: env pairs filtered to an allowlist of glob patterns, plus a minimal
    /// always-passed base set (PATH/HOME/SHELL/TERM/LANG) so a tight allowlist
    /// can't produce the "works in my terminal, fails in the server" class of
    /// bug. `None` means no filtering (current behavior).
    pub fn filtered_pairs(&self, allow: Option<&[String]>) -> Vec<(&str, &str)> {
        match allow {
            None => self.as_pairs(),
            Some(patterns) => {
                const BASE: &[&str] = &["PATH", "HOME", "SHELL", "TERM", "LANG"];
                self.0
                    .iter()
                    .filter(|(k, _)| {
                        BASE.contains(&k.as_str()) || patterns.iter().any(|p| glob_match(p, k))
                    })
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect()
            }
        }
    }
}

/// Minimal glob match supporting `*` wildcards (e.g. `ANTHROPIC_*`, `*_TOKEN`,
/// `AWS_*_KEY`). No character classes — just literal segments split on `*`.
fn glob_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // leading anchor
            if !text[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == parts.len() - 1 {
            // trailing anchor
            return text[pos..].ends_with(part);
        } else {
            match text[pos..].find(part) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }
    true
}

fn capture_shell(shell: &str, timeout: Duration) -> std::io::Result<HashMap<String, String>> {
    // -i interactive (sources ~/.zshrc), -l login, -c env. Deviation #3:
    // interactive is required so ~/.zshrc-exported PATH entries (e.g. agy) appear.
    //
    // S-7/F-7: prefer `env -0` (NUL-delimited) so multi-line values (e.g. a
    // shell function exported into the env) cannot corrupt line-splitting. If
    // `env -0` is unsupported, the `|| command env` fallback yields line output,
    // which we detect by the absence of NUL bytes.
    let mut child = Command::new(shell)
        .args(["-ilc", "command env -0 2>/dev/null || command env"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    // Drain stdout on a thread so a large env can't deadlock against a full pipe.
    let mut out = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    // S-9: watchdog. A blocking ~/.zshrc must not hang startup forever.
    let deadline = if timeout.is_zero() {
        None
    } else {
        Some(Instant::now() + timeout)
    };
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None => {
                if let Some(d) = deadline
                    && Instant::now() >= d
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("exceeded {}s watchdog", timeout.as_secs()),
                    ));
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    let bytes = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let map = if text.contains('\0') {
        parse_env_nul(&text)
    } else {
        parse_env(&text)
    };
    Ok(map)
}

fn is_valid_key(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse NUL-delimited `env -0` output. Each record is `KEY=VALUE` and VALUE may
/// contain newlines (S-7).
fn parse_env_nul(raw: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for record in raw.split('\0') {
        if record.is_empty() {
            continue;
        }
        if let Some((k, v)) = record.split_once('=')
            && is_valid_key(k)
        {
            m.insert(k.to_string(), v.to_string());
        }
    }
    m
}

pub fn parse_env(raw: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in raw.lines() {
        if let Some((k, v)) = line.split_once('=')
            && is_valid_key(k)
        {
            m.insert(k.to_string(), v.to_string());
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keyvalue_skips_noise() {
        let raw = "PATH=/usr/bin:/bin\nHOME=/Users/kk\n\x1b]7;file://x\x1b\\garbage line\nVAR_2=a=b=c\nbad key=nope\n";
        let m = parse_env(raw);
        assert_eq!(m.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
        assert_eq!(m.get("VAR_2").map(String::as_str), Some("a=b=c"));
        assert!(!m.contains_key("garbage line"));
        assert!(!m.contains_key("bad key"));
    }

    #[test]
    fn nul_parse_keeps_multiline_values() {
        // A multi-line value would corrupt line-splitting; NUL-splitting keeps it.
        let raw = "FUNC=line1\nline2\nline3\0PATH=/usr/bin\0";
        let m = parse_env_nul(raw);
        assert_eq!(
            m.get("FUNC").map(String::as_str),
            Some("line1\nline2\nline3")
        );
        assert_eq!(m.get("PATH").map(String::as_str), Some("/usr/bin"));
    }

    #[test]
    fn process_capture_has_path() {
        let env = Env::capture("process", "", 5);
        assert!(env.get("PATH").is_some() || env.get("HOME").is_some());
    }

    #[test]
    fn glob_matches_prefix_suffix_and_infix() {
        assert!(glob_match("ANTHROPIC_*", "ANTHROPIC_API_KEY"));
        assert!(!glob_match("ANTHROPIC_*", "OPENAI_API_KEY"));
        assert!(glob_match("*_TOKEN", "GH_TOKEN"));
        assert!(!glob_match("*_TOKEN", "TOKEN_X"));
        assert!(glob_match("AWS_*_KEY", "AWS_SECRET_KEY"));
        assert!(!glob_match("AWS_*_KEY", "AWS_SECRET"));
        assert!(glob_match("PATH", "PATH"));
        assert!(!glob_match("PATH", "PATHX"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn filtered_pairs_keeps_base_and_allowlisted() {
        let mut m = std::collections::HashMap::new();
        m.insert("PATH".to_string(), "/bin".to_string());
        m.insert("ANTHROPIC_API_KEY".to_string(), "sk".to_string());
        m.insert("AWS_SECRET_ACCESS_KEY".to_string(), "leak".to_string());
        let env = Env(std::sync::Arc::new(m));
        let allow = vec!["ANTHROPIC_*".to_string()];
        let pairs: std::collections::HashMap<_, _> =
            env.filtered_pairs(Some(&allow)).into_iter().collect();
        assert!(pairs.contains_key("PATH")); // base set always passed
        assert!(pairs.contains_key("ANTHROPIC_API_KEY")); // allowlisted
        assert!(!pairs.contains_key("AWS_SECRET_ACCESS_KEY")); // filtered out
        // None = no filtering
        assert_eq!(env.filtered_pairs(None).len(), 3);
    }
}
