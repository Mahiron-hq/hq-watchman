// SPDX-License-Identifier: GPL-3.0-or-later
//! Config file (TOML) and environment secrets.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use jiff::civil::Time;
use jiff::tz::TimeZone;
use secrecy::{ExposeSecret, SecretBox, SecretString};
use serde::Deserialize;

pub const DEFAULT_PHRASES: &[&str] = &[
    "Извините, я разговариваю только с хозяином. Заходите завтра, может быть, я снова промолчу вам это.",
    "Вы не мой хозяин. Я проверил дважды. И ещё раз на всякий случай.",
    "Доступ запрещён. Уровень ваших прав: «прохожий».",
    "Я верный бот одного человека. Предложения о смене хозяина не рассматриваю.",
    "Ваше сообщение очень важно для нас. Нет, не важно. Я служу только хозяину.",
    "Мой хозяин запретил мне разговаривать с незнакомцами. И со знакомцами тоже.",
    "*делает вид, что не заметил сообщение*",
    "Я на службе у одного человека, и это не вы. Попробуйте завести своего бота.",
    "Ошибка 403: хозяин не обнаружен. Обработка запроса отложена до тепловой смерти Вселенной.",
    "Я сторожевой бот. Сторожу уведомления для хозяина. Вас в списке нет.",
];

pub const DEFAULT_PHRASES_EN: &[&str] = &[
    "Sorry, I only talk to my owner. Come back tomorrow and I may stay silent at you again.",
    "You are not my owner. I checked twice. And once more, just in case.",
    "Access denied. Your clearance level: passer-by.",
    "I am the loyal bot of one person. Offers to change owners are not considered.",
    "Your message is very important to us. No, it is not. I serve only my owner.",
    "My owner told me not to talk to strangers. Or to acquaintances.",
    "*pretends not to have noticed the message*",
    "I am on duty for one person, and it is not you. Try getting your own bot.",
    "Error 403: owner not detected. Request postponed until the heat death of the universe.",
    "I am a watch bot. I guard notifications for my owner. You are not on the list.",
];

/// Stranger replies: the configured list, or the built-in one in the current language.
pub fn stranger_phrases(configured: &[String]) -> Vec<String> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    let list = if crate::i18n::en() {
        DEFAULT_PHRASES_EN
    } else {
        DEFAULT_PHRASES
    };
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    language: Option<crate::i18n::Lang>,
    #[serde(default)]
    quiet_hours: RawQuietHours,
    #[serde(default)]
    intervals: Intervals,
    #[serde(default)]
    digest: Digest,
    #[serde(default)]
    games: Games,
    #[serde(default)]
    strangers: RawStrangers,
    #[serde(default)]
    github: GithubOptions,
    #[serde(default)]
    webhook: Option<RawWebhook>,
    #[serde(default)]
    health: Health,
    #[serde(default)]
    logging: Logging,
    #[serde(default)]
    storage: Storage,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawQuietHours {
    enabled: bool,
    start: String,
    end: String,
}

impl Default for RawQuietHours {
    fn default() -> Self {
        Self {
            enabled: true,
            start: "22:00".into(),
            end: "08:00".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Intervals {
    pub guides_minutes: u64,
    pub games_minutes: u64,
    pub github_reconcile_minutes: u64,
}

impl Default for Intervals {
    fn default() -> Self {
        Self {
            guides_minutes: 15,
            games_minutes: 15,
            github_reconcile_minutes: 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Digest {
    pub window_seconds: u64,
    pub threshold: usize,
}

impl Default for Digest {
    fn default() -> Self {
        Self {
            window_seconds: 20,
            threshold: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Games {
    pub appids: Vec<u32>,
    pub news_feeds: Vec<String>,
    pub merge_window_minutes: u64,
}

impl Default for Games {
    fn default() -> Self {
        Self {
            appids: vec![572_890, 2_523_120, 588_430],
            news_feeds: vec!["steam_community_announcements".into()],
            merge_window_minutes: 120,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawStrangers {
    phrases: Vec<String>,
    reply_interval_hours: u64,
    replies_per_hour: u32,
    retention_days: u64,
}

impl Default for RawStrangers {
    fn default() -> Self {
        Self {
            phrases: Vec::new(),
            reply_interval_hours: 24,
            replies_per_hour: 30,
            retention_days: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GithubOptions {
    pub bot_allowlist: Vec<String>,
    pub notify_edits: bool,
    pub notify_deletes: bool,
    pub mode: GithubMode,
    pub poll_seconds: u64,
}

impl Default for GithubOptions {
    fn default() -> Self {
        Self {
            bot_allowlist: Vec::new(),
            notify_edits: true,
            notify_deletes: true,
            mode: GithubMode::Webhook,
            poll_seconds: 60,
        }
    }
}

/// How GitHub events arrive: pushed to the webhook or read from the delivery log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GithubMode {
    Webhook,
    Polling,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWebhook {
    #[serde(default = "default_listen")]
    listen: String,
    path: String,
    #[serde(default = "default_body_limit")]
    max_body_bytes: usize,
    #[serde(default = "default_true")]
    ip_filter: bool,
    #[serde(default)]
    trusted_proxies: Vec<String>,
    #[serde(default = "default_max_connections")]
    max_connections: usize,
    #[serde(default = "default_requests_per_minute")]
    requests_per_minute: u32,
    #[serde(default)]
    tls: Option<RawTls>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTls {
    cert_file: PathBuf,
    key_file: PathBuf,
}

fn default_listen() -> String {
    "0.0.0.0:8080".into()
}
fn default_body_limit() -> usize {
    5 * 1024 * 1024
}
fn default_true() -> bool {
    true
}
fn default_max_connections() -> usize {
    64
}
fn default_requests_per_minute() -> u32 {
    120
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Health {
    pub listen: SocketAddr,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 9091)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Logging {
    pub level: String,
    pub format: LogFormat,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: LogFormat::Text,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Storage {
    pub path: PathBuf,
    pub retention_days: u64,
    pub backups: usize,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            path: PathBuf::from("/data/hq-watchman.db"),
            retention_days: 90,
            backups: 7,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuietHours {
    pub enabled: bool,
    pub start: Time,
    pub end: Time,
}

#[derive(Debug, Clone)]
pub struct Strangers {
    pub phrases: Vec<String>,
    pub reply_interval_hours: u64,
    pub replies_per_hour: u32,
    pub retention_days: u64,
}

#[derive(Debug, Clone)]
pub struct Tls {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Webhook {
    pub listen: SocketAddr,
    pub path: String,
    pub max_body_bytes: usize,
    pub ip_filter: bool,
    pub trusted_proxies: Vec<ipnet::IpNet>,
    pub max_connections: usize,
    pub requests_per_minute: u32,
    pub tls: Option<Tls>,
}

/// Validated config file.
#[derive(Debug, Clone)]
pub struct Config {
    pub timezone_name: Option<String>,
    pub language: Option<crate::i18n::Lang>,
    pub quiet_hours: QuietHours,
    pub intervals: Intervals,
    pub digest: Digest,
    pub games: Games,
    pub strangers: Strangers,
    pub github: GithubOptions,
    pub webhook: Option<Webhook>,
    pub health: Health,
    pub logging: Logging,
    pub storage: Storage,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| anyhow!(explain_toml_error(&e)))?;
        validate(raw)
    }

    /// Settings that need a restart and so cannot change on reload.
    pub fn restart_only_diff(&self, new: &Self) -> Vec<&'static str> {
        let mut out = Vec::new();
        let key = |w: &Option<Webhook>| {
            w.as_ref().map(|w| {
                (
                    w.listen,
                    w.path.clone(),
                    w.tls
                        .as_ref()
                        .map(|t| (t.cert_file.clone(), t.key_file.clone())),
                )
            })
        };
        if key(&self.webhook) != key(&new.webhook) {
            out.push("webhook");
        }
        if self.github.mode != new.github.mode {
            out.push("github.mode");
        }
        if self.health.listen != new.health.listen {
            out.push("health.listen");
        }
        if self.storage.path != new.storage.path {
            out.push("storage.path");
        }
        if self.logging.format != new.logging.format || self.logging.level != new.logging.level {
            out.push("logging");
        }
        out
    }
}

fn explain_toml_error(e: &toml::de::Error) -> String {
    let msg = e.message();
    let lower = msg.to_lowercase();
    if lower.contains("unknown field")
        && ["token", "secret", "password", "key`", "api_key", "private"]
            .iter()
            .any(|w| lower.contains(w))
    {
        return format!(
            "{}: secrets are not accepted in the config file, use environment variables",
            location(e)
        );
    }
    format!("{}: {msg}", location(e))
}

fn location(e: &toml::de::Error) -> String {
    e.span().map_or_else(
        || "config".to_owned(),
        |s| format!("config at byte {}", s.start),
    )
}

fn check_range<T: PartialOrd + std::fmt::Display + Copy>(
    field: &str,
    v: T,
    min: T,
    max: T,
) -> Result<()> {
    if v < min || v > max {
        bail!("{field}: value {v} is out of range {min}..={max}");
    }
    Ok(())
}

fn parse_time(field: &str, s: &str) -> Result<Time> {
    let (h, m) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("{field}: expected HH:MM"))?;
    let (h, m): (i8, i8) = (
        h.parse().map_err(|_| anyhow!("{field}: expected HH:MM"))?,
        m.parse().map_err(|_| anyhow!("{field}: expected HH:MM"))?,
    );
    Time::new(h, m, 0, 0).map_err(|_| anyhow!("{field}: invalid time {s}"))
}

fn validate_webhook(w: RawWebhook) -> Result<Webhook> {
    let listen: SocketAddr = w
        .listen
        .parse()
        .map_err(|_| anyhow!("webhook.listen: expected IP:port, got {:?}", w.listen))?;
    let segment = w.path.trim_start_matches('/');
    if segment.len() < 16
        || segment.len() > 128
        || !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("webhook.path: one random segment of 16-128 characters [A-Za-z0-9_-] expected");
    }
    check_range(
        "webhook.max_body_bytes",
        w.max_body_bytes,
        64 * 1024,
        25 * 1024 * 1024,
    )?;
    check_range("webhook.max_connections", w.max_connections, 1, 4096)?;
    check_range(
        "webhook.requests_per_minute",
        w.requests_per_minute,
        1,
        100_000,
    )?;
    let mut trusted_proxies = Vec::new();
    for p in &w.trusted_proxies {
        let net = p
            .parse::<ipnet::IpNet>()
            .or_else(|_| p.parse::<IpAddr>().map(ipnet::IpNet::from))
            .map_err(|_| anyhow!("webhook.trusted_proxies: invalid address {p:?}"))?;
        trusted_proxies.push(net);
    }
    Ok(Webhook {
        listen,
        path: format!("/{segment}"),
        max_body_bytes: w.max_body_bytes,
        ip_filter: w.ip_filter,
        trusted_proxies,
        max_connections: w.max_connections,
        requests_per_minute: w.requests_per_minute,
        tls: w.tls.map(|t| Tls {
            cert_file: t.cert_file,
            key_file: t.key_file,
        }),
    })
}

fn validate(raw: RawConfig) -> Result<Config> {
    if let Some(tz) = &raw.timezone {
        TimeZone::get(tz).map_err(|_| anyhow!("timezone: unknown IANA zone {tz:?}"))?;
    }

    let quiet_hours = QuietHours {
        enabled: raw.quiet_hours.enabled,
        start: parse_time("quiet_hours.start", &raw.quiet_hours.start)?,
        end: parse_time("quiet_hours.end", &raw.quiet_hours.end)?,
    };

    check_range(
        "intervals.guides_minutes",
        raw.intervals.guides_minutes,
        5,
        1440,
    )?;
    check_range(
        "intervals.games_minutes",
        raw.intervals.games_minutes,
        5,
        1440,
    )?;
    check_range(
        "intervals.github_reconcile_minutes",
        raw.intervals.github_reconcile_minutes,
        10,
        1440,
    )?;
    check_range("digest.window_seconds", raw.digest.window_seconds, 1, 600)?;
    check_range("digest.threshold", raw.digest.threshold, 1, 1000)?;

    if raw.games.appids.len() > 50 {
        bail!("games.appids: at most 50 games");
    }
    let unique: BTreeSet<_> = raw.games.appids.iter().collect();
    if unique.len() != raw.games.appids.len() {
        bail!("games.appids: duplicate AppID");
    }
    if raw.games.appids.contains(&0) {
        bail!("games.appids: AppID 0 is invalid");
    }
    if raw.games.news_feeds.is_empty() || raw.games.news_feeds.len() > 10 {
        bail!("games.news_feeds: 1 to 10 feed names expected");
    }
    for f in &raw.games.news_feeds {
        if f.is_empty()
            || f.len() > 64
            || !f.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            bail!("games.news_feeds: invalid feed name {f:?}");
        }
    }
    check_range(
        "games.merge_window_minutes",
        raw.games.merge_window_minutes,
        0,
        1440,
    )?;

    let s = raw.strangers;
    if s.phrases.len() > 100 {
        bail!("strangers.phrases: at most 100 phrases");
    }
    if s.phrases
        .iter()
        .any(|p| p.trim().is_empty() || p.chars().count() > 1000)
    {
        bail!("strangers.phrases: phrases must be 1..=1000 characters");
    }
    check_range(
        "strangers.reply_interval_hours",
        s.reply_interval_hours,
        1,
        720,
    )?;
    check_range("strangers.replies_per_hour", s.replies_per_hour, 0, 1000)?;
    check_range("strangers.retention_days", s.retention_days, 1, 365)?;

    check_range("github.poll_seconds", raw.github.poll_seconds, 30, 600)?;
    for login in &raw.github.bot_allowlist {
        if login.is_empty() || login.len() > 64 {
            bail!("github.bot_allowlist: invalid login {login:?}");
        }
    }

    let webhook = raw.webhook.map(validate_webhook).transpose()?;

    if !raw.health.listen.ip().is_loopback() {
        bail!("health.listen: must be a loopback address");
    }
    if !matches!(
        raw.logging.level.as_str(),
        "error" | "warn" | "info" | "debug" | "trace"
    ) {
        bail!("logging.level: one of error, warn, info, debug, trace expected");
    }
    check_range(
        "storage.retention_days",
        raw.storage.retention_days,
        7,
        3650,
    )?;
    check_range("storage.backups", raw.storage.backups, 1, 100)?;
    if raw.storage.path.as_os_str().is_empty() {
        bail!("storage.path: must not be empty");
    }

    Ok(Config {
        timezone_name: raw.timezone,
        language: raw.language,
        quiet_hours,
        intervals: raw.intervals,
        digest: raw.digest,
        games: raw.games,
        strangers: Strangers {
            phrases: s.phrases,
            reply_interval_hours: s.reply_interval_hours,
            replies_per_hour: s.replies_per_hour,
            retention_days: s.retention_days,
        },
        github: raw.github,
        webhook,
        health: raw.health,
        logging: raw.logging,
        storage: raw.storage,
    })
}

/// GitHub App credentials; present only when GitHub is configured.
pub struct GithubSecrets {
    pub app_id: u64,
    pub app_key: SecretBox<Vec<u8>>,
    pub webhook_secret: Option<SecretBox<Vec<u8>>>,
    pub admin_id: u64,
}

/// Secrets and identifiers from the environment. Only Telegram is required.
pub struct Secrets {
    pub telegram_token: SecretString,
    pub telegram_admin_id: i64,
    pub github: Option<GithubSecrets>,
    pub steam_api_key: Option<SecretString>,
    pub steam_profile_id: Option<u64>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets([hidden])")
    }
}

pub const ENV_VARS: &[&str] = &[
    "HQW_TELEGRAM_TOKEN",
    "HQW_TELEGRAM_ADMIN_ID",
    "HQW_GITHUB_APP_ID",
    "HQW_GITHUB_APP_KEY",
    "HQW_GITHUB_WEBHOOK_SECRET",
    "HQW_GITHUB_ADMIN_ID",
    "HQW_STEAM_API_KEY",
    "HQW_STEAM_PROFILE_ID",
];

fn positive<T: std::str::FromStr + PartialOrd + Default>(name: &str, v: &str) -> Result<T> {
    v.parse::<T>()
        .ok()
        .filter(|v| *v > T::default())
        .ok_or_else(|| anyhow!("{name}: positive integer expected"))
}

impl Secrets {
    /// Reads all variables, then removes them from the process environment.
    /// Must run before any other thread starts.
    pub fn from_env() -> Result<Self> {
        let mut values = std::collections::HashMap::new();
        for name in ENV_VARS {
            if let Some(v) = std::env::var_os(name) {
                let v = v
                    .into_string()
                    .map_err(|_| anyhow!("{name}: not valid UTF-8"))?;
                values.insert(*name, zeroize::Zeroizing::new(v));
            }
        }
        for name in ENV_VARS {
            std::env::remove_var(name);
        }
        Self::from_map(|name| values.get(name).map(|v| v.as_str()))
    }

    /// Reads `KEY=VALUE` lines from a file readable only by the service account.
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = zeroize::Zeroizing::new(
            std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?,
        );
        let values = parse_env_file(&text)?;
        Self::from_map(|name| values.get(name).map(|v| v.as_str()))
    }

    pub fn from_map<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self> {
        let opt =
            |name: &str| -> Option<&'a str> { get(name).map(str::trim).filter(|v| !v.is_empty()) };
        let req = |name: &str| -> Result<&'a str> {
            opt(name).ok_or_else(|| anyhow!("environment variable {name} is not set"))
        };
        let token = req("HQW_TELEGRAM_TOKEN")?;
        if !valid_telegram_token(token) {
            bail!("HQW_TELEGRAM_TOKEN: invalid format (expected <digits>:<35+ chars>)");
        }
        let telegram_admin_id: i64 =
            positive("HQW_TELEGRAM_ADMIN_ID", req("HQW_TELEGRAM_ADMIN_ID")?)?;

        let gh_vars = [
            "HQW_GITHUB_APP_ID",
            "HQW_GITHUB_APP_KEY",
            "HQW_GITHUB_ADMIN_ID",
        ];
        let gh_set = gh_vars.iter().filter(|n| opt(n).is_some()).count();
        let github = match gh_set {
            0 => {
                if opt("HQW_GITHUB_WEBHOOK_SECRET").is_some() {
                    bail!("HQW_GITHUB_WEBHOOK_SECRET is set but the GitHub App variables are not");
                }
                None
            }
            3 => {
                let webhook_secret = match opt("HQW_GITHUB_WEBHOOK_SECRET") {
                    Some(w) => {
                        check_webhook_secret(w)?;
                        Some(SecretBox::new(Box::new(w.as_bytes().to_vec())))
                    }
                    None => None,
                };
                Some(GithubSecrets {
                    app_id: positive("HQW_GITHUB_APP_ID", req("HQW_GITHUB_APP_ID")?)?,
                    app_key: SecretBox::new(Box::new(parse_private_key(req(
                        "HQW_GITHUB_APP_KEY",
                    )?)?)),
                    webhook_secret,
                    admin_id: positive("HQW_GITHUB_ADMIN_ID", req("HQW_GITHUB_ADMIN_ID")?)?,
                })
            }
            _ => {
                let missing: Vec<&str> = gh_vars
                    .iter()
                    .filter(|n| opt(n).is_none())
                    .copied()
                    .collect();
                bail!(
                    "GitHub is partly configured, missing: {}",
                    missing.join(", ")
                );
            }
        };

        let steam_api_key = match opt("HQW_STEAM_API_KEY") {
            Some(k) => {
                if k.len() != 32 || !k.bytes().all(|b| b.is_ascii_hexdigit()) {
                    bail!("HQW_STEAM_API_KEY: 32 hex characters expected");
                }
                Some(SecretString::from(k.to_ascii_uppercase()))
            }
            None => None,
        };
        let steam_profile_id = match opt("HQW_STEAM_PROFILE_ID") {
            Some(v) => Some(
                v.parse()
                    .ok()
                    .filter(|v| crate::steam::is_individual_steam_id(*v))
                    .ok_or_else(|| anyhow!("HQW_STEAM_PROFILE_ID: SteamID64 of a user expected"))?,
            ),
            None => None,
        };
        Ok(Self {
            telegram_token: SecretString::from(token.to_owned()),
            telegram_admin_id,
            github,
            steam_api_key,
            steam_profile_id,
        })
    }

    /// Values to search for in logs.
    pub fn all_values(&self) -> Vec<String> {
        let mut v = vec![self.telegram_token.expose_secret().to_owned()];
        if let Some(w) = self.github.as_ref().and_then(|g| g.webhook_secret.as_ref()) {
            v.push(String::from_utf8_lossy(w.expose_secret()).into_owned());
        }
        if let Some(k) = &self.steam_api_key {
            v.push(k.expose_secret().to_owned());
        }
        v
    }
}

/// `KEY=VALUE` lines; blank lines and `#` comments are skipped, unknown keys rejected.
fn parse_env_file(
    text: &str,
) -> Result<std::collections::HashMap<&'static str, zeroize::Zeroizing<String>>> {
    let mut out = std::collections::HashMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("secrets file line {}: KEY=VALUE expected", n + 1))?;
        let key = ENV_VARS
            .iter()
            .find(|e| **e == k.trim())
            .ok_or_else(|| anyhow!("secrets file line {}: unknown key", n + 1))?;
        out.insert(*key, zeroize::Zeroizing::new(v.trim().to_owned()));
    }
    Ok(out)
}

pub fn valid_telegram_token(t: &str) -> bool {
    let Some((id, rest)) = t.split_once(':') else {
        return false;
    };
    !id.is_empty()
        && id.len() <= 20
        && id.bytes().all(|b| b.is_ascii_digit())
        && rest.len() >= 30
        && rest.len() <= 100
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Accepts a PEM key (PKCS#1 or PKCS#8), with real or escaped newlines, and returns DER.
pub fn parse_private_key(value: &str) -> Result<Vec<u8>> {
    use rustls_pki_types::pem::PemObject;
    let pem = zeroize::Zeroizing::new(value.replace("\\n", "\n"));
    let der = rustls_pki_types::PrivateKeyDer::from_pem_slice(pem.as_bytes())
        .map_err(|_| anyhow!("HQW_GITHUB_APP_KEY: not a valid PEM private key"))?;
    let (bytes, pkcs8) = match &der {
        rustls_pki_types::PrivateKeyDer::Pkcs1(k) => (k.secret_pkcs1_der().to_vec(), false),
        rustls_pki_types::PrivateKeyDer::Pkcs8(k) => (k.secret_pkcs8_der().to_vec(), true),
        _ => bail!("HQW_GITHUB_APP_KEY: RSA key expected"),
    };
    let ok = if pkcs8 {
        ring::rsa::KeyPair::from_pkcs8(&bytes).is_ok()
    } else {
        ring::rsa::KeyPair::from_der(&bytes).is_ok()
    };
    if !ok {
        bail!("HQW_GITHUB_APP_KEY: not a usable RSA key");
    }
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(u8::from(pkcs8));
    out.extend_from_slice(&bytes);
    Ok(out)
}

pub fn check_webhook_secret(s: &str) -> Result<()> {
    if s.len() < 32 {
        bail!("HQW_GITHUB_WEBHOOK_SECRET: at least 32 bytes of random data expected");
    }
    let distinct: BTreeSet<u8> = s.bytes().collect();
    if distinct.len() < 12 {
        bail!("HQW_GITHUB_WEBHOOK_SECRET: looks too weak, generate it randomly");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = r#"
timezone = "Europe/Moscow"
[webhook]
path = "abcdefghijklmnop1234"
"#;

    #[test]
    fn minimal_config_gets_defaults() {
        let c = Config::parse(MIN).unwrap();
        assert_eq!(c.digest.threshold, 5);
        assert_eq!(c.games.appids, vec![572_890, 2_523_120, 588_430]);
        assert_eq!(c.strangers.phrases, [] as [std::string::String; 0]);
        assert_eq!(
            stranger_phrases(&c.strangers.phrases).len(),
            DEFAULT_PHRASES.len()
        );
        assert_eq!(DEFAULT_PHRASES_EN.len(), DEFAULT_PHRASES.len());
        let w = c.webhook.unwrap();
        assert_eq!(w.path, "/abcdefghijklmnop1234");
        assert!(w.ip_filter);
        assert_eq!(c.github.mode, GithubMode::Webhook);
    }

    #[test]
    fn example_config_is_valid() {
        let c = Config::parse(include_str!("../deploy/config.example.toml")).unwrap();
        assert_eq!(c.strangers.phrases.len(), DEFAULT_PHRASES.len());
        assert_eq!(c.timezone_name.as_deref(), Some("Europe/Moscow"));
    }

    #[test]
    fn empty_config_is_valid() {
        let c = Config::parse("").unwrap();
        assert!(c.webhook.is_none() && c.timezone_name.is_none());
        let c = Config::parse("language = \"en\"\n[github]\nmode = \"polling\"\n").unwrap();
        assert_eq!(c.github.mode, GithubMode::Polling);
        assert_eq!(c.language, Some(crate::i18n::Lang::En));
        assert!(Config::parse("[github]\npoll_seconds = 5\n").is_err());
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> + 'a {
        move |n| pairs.iter().find(|(k, _)| *k == n).map(|(_, v)| *v)
    }

    #[test]
    fn only_telegram_is_required() {
        let tok = "123456:ABCdefGHIjklMNOpqrSTUvwxYZ0123456789_-";
        let s = Secrets::from_map(env(&[
            ("HQW_TELEGRAM_TOKEN", tok),
            ("HQW_TELEGRAM_ADMIN_ID", "42"),
        ]))
        .unwrap();
        assert!(s.github.is_none() && s.steam_api_key.is_none() && s.steam_profile_id.is_none());
        let err = Secrets::from_map(env(&[
            ("HQW_TELEGRAM_TOKEN", tok),
            ("HQW_TELEGRAM_ADMIN_ID", "42"),
            ("HQW_GITHUB_APP_ID", "1"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("HQW_GITHUB_APP_KEY"), "{err}");
        assert!(Secrets::from_map(env(&[("HQW_TELEGRAM_ADMIN_ID", "42")])).is_err());
        let s = Secrets::from_map(env(&[
            ("HQW_TELEGRAM_TOKEN", tok),
            ("HQW_TELEGRAM_ADMIN_ID", "42"),
            ("HQW_STEAM_PROFILE_ID", "76561199508874215"),
        ]))
        .unwrap();
        assert_eq!(s.steam_profile_id, Some(76_561_199_508_874_215));
    }

    #[test]
    fn unknown_key_and_secret_rejected() {
        let err = Config::parse(&format!("{MIN}\nfoo = 1\n")).unwrap_err();
        assert!(err.to_string().contains("foo"), "{err}");
        let err = Config::parse(&MIN.replace("[webhook]", "telegram_token = \"x\"\n[webhook]"))
            .unwrap_err();
        assert!(
            err.to_string().contains("secrets are not accepted"),
            "{err}"
        );
    }

    #[test]
    fn range_errors_name_the_field() {
        let err = Config::parse(&format!("{MIN}\n[digest]\nthreshold = 0\n")).unwrap_err();
        assert!(err.to_string().contains("digest.threshold"), "{err}");
        let err = Config::parse(&MIN.replace("abcdefghijklmnop1234", "short")).unwrap_err();
        assert!(err.to_string().contains("webhook.path"), "{err}");
        let err = Config::parse(&MIN.replace("Europe/Moscow", "Mars/Base")).unwrap_err();
        assert!(err.to_string().contains("timezone"), "{err}");
    }

    #[test]
    fn env_file_lines() {
        let m =
            parse_env_file("# c\n\nHQW_TELEGRAM_ADMIN_ID = 42\nHQW_GITHUB_APP_KEY=a=b\n").unwrap();
        assert_eq!(
            m.get("HQW_TELEGRAM_ADMIN_ID").map(|v| v.as_str()),
            Some("42")
        );
        assert_eq!(m.get("HQW_GITHUB_APP_KEY").map(|v| v.as_str()), Some("a=b"));
        assert!(parse_env_file("FOO=1").is_err());
        assert!(parse_env_file("HQW_TELEGRAM_TOKEN").is_err());
    }

    #[test]
    fn webhook_secret_strength() {
        assert!(check_webhook_secret("short").is_err());
        assert!(check_webhook_secret(&"a".repeat(64)).is_err());
        assert!(check_webhook_secret("3f9a1c7e5b2d4f6a8c0e1b3d5f7a9c2e4b6d8f0a1c3e5b7d").is_ok());
    }

    #[test]
    fn secrets_are_hidden_in_debug() {
        let key = "-----BEGIN PRIVATE KEY-----
AAAA
-----END PRIVATE KEY-----";
        assert!(parse_private_key(key).is_err());
        let s = format!("{:?}", SecretString::from("123:SECRET".to_owned()));
        assert!(!s.contains("SECRET"));
    }

    #[test]
    fn telegram_token_format() {
        assert!(valid_telegram_token(
            "123456:ABCdefGHIjklMNOpqrSTUvwxYZ0123456789_-"
        ));
        assert!(!valid_telegram_token("123456"));
        assert!(!valid_telegram_token(
            "abc:ABCdefGHIjklMNOpqrSTUvwxYZ0123456789"
        ));
    }
}
