mod backend;
mod config;
mod env;
mod http;
mod http_client;
mod openai;
mod pty;
mod router;

use std::process::ExitCode;
use std::sync::Arc;
use tiny_http::Server;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_CONFIG: &str = "~/.tmuxlet/server.toml";

struct Args {
    config: String,
    validate: bool,
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args {
        config: DEFAULT_CONFIG.to_string(),
        validate: false,
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
                eprintln!("tmuxlet-server: unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
        i += 1;
    }

    if args.validate {
        let path = config::tilde_path(&args.config);
        return match config::load(&path) {
            Ok(cfg) => {
                println!(
                    "OK ({} backends, {} chains)",
                    cfg.backends.len(),
                    cfg.chains.len()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("tmuxlet-server: {e}");
                ExitCode::FAILURE
            }
        };
    }

    run_serve(&args.config)
}

fn run_serve(config_path: &str) -> ExitCode {
    // 1. rustls ring CryptoProvider — REQUIRED before any TLS use (api backend).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 2. Load + validate config.
    let path = config::tilde_path(config_path);
    let cfg = match config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tmuxlet-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    // 3. Capture environment for backends.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let environment = env::Env::capture(&cfg.server.env_source, &shell);
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

    // 4. Bind.
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

    // 5. Signal handler → unblock workers for graceful shutdown.
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
    });
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    http::serve(server, state, workers);
    ExitCode::SUCCESS
}

fn print_help() {
    println!(
        "tmuxlet-server {VERSION}\n\nUsage:\n  tmuxlet-server [--config FILE]    Start the server\n  tmuxlet-server --validate         Validate config and exit\n  tmuxlet-server --version          Print version\n  tmuxlet-server --help             Print this help"
    );
}
