// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared state, task supervision, maintenance and startup.

use std::fmt::Write as _;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Secrets};
use crate::event::Event;
use crate::github::GithubApi;
use crate::health::Health;
use crate::net::HttpClient;
use crate::steam::games::GamesPoller;
use crate::steam::guides::GuidesPoller;
use crate::steam::SteamHttp;
use crate::storage::Db;
use crate::telegram::api::{TelegramApi, TgError};
use crate::text::escape_html;

#[derive(Default)]
pub struct CheckNow {
    pub github: Notify,
    pub guides: Notify,
    pub games: Notify,
}

pub struct App {
    pub config_path: PathBuf,
    cfg: RwLock<Arc<Config>>,
    pub secrets: Secrets,
    pub db: Db,
    pub health: Health,
    pub http: HttpClient,
    pub github: GithubApi,
    pub steam: Arc<SteamHttp>,
    pub telegram: TelegramApi,
    installations: RwLock<Vec<i64>>,
    pub hook_ranges: RwLock<Option<Vec<ipnet::IpNet>>>,
    pub check_now: CheckNow,
    pub outbox_signal: Notify,
    pub cancel: CancellationToken,
    pub stranger_tx: RwLock<Option<mpsc::Sender<crate::telegram::commands::StrangerHit>>>,
}

impl App {
    pub fn cfg(&self) -> Arc<Config> {
        Arc::clone(
            &self
                .cfg
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    pub fn known_installations(&self) -> Vec<i64> {
        self.installations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_installations(&self, ids: Vec<i64>) {
        *self
            .installations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ids;
    }

    /// `/reload`: applies a valid config, reports what needs a restart.
    pub fn reload(&self) -> String {
        let new = match Config::load(&self.config_path) {
            Ok(c) => c,
            Err(e) => {
                return format!(
                    "Конфиг не применён, ошибка:\n<code>{}</code>",
                    escape_html(&format!("{e:#}"))
                )
            }
        };
        let old = self.cfg();
        let restart = old.restart_only_diff(&new);
        let games_changed = old.games.appids != new.games.appids;
        crate::set_timezone(new.timezone.clone());
        *self
            .cfg
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(new);
        if games_changed {
            self.check_now.games.notify_one();
        }
        let mut s = String::from("Конфиг перечитан и применён.");
        if games_changed {
            s.push_str(" Список игр изменился, проверка игр запущена.");
        }
        if !restart.is_empty() {
            let _ = write!(
                s,
                "\nЭти параметры действуют только после перезапуска: {}.",
                escape_html(&restart.join(", "))
            );
        }
        s
    }

    async fn notify(&self, ev: &Event) {
        if let Err(e) = self.db.enqueue(ev).await {
            tracing::error!(error = %e, "cannot queue service message");
        }
        self.outbox_signal.notify_one();
    }
}

/// Restarts a task after an error or a panic, reports panics.
async fn supervise<F, Fut>(app: Arc<App>, name: &'static str, make: F)
where
    F: Fn(Arc<App>) -> Fut,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    loop {
        let handle = tokio::spawn(make(Arc::clone(&app)));
        let result = handle.await;
        if app.cancel.is_cancelled() {
            return;
        }
        match result {
            Ok(Err(e)) => {
                tracing::error!(task = name, error = %format!("{e:#}"), "task failed, restarting");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            Err(e) if e.is_panic() => {
                tracing::error!(task = name, "task panicked, restarting");
                let ev = Event::service(
                    format!("bot:panic:{name}:{}", crate::now()),
                    "Паника в подсистеме",
                    format!("Подсистема «{name}» аварийно завершилась и перезапущена. Остальные подсистемы работают. Если повторяется, нужен разбор логов."),
                );
                app.notify(&ev).await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Ok(Ok(())) | Err(_) => return,
        }
    }
}

fn jitter(base: Duration) -> Duration {
    base + Duration::from_secs(rand::random_range(0..60))
}

async fn guides_loop(app: Arc<App>) -> Result<()> {
    let poller = GuidesPoller {
        profile: app.secrets.steam_profile_id,
        steam: Arc::clone(&app.steam),
    };
    loop {
        if let Err(e) = poller.poll(&app.db, &app.health).await {
            app.health
                .fail("guides", &format!("{e:#}"), 3, "смотрите логи бота")
                .await;
        }
        app.outbox_signal.notify_one();
        let minutes = app.cfg().intervals.guides_minutes;
        app.health
            .beat("guides_loop", i64::try_from(minutes)? * 60 + 1800);
        tokio::select! {
            () = app.cancel.cancelled() => return Ok(()),
            () = app.check_now.guides.notified() => {}
            () = tokio::time::sleep(jitter(Duration::from_secs(minutes * 60))) => {}
        }
    }
}

async fn games_loop(app: Arc<App>) -> Result<()> {
    let mut poller = GamesPoller::new();
    loop {
        let cfg = app.cfg();
        match poller.poll(&cfg, &app.db, &app.health, &app.steam).await {
            Ok(()) => app.health.ok("games").await,
            Err(e) => {
                app.health
                    .fail(
                        "games",
                        &format!("{e:#}"),
                        3,
                        "проверьте доступность Steam; бот повторит сам",
                    )
                    .await;
            }
        }
        app.outbox_signal.notify_one();
        let minutes = cfg.intervals.games_minutes;
        app.health
            .beat("games_loop", i64::try_from(minutes)? * 60 + 1800);
        tokio::select! {
            () = app.cancel.cancelled() => return Ok(()),
            () = app.check_now.games.notified() => {}
            () = tokio::time::sleep(jitter(Duration::from_secs(minutes * 60))) => {}
        }
    }
}

/// Liveness probe, write probe, GitHub IP ranges, cleanup and daily backup.
async fn maintenance_loop(app: Arc<App>) -> Result<()> {
    let mut ranges_at = 0i64;
    loop {
        app.health.beat("maintenance", 600);
        match app.db.write_probe().await {
            Ok(()) => app.health.ok("storage").await,
            Err(e) => {
                app.health
                    .fail(
                        "storage",
                        &format!("запись в базу не удалась: {e:#}"),
                        1,
                        "проверьте свободное место и права на том с базой",
                    )
                    .await;
            }
        }
        let now = crate::now();
        if now - ranges_at >= 86_400
            || (app.cfg().webhook.ip_filter
                && app.hook_ranges.read().map_or(true, |r| r.is_none())
                && now - ranges_at >= 3600)
        {
            match app.github.hook_ranges().await {
                Ok(r) => {
                    *app.hook_ranges
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(r);
                    ranges_at = now;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "GitHub hook ranges not loaded");
                    ranges_at = now - 86_400 + 3600;
                }
            }
        }
        let last_daily: i64 = app
            .db
            .meta_get("last_daily")
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if now - last_daily >= 86_400 {
            let cfg = app.cfg();
            if let Err(e) = app
                .db
                .cleanup(cfg.storage.retention_days, cfg.strangers.retention_days)
                .await
            {
                tracing::error!(error = %e, "cleanup failed");
            }
            match app.db.backup(cfg.storage.backups).await {
                Ok(p) => tracing::info!(file = %p.display(), "database backup written"),
                Err(e) => {
                    app.health
                        .fail(
                            "storage",
                            &format!("резервная копия не создана: {e:#}"),
                            1,
                            "проверьте свободное место на томе с базой",
                        )
                        .await;
                }
            }
            app.db.meta_set("last_daily", &now.to_string()).await?;
        }
        tokio::select! {
            () = app.cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep(Duration::from_secs(60)) => {}
        }
    }
}

async fn health_server(app: Arc<App>) -> Result<()> {
    let addr = app.cfg().health.listen;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    loop {
        let (mut stream, _) = tokio::select! {
            () = app.cancel.cancelled() => return Ok(()),
            r = listener.accept() => r?,
        };
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let _ = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;
            let healthy = app.db.ping().await.is_ok() && app.health.stale().is_empty();
            let resp: &[u8] = if healthy {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
            } else {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfail"
            };
            let _ = stream.write_all(resp).await;
        });
    }
}

/// `healthcheck` subcommand for Docker: exit code 0 when the bot reports healthy.
pub async fn healthcheck(addr: &str) -> Result<()> {
    let mut s = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(addr))
        .await
        .context("timeout")??;
    s.write_all(b"GET /health HTTP/1.0\r\n\r\n").await?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf))
        .await
        .context("timeout")??;
    if buf.starts_with(b"HTTP/1.1 200") {
        Ok(())
    } else {
        bail!("unhealthy")
    }
}

async fn startup_message(app: &App, previous_alive: Option<i64>) -> Result<()> {
    let now = crate::now();
    let repos = app.db.repos().await?.len();
    let guides = app.db.guides().await?.len();
    let games = app.cfg().games.appids.len();
    let mut text = format!(
        "Версия {} ({}). Под наблюдением: репозиториев {repos}, руководств {guides}, игр {games}.",
        env!("CARGO_PKG_VERSION"),
        option_env!("HQW_COMMIT").unwrap_or("dev")
    );
    match previous_alive {
        Some(t) => {
            let _ = write!(text, "\nПростой: {} (с {}).", crate::fmt_duration(now - t), crate::fmt_time(t));
            if now - t > 3 * 86_400 {
                let _ = write!(
                    text,
                    "\nБот был выключен дольше 3 суток. GitHub хранит журнал доставок только 3 дня, поэтому события GitHub с {} по {} могли быть потеряны.",
                    crate::fmt_time(t),
                    crate::fmt_time(now - 3 * 86_400)
                );
            }
        }
        None => text.push_str("\nПервый запуск: сначала записывается текущее состояние, старые события не присылаются."),
    }
    app.notify(&Event::service(
        format!("bot:start:{now}"),
        "Бот запущен",
        text,
    ))
    .await;
    Ok(())
}

pub async fn run(config_path: PathBuf, cfg: Config, secrets: Secrets) -> Result<()> {
    crate::set_timezone(cfg.timezone.clone());
    let db = Db::open(&cfg.storage.path, cfg.storage.backups).await?;
    let ua = format!("hq-watchman/{}", env!("CARGO_PKG_VERSION"));
    let http = HttpClient::new(&ua)?;
    let telegram = TelegramApi::new(http.clone(), secrets.telegram_token.clone());
    match telegram.get_me().await {
        Ok(_) => {}
        Err(TgError::Unauthorized) => bail!("Telegram rejected HQW_TELEGRAM_TOKEN"),
        Err(e) => tracing::warn!(error = %e, "Telegram not reachable at start, will retry"),
    }
    let github = GithubApi::new(http.clone(), secrets.github_app_id, &secrets.github_app_key)?;
    let steam = Arc::new(SteamHttp::new(http.clone(), secrets.steam_api_key.clone()));
    let previous_alive = db
        .meta_get("last_alive")
        .await?
        .and_then(|v| v.parse().ok());
    let installations = db
        .installations()
        .await?
        .into_iter()
        .filter(|(_, _, s)| !s)
        .map(|(id, _, _)| id)
        .collect();
    let app = Arc::new(App {
        config_path,
        cfg: RwLock::new(Arc::new(cfg)),
        secrets,
        health: Health::new(db.clone()),
        db,
        http,
        github,
        steam,
        telegram,
        installations: RwLock::new(installations),
        hook_ranges: RwLock::new(None),
        check_now: CheckNow::default(),
        outbox_signal: Notify::new(),
        cancel: CancellationToken::new(),
        stranger_tx: RwLock::new(None),
    });
    app.db.write_probe().await?;
    if let Err(e) = app
        .telegram
        .register_commands(
            app.secrets.telegram_admin_id,
            crate::telegram::commands::COMMANDS,
        )
        .await
    {
        tracing::warn!(error = %e, "command menu not registered");
    }
    startup_message(&app, previous_alive).await?;

    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(supervise(Arc::clone(&app), "sender", |a| {
        crate::telegram::sender::Sender::new(a).run()
    }));
    tasks.spawn(supervise(Arc::clone(&app), "telegram", |a| {
        crate::telegram::commands::Poller::new(a).run()
    }));
    tasks.spawn(supervise(Arc::clone(&app), "strangers", |a| {
        let (tx, rx) = mpsc::channel(100);
        *a.stranger_tx
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
        crate::telegram::commands::stranger_worker(a, rx)
    }));
    tasks.spawn(supervise(
        Arc::clone(&app),
        "webhook",
        crate::github::webhook::serve,
    ));
    tasks.spawn(supervise(
        Arc::clone(&app),
        "github",
        crate::github::reconcile::run,
    ));
    tasks.spawn(supervise(Arc::clone(&app), "guides", guides_loop));
    tasks.spawn(supervise(Arc::clone(&app), "games", games_loop));
    tasks.spawn(supervise(Arc::clone(&app), "maintenance", maintenance_loop));
    tasks.spawn(supervise(Arc::clone(&app), "health", health_server));

    shutdown_signal().await;
    tracing::info!("shutdown requested");
    app.cancel.cancel();
    if tokio::time::timeout(Duration::from_secs(10), async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tracing::warn!("tasks did not stop in 10 s");
        tasks.abort_all();
    }
    app.db.pool().close().await;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
