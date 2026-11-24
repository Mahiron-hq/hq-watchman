// SPDX-License-Identifier: GPL-3.0-or-later
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hq_watchman::config::{Config, GithubMode, LogFormat, Secrets};

const USAGE: &str = "usage: hq-watchman [run] [--config PATH] [--env-file PATH] [--log-file PATH]
       hq-watchman check [--config PATH] [--env-file PATH]
       hq-watchman settings get|set [--config PATH]
       hq-watchman backup [--config PATH]
       hq-watchman healthcheck [--addr HOST:PORT]
       hq-watchman --version";

enum Mode {
    Run(PathBuf, Option<PathBuf>, Option<PathBuf>),
    Check(PathBuf, Option<PathBuf>),
    SettingsGet(PathBuf),
    SettingsSet(PathBuf),
    Backup(PathBuf),
    Health(String),
    Version,
}

fn parse_args() -> Result<Mode, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.iter().map(String::as_str);
    let mut mode = "run";
    let mut config = PathBuf::from("/config/config.toml");
    let mut addr = "127.0.0.1:9091".to_owned();
    let mut env_file = None;
    let mut log_file = None;
    while let Some(a) = it.next() {
        match a {
            "run" | "healthcheck" | "check" | "backup" => mode = a,
            "settings" => {
                mode = match it.next() {
                    Some("get") => "settings-get",
                    Some("set") => "settings-set",
                    _ => return Err(format!("settings needs get or set\n{USAGE}")),
                };
            }
            "--version" | "-V" => return Ok(Mode::Version),
            "--config" => config = it.next().ok_or("--config needs a path")?.into(),
            "--log-file" => {
                log_file = Some(PathBuf::from(it.next().ok_or("--log-file needs a path")?));
            }
            "--env-file" => {
                env_file = Some(PathBuf::from(it.next().ok_or("--env-file needs a path")?));
            }
            "--addr" => it
                .next()
                .ok_or("--addr needs HOST:PORT")?
                .clone_into(&mut addr),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    Ok(match mode {
        "healthcheck" => Mode::Health(addr),
        "check" => Mode::Check(config, env_file),
        "settings-get" => Mode::SettingsGet(config),
        "settings-set" => Mode::SettingsSet(config),
        "backup" => Mode::Backup(config),
        _ => Mode::Run(config, env_file, log_file),
    })
}

/// Log file for installations without a journal; the old file is kept once as `.1`.
fn open_log(path: &Path) -> Result<std::fs::File, String> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("log file {}: {e}", path.display()))
}

fn init_logging(cfg: &Config, file: Option<std::fs::File>) {
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
    let layer = match (cfg.logging.format, file) {
        (LogFormat::Json, None) => base.json().flatten_event(true).with_filter(filter).boxed(),
        (LogFormat::Text, None) => base.with_filter(filter).boxed(),
        (LogFormat::Json, Some(f)) => base
            .with_writer(std::sync::Mutex::new(f))
            .json()
            .flatten_event(true)
            .with_filter(filter)
            .boxed(),
        (LogFormat::Text, Some(f)) => base
            .with_writer(std::sync::Mutex::new(f))
            .with_filter(filter)
            .boxed(),
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

fn out(text: &str) -> Result<(), String> {
    let mut o = std::io::stdout().lock();
    writeln!(o, "{text}").map_err(|e| format!("stdout: {e}"))
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
        Mode::Check(path, env) => check(&path, env.as_deref()),
        Mode::SettingsGet(path) => settings_get(&path),
        Mode::SettingsSet(path) => settings_set(&path),
        Mode::Backup(path) => backup(&path),
        Mode::Run(path, env, log) => run(path, env.as_deref(), log.as_deref()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn secrets(env_file: Option<&Path>) -> Result<Secrets, String> {
    match env_file {
        Some(f) => Secrets::from_file(f).map_err(|e| format!("{e:#}")),
        None => Secrets::from_env().map_err(|e| e.to_string()),
    }
}

fn run(path: PathBuf, env_file: Option<&Path>, log_file: Option<&Path>) -> Result<(), String> {
    let secrets = secrets(env_file)?;
    let cfg = Config::load(&path).map_err(|e| format!("{e:#}"))?;
    let file = log_file.map(open_log).transpose()?;
    init_logging(&cfg, file);
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

/// Validates the config file and the environment without starting anything.
fn check(path: &Path, env_file: Option<&Path>) -> Result<(), String> {
    let cfg = Config::load(path).map_err(|e| format!("{e:#}"))?;
    let secrets = secrets(env_file)?;
    let gh = secrets.github.as_ref();
    if let (Some(g), GithubMode::Webhook) = (gh, cfg.github.mode) {
        if cfg.webhook.is_none() {
            return Err(
                "github.mode is \"webhook\" but the config has no [webhook] section".into(),
            );
        }
        if g.webhook_secret.is_none() {
            return Err(
                "github.mode is \"webhook\" but HQW_GITHUB_WEBHOOK_SECRET is not set".into(),
            );
        }
    }
    let report = serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "github": gh.is_some(),
        "github_mode": match cfg.github.mode {
            GithubMode::Webhook => "webhook",
            GithubMode::Polling => "polling",
        },
        "guides": secrets.steam_profile_id.is_some(),
        "steam_key": secrets.steam_api_key.is_some(),
    });
    out(&report.to_string())
}

fn with_db<T>(
    path: &Path,
    f: impl AsyncFnOnce(&Config, hq_watchman::storage::Db) -> anyhow::Result<T>,
) -> Result<T, String> {
    let cfg = Config::load(path).map_err(|e| format!("{e:#}"))?;
    let rt = runtime()?;
    rt.block_on(async {
        let db = hq_watchman::storage::Db::open(&cfg.storage.path, cfg.storage.backups).await?;
        let r = f(&cfg, db.clone()).await;
        db.pool().close().await;
        r
    })
    .map_err(|e| format!("{e:#}"))
}

/// Prints settings plus what the bot knows (repositories, guides, games) as JSON.
fn settings_get(path: &Path) -> Result<(), String> {
    let text = with_db(path, async |cfg, db| {
        let settings = match hq_watchman::settings::load(&db).await? {
            Some((s, _)) => s,
            None => hq_watchman::settings::Settings::seed(cfg),
        };
        let repos: Vec<_> = db
            .repos()
            .await?
            .into_iter()
            .map(|r| serde_json::json!({"name": r.full_name, "private": r.private}))
            .collect();
        let guides: Vec<_> = db
            .guides()
            .await?
            .into_iter()
            .map(|g| serde_json::json!({"id": g.id, "title": g.title, "game": g.game}))
            .collect();
        let v = serde_json::json!({
            "settings": settings,
            "repos": repos,
            "guides": guides,
        });
        Ok(v.to_string())
    })?;
    out(&text)
}

/// Reads settings JSON from stdin, validates and stores it; a running bot applies it.
fn settings_set(path: &Path) -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut input)
        .map_err(|e| format!("stdin: {e}"))?;
    let settings =
        hq_watchman::settings::Settings::from_json(&input).map_err(|e| format!("{e:#}"))?;
    with_db(path, async |_, db| {
        hq_watchman::settings::save(&db, &settings).await?;
        Ok(())
    })?;
    out("{\"ok\":true}")
}

fn backup(path: &Path) -> Result<(), String> {
    let file = with_db(path, async |cfg, db| {
        Ok(db.backup(cfg.storage.backups).await?.display().to_string())
    })?;
    out(&serde_json::json!({ "ok": true, "file": file }).to_string())
}
