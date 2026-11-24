// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::PathBuf;
use std::process::ExitCode;

use hq_watchman::config::{Config, LogFormat, Secrets};

const USAGE: &str =
    "usage: hq-watchman [run] [--config PATH] | healthcheck [--addr HOST:PORT] | --version";

enum Mode {
    Run(PathBuf),
    Health(String),
    Version,
}

fn parse_args() -> Result<Mode, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.iter().map(String::as_str);
    let mut mode = "run";
    let mut config = PathBuf::from("/config/config.toml");
    let mut addr = "127.0.0.1:9091".to_owned();
    while let Some(a) = it.next() {
        match a {
            "run" | "healthcheck" => mode = if a == "run" { "run" } else { "health" },
            "--version" | "-V" => return Ok(Mode::Version),
            "--config" => config = it.next().ok_or("--config needs a path")?.into(),
            "--addr" => it
                .next()
                .ok_or("--addr needs HOST:PORT")?
                .clone_into(&mut addr),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    Ok(if mode == "health" {
        Mode::Health(addr)
    } else {
        Mode::Run(config)
    })
}

fn init_logging(cfg: &Config) {
    use tracing_subscriber::filter::{LevelFilter, Targets};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;
    let level = cfg
        .logging
        .level
        .parse::<LevelFilter>()
        .unwrap_or(LevelFilter::INFO);
    let filter = Targets::new()
        .with_target("hq_watchman", level)
        .with_default(LevelFilter::WARN.min(level));
    let base = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false);
    let layer = match cfg.logging.format {
        LogFormat::Json => base.json().flatten_event(true).with_filter(filter).boxed(),
        LogFormat::Text => base.with_filter(filter).boxed(),
    };
    tracing_subscriber::registry().with(layer).init();
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))
}

fn main() -> ExitCode {
    let mode = match parse_args() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let result = match mode {
        Mode::Version => {
            eprintln!(
                "hq-watchman {} ({})",
                env!("CARGO_PKG_VERSION"),
                option_env!("HQW_COMMIT").unwrap_or("dev")
            );
            Ok(())
        }
        Mode::Health(addr) => runtime().and_then(|rt| {
            rt.block_on(hq_watchman::app::healthcheck(&addr))
                .map_err(|e| e.to_string())
        }),
        Mode::Run(path) => run(path),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: PathBuf) -> Result<(), String> {
    let secrets = Secrets::from_env().map_err(|e| e.to_string())?;
    let cfg = Config::load(&path).map_err(|e| format!("{e:#}"))?;
    init_logging(&cfg);
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        return Err("TLS provider already installed".into());
    }
    let rt = runtime()?;
    rt.block_on(hq_watchman::app::run(path, cfg, secrets))
        .map_err(|e| format!("{e:#}"))
}
