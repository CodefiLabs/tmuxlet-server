//! CLI flag handling: --version, --help, --validate (ok + fail), bad args.
use std::io::Write;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tmuxlet-server"))
}

fn write_config(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    (dir, path)
}

const GOOD: &str = r#"
[server]
listen = "127.0.0.1:1"
default_chain = "d"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.d]
order = ["echo"]
"#;

#[test]
fn version_flag_prints_and_exits_zero() {
    let out = bin().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("tmuxlet-server"));
}

#[test]
fn help_flag_exits_zero() {
    let out = bin().arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Usage"));
}

#[test]
fn validate_ok_on_good_config() {
    let (_dir, cfg) = write_config(GOOD);
    let out = bin()
        .arg("--validate")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("OK"));
}

#[test]
fn validate_fails_on_chain_referencing_unknown_backend() {
    let (_dir, cfg) = write_config(
        r#"
[server]
listen = "127.0.0.1:1"
default_chain = "d"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.d]
order = ["nope"]
"#,
    );
    let out = bin()
        .arg("--validate")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected validation failure");
}

#[test]
fn unknown_arg_exits_nonzero() {
    let out = bin().arg("--bogus").output().unwrap();
    assert!(!out.status.success());
}
