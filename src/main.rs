mod backend;
mod config;
mod env;
mod http;
mod http_client;
mod openai;
mod pty;
mod router;

use backend::Backend;
use config::Config;
use env::Env;
use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::ExitCode;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tiny_http::Server;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_CONFIG: &str = "~/.tmuxlet/server.toml";
const DEFAULT_LISTEN: &str = "127.0.0.1:3456";

struct Args {
    config: String,
    validate: bool,
    check_backends: bool,
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args {
        config: DEFAULT_CONFIG.to_string(),
        validate: false,
        check_backends: false,
    };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--version" | "-V" => {
                println!("tmuxlet-server {VERSION}");
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            "--validate" => args.validate = true,
            "--check-backends" => args.check_backends = true,
            "--config" => {
                i += 1;
                match argv.get(i) {
                    Some(p) => args.config = p.clone(),
                    None => {
                        eprintln!("tmuxlet-server: --config requires a path");
                        return ExitCode::FAILURE;
                    }
                }
            }
            other => {
                if let Some(p) = other.strip_prefix("--config=") {
                    args.config = p.to_string();
                } else {
                    eprintln!("tmuxlet-server: unknown argument: {other}");
                    return ExitCode::FAILURE;
                }
            }
        }
        i += 1;
    }

    if args.validate || args.check_backends {
        return run_validate(&args);
    }

    run_serve(&args.config)
}

/// Capture the backend environment per config (S-9 timeout-bounded).
fn capture_env(cfg: &Config) -> Env {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    Env::capture(
        &cfg.server.env_source,
        &shell,
        cfg.server.env_capture_timeout_secs,
    )
}

/// `--validate` (strict: lint warnings are errors) and/or `--check-backends`
/// (probe cli/tmuxlet/api reachability). Both may be requested together.
fn run_validate(args: &Args) -> ExitCode {
    let path = config::tilde_path(&args.config);
    let (cfg, warnings) = match config::load(&path) {
        Ok(cw) => cw,
        Err(e) => {
            eprintln!("tmuxlet-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut failed = false;
    if args.validate {
        if warnings.is_empty() {
            println!(
                "OK ({} backends, {} chains)",
                cfg.backends.len(),
                cfg.chains.len()
            );
        } else {
            for w in &warnings {
                eprintln!("tmuxlet-server: {w}");
            }
            failed = true;
        }
    }
    if args.check_backends {
        let env = capture_env(&cfg);
        if !check_backends(&cfg, &env) {
            failed = true;
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn backend_type(b: &config::Backend) -> &'static str {
    match b {
        config::Backend::Tmuxlet { .. } => "tmuxlet",
        config::Backend::Api { .. } => "api",
        config::Backend::Cli { .. } => "cli",
    }
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
    }
}

/// U-5 `--check-backends`: per backend, print PASS/FAIL and a detail line.
/// Returns true iff every backend passed.
fn check_backends(cfg: &Config, env: &Env) -> bool {
    let mut all_ok = true;
    let mut names: Vec<&String> = cfg.backends.keys().collect();
    names.sort();
    for name in names {
        let b = &cfg.backends[name];
        let (ok, detail) = match b {
            config::Backend::Cli { bin, .. } => {
                let path = std::path::PathBuf::from(bin);
                if is_executable(&path) {
                    (true, path.display().to_string())
                } else {
                    (
                        false,
                        format!("{} not found or not executable", path.display()),
                    )
                }
            }
            config::Backend::Tmuxlet { .. } => {
                let resolved = backend::resolve_program("tmuxlet", env);
                match Command::new(&resolved)
                    .arg("--version")
                    .env_clear()
                    .envs(env.as_pairs())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                {
                    Ok(s) if s.success() => (true, resolved.display().to_string()),
                    Ok(s) => (
                        false,
                        format!("{} exited {}", resolved.display(), s.code().unwrap_or(-1)),
                    ),
                    Err(e) => (false, format!("tmuxlet not runnable ({e})")),
                }
            }
            config::Backend::Api { base_url, .. } => {
                let (_scheme, host, port, _path) = backend::api::split_url(base_url);
                let addr = format!("{host}:{port}");
                match addr.to_socket_addrs().ok().and_then(|mut it| it.next()) {
                    Some(sa) => match TcpStream::connect_timeout(&sa, Duration::from_secs(3)) {
                        Ok(_) => (true, format!("connected {addr}")),
                        Err(e) => (false, format!("cannot connect {addr} ({e})")),
                    },
                    None => (false, format!("cannot resolve {addr}")),
                }
            }
        };
        println!(
            "{} {name} ({}) — {detail}",
            if ok { "PASS" } else { "FAIL" },
            backend_type(b)
        );
        if !ok {
            all_ok = false;
        }
    }
    all_ok
}

fn run_serve(config_path: &str) -> ExitCode {
    // 1. rustls ring CryptoProvider — REQUIRED before any TLS use (api backend).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 2. Load + validate config. Lint warnings are non-fatal at serve time
    //    (UX invariant 2: an upgrade must not brick a running service).
    let path = config::tilde_path(config_path);
    let (cfg, warnings) = match config::load(&path) {
        Ok(cw) => cw,
        Err(e) => {
            eprintln!("tmuxlet-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    for w in &warnings {
        eprintln!("[warn] {w}");
    }

    // 3. Capture environment for backends (S-9 timeout-bounded).
    let environment = capture_env(&cfg);
    for b in cfg.backends.values() {
        if let config::Backend::Api {
            api_key_env: Some(k),
            ..
        } = b
            && environment.get(k).is_none()
        {
            eprintln!("[warn] api_key_env {k} is unset");
        }
    }

    // 4. Build the runtime backends once (P-3), resolving tmuxlet's path from
    //    the captured env PATH (F-4).
    let backends: HashMap<String, Backend> = cfg
        .backends
        .iter()
        .map(|(name, b)| (name.clone(), Backend::from_config(name, b, &environment)))
        .collect();

    // 5. Bind.
    let listen = cfg.server.listen.clone();
    let server = match Server::http(&listen) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("tmuxlet-server: failed to bind {listen}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "tmuxlet-server {VERSION} listening on http://{listen} ({} backends, {} chains)",
        cfg.backends.len(),
        cfg.chains.len()
    );

    // 6. Signal handler → unblock workers for graceful shutdown.
    {
        let server = Arc::clone(&server);
        let _ = ctrlc::set_handler(move || {
            eprintln!("shutting down...");
            server.unblock();
        });
    }

    let state = Arc::new(http::State {
        cfg,
        env: environment,
        backends,
    });
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    http::serve(server, state, workers);
    ExitCode::SUCCESS
}

fn print_help() {
    println!(
        "tmuxlet-server {VERSION}\n\nUsage:\n  tmuxlet-server [--config FILE]    Start the server (default config: {DEFAULT_CONFIG})\n  tmuxlet-server --validate         Validate config (strict) and exit\n  tmuxlet-server --check-backends   Probe each backend's reachability and exit\n  tmuxlet-server --version          Print version\n  tmuxlet-server --help             Print this help\n\nConfig is TOML at {DEFAULT_CONFIG} (override with --config FILE or --config=FILE).\nA fresh config listens on {DEFAULT_LISTEN}."
    );
}
