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

use crate::config::{Config, GithubMode, GithubOptions, Secrets};
use crate::event::Event;
use crate::github::GithubApi;
use crate::health::Health;
use crate::net::HttpClient;
use crate::settings::Settings;
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
    /// Set by `/check_now`: the next pass of that source reports its result.
    pub report_github: std::sync::atomic::AtomicBool,
    pub report_guides: std::sync::atomic::AtomicBool,
    pub report_games: std::sync::atomic::AtomicBool,
}

/// Result message of a check the admin asked for.
pub struct CheckReport {
    title: &'static str,
    source: crate::event::Source,
    health: &'static [&'static str],
    mark: i64,
    started: i64,
}

impl CheckReport {
    pub async fn start(
        app: &App,
        flag: &std::sync::atomic::AtomicBool,
        title: &'static str,
        source: crate::event::Source,
        health: &'static [&'static str],
    ) -> Option<Self> {
        if !flag.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let mark = app.db.max_outbox_id().await.unwrap_or(0);
        Some(Self {
            title,
            source,
            health,
            mark,
            started: crate::now(),
        })
    }

    pub async fn finish(self, app: &App) {
        let title = escape_html(self.title);
        let text = if crate::settings::current().source_on(self.source) {
            let n = app
                .db
                .count_events_since(self.mark, self.source.as_str())
                .await
                .unwrap_or(0);
            let snap = app.health.snapshot();
            let errors: Vec<String> = self
                .health
                .iter()
                .filter_map(|h| {
                    let (t, e) = snap.get(h)?.last_error.as_ref()?;
                    (*t >= self.started).then(|| {
                        format!(
                            "{}: {}",
                            escape_html(crate::health::title(h)),
                            crate::text::external_html(&crate::text::truncate_chars(e, 300))
                        )
                    })
                })
                .collect();
            if errors.is_empty() {
                crate::trf!(
                    "<b>{title}</b>: проверка завершена, новых событий: {n}.",
                    "<b>{title}</b>: check finished, new events: {n}."
                )
            } else {
                let list = errors.join("\n");
                crate::trf!(
                    "<b>{title}</b>: проверка завершена с ошибками, новых событий: {n}.\n{list}",
                    "<b>{title}</b>: check finished with errors, new events: {n}.\n{list}"
                )
            }
        } else {
            crate::trf!(
                "<b>{title}</b>: источник выключен в /settings, проверка не выполнялась.",
                "<b>{title}</b>: the source is off in /settings, nothing was checked."
            )
        };
        let key = format!("check:{}:{}", self.source.as_str(), self.started);
        if let Err(e) = app.db.enqueue_reply(&key, &text).await {
            tracing::warn!(error = %e, "check report not queued");
        }
        app.outbox_signal.notify_one();
    }
}

pub struct App {
    pub config_path: PathBuf,
    cfg: RwLock<Arc<Config>>,
    pub secrets: Secrets,
    pub db: Db,
    pub health: Health,
    pub http: HttpClient,
    pub github: Option<GithubApi>,
    pub steam: Arc<SteamHttp>,
    pub telegram: TelegramApi,
    installations: RwLock<Vec<i64>>,
    pub hook_ranges: RwLock<Option<Vec<ipnet::IpNet>>>,
    pub check_now: CheckNow,
    pub outbox_signal: Notify,
    pub cancel: CancellationToken,
    pub stranger_tx: RwLock<Option<mpsc::Sender<crate::telegram::commands::StrangerHit>>>,
    settings_lock: tokio::sync::Mutex<()>,
    settings_text: std::sync::Mutex<String>,
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

    /// Payload options: bot allow-list from the config, edits and deletes from settings.
    pub fn github_options(&self) -> GithubOptions {
        let s = crate::settings::current();
        GithubOptions {
            notify_edits: s.github.edits,
            notify_deletes: s.github.deletes,
            ..self.cfg().github.clone()
        }
    }

    /// Changes settings atomically: validate, store, apply side effects, activate.
    pub async fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<Settings> {
        let _guard = self.settings_lock.lock().await;
        let old = crate::settings::current();
        let mut new = (*old).clone();
        change(&mut new);
        if new == *old {
            return Ok(new);
        }
        let text = crate::settings::save(&self.db, &new).await?;
        self.activate(&old, new.clone(), text).await?;
        Ok(new)
    }

    async fn activate(&self, old: &Settings, new: Settings, text: String) -> Result<()> {
        if old.games.enabled && !new.games.enabled {
            self.db.forget_games_except(&[]).await?;
        }
        if old.guides.enabled && !new.guides.enabled {
            self.db.forget_guides().await?;
        }
        let games_changed =
            old.appids() != new.appids() || (!old.games.enabled && new.games.enabled);
        let guides_on = !old.guides.enabled && new.guides.enabled;
        let github_on = !old.github.enabled && new.github.enabled;
        let lang_changed = old.language != new.language;
        crate::settings::install(new);
        *self
            .settings_text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = text;
        if games_changed {
            self.check_now.games.notify_one();
        }
        if guides_on {
            self.check_now.guides.notify_one();
        }
        if github_on {
            self.check_now.github.notify_one();
        }
        if lang_changed {
            self.register_commands().await;
        }
        Ok(())
    }

    /// Picks up settings written by `hq-watchman settings set` while the bot runs.
    async fn reload_settings_if_changed(&self) -> Result<()> {
        let Some(text) = self.db.meta_get("settings").await? else {
            return Ok(());
        };
        let same = *self
            .settings_text
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            == text;
        if same {
            return Ok(());
        }
        let _guard = self.settings_lock.lock().await;
        match Settings::from_json(&text) {
            Ok(new) => {
                let old = crate::settings::current();
                tracing::info!("settings changed outside the bot, applied");
                self.activate(&old, new, text).await?;
            }
            Err(e) => {
                tracing::warn!(error = %e, "stored settings are invalid, keeping the active ones");
                *self
                    .settings_text
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = text;
            }
        }
        Ok(())
    }

    /// A group became a supergroup: follow it in settings and in the queue.
    pub async fn move_chat(&self, from: i64, to: i64) -> Result<()> {
        self.update_settings(|s| {
            if let Some(c) = s.chats.iter_mut().find(|c| c.id == from) {
                c.id = to;
            }
        })
        .await?;
        self.db.move_chat(from, to).await
    }

    pub async fn register_commands(&self) {
        if let Err(e) = self
            .telegram
            .register_commands(
                self.secrets.telegram_admin_id,
                &crate::telegram::commands::commands(),
            )
            .await
        {
            tracing::warn!(error = %e, "command menu not registered");
        }
    }

    /// `/reload`: applies a valid config, reports what needs a restart.
    pub fn reload(&self) -> String {
        let new = match Config::load(&self.config_path) {
            Ok(c) => c,
            Err(e) => {
                return crate::trf!(
                    "Конфиг не применён, ошибка:\n<code>{}</code>",
                    "Config not applied, error:\n<code>{}</code>",
                    escape_html(&format!("{e:#}"))
                )
            }
        };
        let old = self.cfg();
        let restart = old.restart_only_diff(&new);
        *self
            .cfg
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(new);
        let mut s = crate::tr!(
            "Конфиг перечитан и применён.",
            "Config re-read and applied."
        )
        .to_owned();
        if !restart.is_empty() {
            let list = escape_html(&restart.join(", "));
            let _ = write!(
                s,
                "{}",
                crate::trf!(
                    "\nЭти параметры действуют только после перезапуска: {list}.",
                    "\nThese settings take effect only after a restart: {list}."
                )
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
                    crate::tr!("Паника в подсистеме", "Subsystem panic"),
                    crate::trf!(
                        "Подсистема «{name}» аварийно завершилась и перезапущена. Остальные подсистемы работают. Если повторяется, нужен разбор логов.",
                        "Subsystem \"{name}\" crashed and was restarted. Other subsystems keep working. If this repeats, the logs need a look."
                    ),
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
    let Some(profile) = app.secrets.steam_profile_id else {
        return Ok(());
    };
    let poller = GuidesPoller {
        profile,
        steam: Arc::clone(&app.steam),
    };
    loop {
        let report = CheckReport::start(
            &app,
            &app.check_now.report_guides,
            crate::tr!("Руководства Steam", "Steam guides"),
            crate::event::Source::SteamGuide,
            &["guides", "guides_parser"],
        )
        .await;
        if crate::settings::current().guides.enabled {
            if let Err(e) = poller.poll(&app.db, &app.health).await {
                app.health
                    .fail(
                        "guides",
                        &format!("{e:#}"),
                        3,
                        crate::tr!("смотрите логи бота", "see the bot logs"),
                    )
                    .await;
            }
            app.outbox_signal.notify_one();
        }
        if let Some(r) = report {
            r.finish(&app).await;
        }
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

/// Copies game names learned from Steam into the settings list.
async fn sync_game_names(app: &App) -> Result<()> {
    let known: Vec<(i64, String, String)> = app.db.games().await?;
    let s = crate::settings::current();
    let stale = known.iter().any(|(id, name, _)| {
        s.games
            .list
            .iter()
            .any(|g| i64::from(g.appid) == *id && g.name != *name && !name.is_empty())
    });
    if stale {
        app.update_settings(|s| {
            for g in &mut s.games.list {
                if let Some((_, name, _)) =
                    known.iter().find(|(id, _, _)| *id == i64::from(g.appid))
                {
                    g.name = crate::text::clean_line(name, 100);
                }
            }
        })
        .await?;
    }
    Ok(())
}

async fn games_loop(app: Arc<App>) -> Result<()> {
    let mut poller = GamesPoller::new();
    loop {
        let report = CheckReport::start(
            &app,
            &app.check_now.report_games,
            crate::tr!("Игры Steam", "Steam games"),
            crate::event::Source::SteamUpdate,
            &["games", "pics", "news"],
        )
        .await;
        let cfg = app.cfg();
        let s = crate::settings::current();
        if s.games.enabled && !s.games.list.is_empty() {
            match poller
                .poll(&cfg, &s.appids(), &app.db, &app.health, &app.steam)
                .await
            {
                Ok(()) => {
                    app.health.ok("games").await;
                    if let Err(e) = sync_game_names(&app).await {
                        tracing::warn!(error = %e, "game names not saved");
                    }
                }
                Err(e) => {
                    app.health
                        .fail(
                            "games",
                            &format!("{e:#}"),
                            3,
                            crate::tr!(
                                "проверьте доступность Steam; бот повторит сам",
                                "check that Steam is reachable; the bot retries by itself"
                            ),
                        )
                        .await;
                }
            }
            app.outbox_signal.notify_one();
        } else if !s.games.enabled {
            app.db.forget_games_except(&[]).await?;
        }
        if let Some(r) = report {
            r.finish(&app).await;
        }
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

async fn settings_watch(app: Arc<App>) -> Result<()> {
    loop {
        if let Err(e) = app.reload_settings_if_changed().await {
            tracing::warn!(error = %e, "settings check failed");
        }
        app.health.beat("settings", 120);
        tokio::select! {
            () = app.cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep(Duration::from_secs(5)) => {}
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
                        &crate::trf!(
                            "запись в базу не удалась: {e:#}",
                            "database write failed: {e:#}"
                        ),
                        1,
                        crate::tr!(
                            "проверьте свободное место и права на том с базой",
                            "check free space and permissions of the database volume"
                        ),
                    )
                    .await;
            }
        }
        let now = crate::now();
        let cfg = app.cfg();
        let filter = app.github.is_some()
            && cfg.github.mode == GithubMode::Webhook
            && cfg.webhook.as_ref().is_some_and(|w| w.ip_filter);
        if filter
            && (now - ranges_at >= 86_400
                || (app.hook_ranges.read().map_or(true, |r| r.is_none())
                    && now - ranges_at >= 3600))
        {
            match crate::github::hook_ranges(&app.http).await {
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
                            &crate::trf!(
                                "резервная копия не создана: {e:#}",
                                "backup not written: {e:#}"
                            ),
                            1,
                            crate::tr!(
                                "проверьте свободное место на томе с базой",
                                "check free space on the database volume"
                            ),
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

fn source_line(on: bool, configured: bool) -> &'static str {
    match (on, configured) {
        (_, false) => crate::tr!("не настроено", "not configured"),
        (true, true) => crate::tr!("включено", "on"),
        (false, true) => crate::tr!("выключено", "off"),
    }
}

async fn startup_message(app: &App, previous_alive: Option<i64>) -> Result<()> {
    let now = crate::now();
    let s = crate::settings::current();
    let repos = app.db.repos().await?.len();
    let guides = app.db.guides().await?.len();
    let games = s.games.list.len();
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("HQW_COMMIT").unwrap_or("dev");
    let mut text = crate::trf!(
        "Версия {version} ({commit}).",
        "Version {version} ({commit})."
    );
    let gh = source_line(s.github.enabled, app.github.is_some());
    let gd = source_line(s.guides.enabled, app.secrets.steam_profile_id.is_some());
    let gm = source_line(s.games.enabled, true);
    let _ = write!(
        text,
        "{}",
        crate::trf!(
            "\nGitHub: {gh}, репозиториев {repos}.\nРуководства Steam: {gd}, руководств {guides}.\nИгры Steam: {gm}, игр {games}.",
            "\nGitHub: {gh}, repositories {repos}.\nSteam guides: {gd}, guides {guides}.\nSteam games: {gm}, games {games}."
        )
    );
    match previous_alive {
        Some(t) => {
            let (down, since) = (crate::fmt_duration(now - t), crate::fmt_time(t));
            let _ = write!(text, "{}", crate::trf!("\nПростой: {down} (с {since}).", "\nDowntime: {down} (since {since})."));
            if now - t > 3 * 86_400 && app.github.is_some() {
                let (from, to) = (crate::fmt_time(t), crate::fmt_time(now - 3 * 86_400));
                let _ = write!(
                    text,
                    "{}",
                    crate::trf!(
                        "\nБот был выключен дольше 3 суток. GitHub хранит журнал доставок только 3 дня, поэтому события GitHub с {from} по {to} могли быть потеряны.",
                        "\nThe bot was off for more than 3 days. GitHub keeps the delivery log for 3 days only, so GitHub events from {from} to {to} may be lost."
                    )
                );
            }
        }
        None => text.push_str(crate::tr!(
            "\nПервый запуск: сначала записывается текущее состояние, старые события не присылаются. Настройки: /settings.",
            "\nFirst start: the current state is recorded first, old events are not sent. Settings: /settings."
        )),
    }
    app.notify(&Event::service(
        format!("bot:start:{now}"),
        crate::tr!("Бот запущен", "Bot started"),
        text,
    ))
    .await;
    Ok(())
}

/// Refuses to start with a GitHub setup that cannot receive events.
fn check_github_setup(cfg: &Config, secrets: &Secrets) -> Result<()> {
    let Some(gh) = &secrets.github else {
        return Ok(());
    };
    if cfg.github.mode == GithubMode::Webhook {
        if cfg.webhook.is_none() {
            bail!("github.mode is \"webhook\" but the config has no [webhook] section; add it or set github.mode = \"polling\"");
        }
        if gh.webhook_secret.is_none() {
            bail!("github.mode is \"webhook\" but HQW_GITHUB_WEBHOOK_SECRET is not set");
        }
    }
    Ok(())
}

pub async fn run(config_path: PathBuf, cfg: Config, secrets: Secrets) -> Result<()> {
    check_github_setup(&cfg, &secrets)?;
    let db = Db::open(&cfg.storage.path, cfg.storage.backups).await?;
    let settings = if let Some((s, _)) = crate::settings::load(&db).await? {
        s
    } else {
        let s = Settings::seed(&cfg);
        crate::settings::save(&db, &s).await?;
        s
    };
    let settings_text = serde_json::to_string(&settings)?;
    crate::settings::install(settings);
    let ua = format!("hq-watchman/{}", env!("CARGO_PKG_VERSION"));
    let http = HttpClient::new(&ua)?;
    let telegram = TelegramApi::new(http.clone(), secrets.telegram_token.clone());
    match telegram.get_me().await {
        Ok(_) => {}
        Err(TgError::Unauthorized) => bail!("Telegram rejected HQW_TELEGRAM_TOKEN"),
        Err(e) => tracing::warn!(error = %e, "Telegram not reachable at start, will retry"),
    }
    let github = match &secrets.github {
        Some(g) => Some(GithubApi::new(http.clone(), g.app_id, &g.app_key)?),
        None => None,
    };
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
    let mode = cfg.github.mode;
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
        settings_lock: tokio::sync::Mutex::new(()),
        settings_text: std::sync::Mutex::new(settings_text),
    });
    app.db.write_probe().await?;
    app.register_commands().await;
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
    if app.github.is_some() {
        match mode {
            GithubMode::Webhook => {
                tasks.spawn(supervise(
                    Arc::clone(&app),
                    "webhook",
                    crate::github::webhook::serve,
                ));
            }
            GithubMode::Polling => {
                tasks.spawn(supervise(
                    Arc::clone(&app),
                    "github_poll",
                    crate::github::poll::run,
                ));
            }
        }
        tasks.spawn(supervise(
            Arc::clone(&app),
            "github",
            crate::github::reconcile::run,
        ));
    }
    tasks.spawn(supervise(Arc::clone(&app), "guides", guides_loop));
    tasks.spawn(supervise(Arc::clone(&app), "games", games_loop));
    tasks.spawn(supervise(Arc::clone(&app), "settings", settings_watch));
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
