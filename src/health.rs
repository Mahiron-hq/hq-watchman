// SPDX-License-Identifier: GPL-3.0-or-later
//! Source health: last success and error, one alert per outage, liveness.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::event::Event;
use crate::storage::Db;

#[derive(Debug, Clone, Default)]
pub struct SourceState {
    pub last_success: Option<i64>,
    pub last_error: Option<(i64, String)>,
    pub failures: u32,
    pub failing_since: Option<i64>,
    pub alerted: bool,
    pub heartbeat: i64,
    pub max_silence: i64,
}

pub struct Health {
    db: Db,
    started: i64,
    states: Mutex<BTreeMap<&'static str, SourceState>>,
}

/// Names of tracked subsystems.
pub const SUBSYSTEMS: &[&str] = &[
    "telegram",
    "webhook",
    "github",
    "guides",
    "guides_parser",
    "games",
    "pics",
    "news",
    "deliveries",
    "storage",
];

pub fn title(name: &str) -> &'static str {
    match name {
        "telegram" => "Telegram",
        "webhook" => crate::tr!("Приём вебхуков GitHub", "GitHub webhook endpoint"),
        "github" => "GitHub API",
        "guides" => crate::tr!("Руководства Steam", "Steam guides"),
        "guides_parser" => crate::tr!("Парсер страниц Steam", "Steam page parser"),
        "games" => crate::tr!("Игры Steam", "Steam games"),
        "pics" => "Steam PICS",
        "news" => crate::tr!("Новости Steam", "Steam news"),
        "deliveries" => crate::tr!("Доставки вебхуков GitHub", "GitHub webhook deliveries"),
        "storage" => crate::tr!("База данных", "Database"),
        _ => crate::tr!("Подсистема", "Subsystem"),
    }
}

impl Health {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            started: crate::now(),
            states: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn started(&self) -> i64 {
        self.started
    }

    fn with<R>(&self, name: &'static str, f: impl FnOnce(&mut SourceState) -> R) -> R {
        let mut map = self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(map.entry(name).or_default())
    }

    pub fn snapshot(&self) -> BTreeMap<&'static str, SourceState> {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Marks a loop as alive; `max_silence` is how long it may stay quiet.
    pub fn beat(&self, name: &'static str, max_silence: i64) {
        let now = crate::now();
        self.with(name, |s| {
            s.heartbeat = now;
            s.max_silence = max_silence;
        });
    }

    pub fn stale(&self) -> Vec<&'static str> {
        let now = crate::now();
        self.snapshot()
            .into_iter()
            .filter(|(_, s)| s.max_silence > 0 && now - s.heartbeat > s.max_silence)
            .map(|(n, _)| n)
            .collect()
    }

    pub async fn ok(&self, name: &'static str) {
        let now = crate::now();
        let recovered = self.with(name, |s| {
            s.last_success = Some(now);
            s.failures = 0;
            let was = s.alerted.then_some(s.failing_since.unwrap_or(now));
            s.alerted = false;
            s.failing_since = None;
            was
        });
        if let Some(since) = recovered {
            let ev = Event::service(
                format!("bot:recover:{name}:{since}"),
                crate::trf!("{}: работа восстановлена", "{}: recovered", title(name)),
                crate::trf!(
                    "Источник снова работает. Сбой длился с {}.",
                    "The source works again. The outage started at {}.",
                    crate::fmt_time(since)
                ),
            );
            self.enqueue(&ev).await;
        }
    }

    /// Remembers an error without alerting (for Telegram itself).
    pub fn record_error(&self, name: &'static str, error: &str) {
        let now = crate::now();
        self.with(name, |s| {
            s.last_error = Some((now, error.to_owned()));
            s.failures = s.failures.saturating_add(1);
            s.failing_since.get_or_insert(now);
        });
        tracing::warn!(source = name, error = %crate::text::log_safe(error), "error");
    }

    /// Counts a failure; alerts once when `threshold` consecutive failures are reached.
    pub async fn fail(&self, name: &'static str, error: &str, threshold: u32, advice: &str) {
        let now = crate::now();
        let alert = self.with(name, |s| {
            s.last_error = Some((now, error.to_owned()));
            s.failures = s.failures.saturating_add(1);
            let since = *s.failing_since.get_or_insert(now);
            if !s.alerted && s.failures >= threshold {
                s.alerted = true;
                Some(since)
            } else {
                None
            }
        });
        tracing::warn!(source = name, error = %crate::text::log_safe(error), "source check failed");
        if let Some(since) = alert {
            let ev = Event::service(
                format!("bot:alert:{name}:{since}"),
                crate::trf!("{}: сбой", "{}: failure", title(name)),
                crate::trf!(
                    "Проблема: {error}\nС какого момента: {}\nЧто сделать: {advice}",
                    "Problem: {error}\nSince: {}\nWhat to do: {advice}",
                    crate::fmt_time(since)
                ),
            );
            self.enqueue(&ev).await;
        }
    }

    async fn enqueue(&self, ev: &Event) {
        if let Err(e) = self.db.enqueue(ev).await {
            tracing::error!(error = %e, "cannot queue service message");
        }
    }
}
