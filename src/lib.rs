// SPDX-License-Identifier: GPL-3.0-or-later
//! HQ Watchman: Telegram notifications about GitHub, Steam guides and Steam games.

pub mod app;
pub mod config;
pub mod event;
pub mod github;
pub mod health;
pub mod net;
pub mod steam;
pub mod storage;
pub mod telegram;
pub mod text;

use std::sync::RwLock;

use jiff::tz::TimeZone;

static TIMEZONE: RwLock<Option<TimeZone>> = RwLock::new(None);

pub fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

pub fn set_timezone(tz: TimeZone) {
    let mut g = TIMEZONE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *g = Some(tz);
}

pub fn timezone() -> TimeZone {
    TIMEZONE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or(TimeZone::UTC)
}

/// Unix time as local date and time of the admin.
pub fn fmt_time(ts: i64) -> String {
    jiff::Timestamp::from_second(ts).map_or_else(
        |_| ts.to_string(),
        |t| {
            t.to_zoned_datetime(timezone())
                .strftime("%d.%m.%Y %H:%M")
                .to_string()
        },
    )
}

/// Seconds as a short Russian duration, e.g. "2 д 3 ч".
pub fn fmt_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, m) => format!("{m} мин"),
        (0, h, m) => format!("{h} ч {m} мин"),
        (d, h, _) => format!("{d} д {h} ч"),
    }
}
