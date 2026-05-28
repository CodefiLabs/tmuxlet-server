use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::Arc;

#[derive(Clone)]
pub struct Env(Arc<HashMap<String, String>>);

impl Env {
    /// `source` is "shell" (run `$SHELL -ilc env`) or "process" (inherit current env).
    pub fn capture(source: &str, shell: &str) -> Env {
        let map: HashMap<String, String> = match source {
            "process" => std::env::vars().collect(),
            _ => capture_shell(shell).unwrap_or_else(|_| std::env::vars().collect()),
        };
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
}

fn capture_shell(shell: &str) -> std::io::Result<HashMap<String, String>> {
    // -i interactive (sources ~/.zshrc), -l login, -c env. Deviation #3:
    // interactive is required so ~/.zshrc-exported PATH entries (e.g. agy) appear.
    let out = Command::new(shell)
        .args(["-ilc", "env"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    Ok(parse_env(&String::from_utf8_lossy(&out.stdout)))
}

fn is_valid_key(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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
}
