// SPDX-License-Identifier: GPL-3.0-or-later
//! Long polling, admin commands, groups and strangers.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;

use super::api::{Message, TgError, Update};
use crate::app::App;
use crate::event::Event;
use crate::text::{escape_html, external_html};

const STALE_AFTER: i64 = 300;

pub const COMMANDS: &[(&str, &str)] = &[
    ("help", "список команд"),
    ("status", "состояние бота и источников"),
    ("sources", "что отслеживается"),
    ("pause", "пауза уведомлений, например /pause 2h"),
    ("resume", "снять паузу"),
    ("check_now", "проверить сейчас: github, guides или games"),
    ("reload", "перечитать конфиг"),
    ("strangers", "посторонние за 7 дней"),
];

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Status,
    Sources,
    Pause(Option<i64>),
    Resume,
    CheckNow(Option<Target>),
    Reload,
    Strangers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Github,
    Guides,
    Games,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    NotACommand,
    Unknown,
    BadArgs(&'static str),
}

/// Duration like `30m`, `2h`, `1d`, `1w` or `1h30m`; at most one year.
pub fn parse_duration(s: &str) -> Option<i64> {
    if s.is_empty() || s.len() > 20 {
        return None;
    }
    let mut total: i64 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: i64 = num.parse().ok()?;
        num.clear();
        let unit = match c.to_ascii_lowercase() {
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(unit)?)?;
    }
    if !num.is_empty() || total <= 0 || total > 365 * 86_400 {
        return None;
    }
    Some(total)
}

pub fn parse_command(text: &str) -> Result<Command, ParseError> {
    let text = text.trim();
    let Some(rest) = text.strip_prefix('/') else {
        return Err(ParseError::NotACommand);
    };
    let mut parts = rest.split_whitespace();
    let head = parts.next().unwrap_or_default();
    let name = head
        .split('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let args: Vec<&str> = parts.collect();
    if args.len() > 1 {
        return Err(ParseError::BadArgs("слишком много аргументов"));
    }
    let arg = args.first().copied();
    let no_args = |c: Command| {
        if arg.is_some() {
            Err(ParseError::BadArgs("команда без аргументов"))
        } else {
            Ok(c)
        }
    };
    match name.as_str() {
        "start" | "help" => no_args(Command::Help),
        "status" => no_args(Command::Status),
        "sources" => no_args(Command::Sources),
        "resume" => no_args(Command::Resume),
        "reload" => no_args(Command::Reload),
        "strangers" => no_args(Command::Strangers),
        "pause" => match arg {
            None => Ok(Command::Pause(None)),
            Some(a) => {
                parse_duration(a)
                    .map(|d| Command::Pause(Some(d)))
                    .ok_or(ParseError::BadArgs(
                        "длительность вида 30m, 2h, 1d, 1w или 1h30m, не больше года",
                    ))
            }
        },
        "check_now" => match arg.map(str::to_ascii_lowercase).as_deref() {
            None => Ok(Command::CheckNow(None)),
            Some("github") => Ok(Command::CheckNow(Some(Target::Github))),
            Some("guides") => Ok(Command::CheckNow(Some(Target::Guides))),
            Some("games") => Ok(Command::CheckNow(Some(Target::Games))),
            Some(_) => Err(ParseError::BadArgs("источник: github, guides или games")),
        },
        _ => Err(ParseError::Unknown),
    }
}

fn help_text() -> String {
    let mut s = String::from("<b>Команды</b>");
    for (c, d) in COMMANDS {
        let _ = write!(s, "\n/{c}: {}", escape_html(d));
    }
    s
}

/// Request from the poller to the stranger worker.
pub struct StrangerHit {
    pub user_id: i64,
    pub chat_id: i64,
    pub username: Option<String>,
    pub stale: bool,
}

pub struct Poller {
    app: Arc<App>,
    last_check: std::collections::HashMap<&'static str, i64>,
}

impl Poller {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            last_check: std::collections::HashMap::new(),
        }
    }

    async fn reply(&self, update_id: i64, html: &str) -> Result<()> {
        self.app
            .db
            .enqueue_reply(&format!("reply:{update_id}"), html)
            .await?;
        self.app.outbox_signal.notify_one();
        Ok(())
    }

    async fn on_group(
        &self,
        chat_id: i64,
        title: Option<&str>,
        kind: &str,
        by: Option<&super::api::User>,
    ) -> Result<()> {
        match self.app.telegram.leave_chat(chat_id).await {
            Ok(_) | Err(TgError::Blocked(_)) => {}
            Err(e) => tracing::warn!(error = %e, "leaveChat failed"),
        }
        let who = by.map_or_else(String::new, |u| {
            format!(
                " Добавил: {} (ID {}).",
                u.username.as_deref().map_or_else(
                    || "без имени пользователя".to_owned(),
                    |n| format!("@{}", crate::text::clean_line(n, 64))
                ),
                u.id
            )
        });
        let ev = Event::service(
            format!("bot:left:{chat_id}:{}", crate::now() / 60),
            "Бот вышел из чата",
            format!(
                "Бота добавили в {} «{}», он сразу вышел.{who}",
                match kind {
                    "channel" => "канал",
                    _ => "группу",
                },
                crate::text::clean_line(title.unwrap_or("без названия"), 100)
            ),
        );
        self.app.db.enqueue(&ev).await?;
        Ok(())
    }

    async fn handle(&mut self, u: Update) -> Result<()> {
        if let Some(m) = &u.my_chat_member {
            if m.chat.kind != "private"
                && matches!(
                    m.new_chat_member.status.as_str(),
                    "member" | "administrator" | "restricted"
                )
            {
                self.on_group(
                    m.chat.id,
                    m.chat.title.as_deref(),
                    &m.chat.kind,
                    Some(&m.from),
                )
                .await?;
            }
            return Ok(());
        }
        let Some(msg) = u.message else { return Ok(()) };
        if msg.chat.kind != "private" {
            self.on_group(
                msg.chat.id,
                msg.chat.title.as_deref(),
                &msg.chat.kind,
                msg.from.as_ref(),
            )
            .await?;
            return Ok(());
        }
        let Some(from) = &msg.from else { return Ok(()) };
        let stale = crate::now() - msg.date > STALE_AFTER;
        if from.id != self.app.secrets.telegram_admin_id || msg.chat.id != from.id {
            let tx = self
                .app
                .stranger_tx
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let (false, Some(tx)) = (from.is_bot, tx) {
                let _ = tx.try_send(StrangerHit {
                    user_id: from.id,
                    chat_id: msg.chat.id,
                    username: from
                        .username
                        .as_deref()
                        .map(|n| crate::text::clean_line(n, 64)),
                    stale,
                });
            }
            return Ok(());
        }
        if stale {
            if msg
                .text
                .as_deref()
                .is_some_and(|t| t.trim_start().starts_with('/'))
            {
                self.reply(u.update_id, "Команда устарела (отправлена больше 5 минут назад) и не выполнена. Повторите её.").await?;
            }
            return Ok(());
        }
        self.admin_message(u.update_id, &msg).await
    }

    async fn admin_message(&mut self, update_id: i64, msg: &Message) -> Result<()> {
        let text = msg.text.as_deref().unwrap_or_default();
        let cmd = match parse_command(text) {
            Ok(c) => c,
            Err(ParseError::BadArgs(why)) => {
                return self
                    .reply(
                        update_id,
                        &format!(
                            "Неверные аргументы: {}.\n\n{}",
                            escape_html(why),
                            help_text()
                        ),
                    )
                    .await;
            }
            Err(_) => return self.reply(update_id, &help_text()).await,
        };
        let answer = match cmd {
            Command::Help => help_text(),
            Command::Status => self.status().await?,
            Command::Sources => self.sources().await?,
            Command::Pause(d) => {
                let value =
                    d.map_or_else(|| "forever".to_owned(), |d| (crate::now() + d).to_string());
                self.app.db.meta_set("paused_until", &value).await?;
                match d {
                    Some(d) => format!(
                        "Пауза до {} ({}). События собираются и придут после паузы.",
                        crate::fmt_time(crate::now() + d),
                        crate::fmt_duration(d)
                    ),
                    None => "Пауза без срока. События собираются, снять паузу: /resume.".to_owned(),
                }
            }
            Command::Resume => {
                let was = super::sender::pause_state(&self.app).await?.is_some();
                self.app.db.meta_del("paused_until").await?;
                let pending = self.app.db.pending_count().await?;
                if was {
                    format!("Пауза снята. В очереди: {pending}.")
                } else {
                    "Паузы не было.".to_owned()
                }
            }
            Command::CheckNow(t) => self.check_now(t),
            Command::Reload => self.app.reload(),
            Command::Strangers => self.strangers_report().await?,
        };
        self.reply(update_id, &answer).await
    }

    fn check_now(&mut self, t: Option<Target>) -> String {
        let targets: Vec<Target> = t.map_or_else(
            || vec![Target::Github, Target::Guides, Target::Games],
            |t| vec![t],
        );
        let now = crate::now();
        let mut started = Vec::new();
        let mut skipped = Vec::new();
        for t in targets {
            let (key, name, notify) = match t {
                Target::Github => ("github", "GitHub", &self.app.check_now.github),
                Target::Guides => ("guides", "руководства", &self.app.check_now.guides),
                Target::Games => ("games", "игры", &self.app.check_now.games),
            };
            let last = self.last_check.get(key).copied().unwrap_or(0);
            if now - last < 60 {
                skipped.push(name);
                continue;
            }
            self.last_check.insert(key, now);
            notify.notify_one();
            started.push(name);
        }
        let mut s = String::new();
        if !started.is_empty() {
            let _ = write!(s, "Проверка запущена: {}.", started.join(", "));
        }
        if !skipped.is_empty() {
            let _ = write!(
                s,
                "{}Не чаще раза в минуту, пропущено: {}.",
                if s.is_empty() { "" } else { "\n" },
                skipped.join(", ")
            );
        }
        s
    }

    async fn status(&self) -> Result<String> {
        let cfg = self.app.cfg();
        let now = crate::now();
        let mut s = format!(
            "<b>Статус</b>\nВерсия: {} ({})\nРаботает: {}",
            env!("CARGO_PKG_VERSION"),
            escape_html(option_env!("HQW_COMMIT").unwrap_or("dev")),
            crate::fmt_duration(now - self.app.health.started())
        );
        let pause = match super::sender::pause_state(&self.app).await? {
            None => "нет".to_owned(),
            Some(None) => "без срока".to_owned(),
            Some(Some(t)) => format!("до {}", crate::fmt_time(t)),
        };
        let quiet = super::sender::is_quiet(&cfg.quiet_hours, now, &cfg.timezone);
        let _ = write!(
            s,
            "\nПауза: {pause}\nОчередь: {}\nТихие часы: {}",
            self.app.db.pending_count().await?,
            if quiet {
                "сейчас активны"
            } else {
                "сейчас нет"
            }
        );
        s.push_str("\n\n<b>Источники</b>");
        let snap = self.app.health.snapshot();
        for (name, title) in crate::health::SUBSYSTEMS {
            let st = snap.get(name).cloned().unwrap_or_default();
            let ok = st
                .last_success
                .map_or_else(|| "ещё не было".to_owned(), crate::fmt_time);
            let _ = write!(s, "\n{}: успех {ok}", escape_html(title));
            if let Some((t, e)) = &st.last_error {
                let _ = write!(
                    s,
                    "; ошибка {}: {}",
                    crate::fmt_time(*t),
                    external_html(&crate::text::truncate_chars(e, 200))
                );
            }
            if st.alerted {
                s.push_str(" [сбой]");
            }
        }
        Ok(s)
    }

    async fn sources(&self) -> Result<String> {
        let mut s = String::from("<b>Репозитории GitHub</b>");
        let repos = self.app.db.repos().await?;
        if repos.is_empty() {
            s.push_str("\nпока нет");
        }
        for r in &repos {
            let _ = write!(
                s,
                "\n<code>{}</code>{}",
                escape_html(&r.full_name),
                if r.private {
                    " (приватный)"
                } else {
                    ""
                }
            );
        }
        s.push_str("\n\n<b>Руководства Steam</b>");
        let guides = self.app.db.guides().await?;
        if guides.is_empty() {
            s.push_str("\nпока нет");
        }
        for g in &guides {
            let _ = write!(
                s,
                "\n{} ({}, комментариев: {})",
                external_html(&g.title),
                external_html(&g.game),
                g.comment_count.max(0)
            );
        }
        s.push_str("\n\n<b>Игры Steam</b>");
        for (appid, _, state) in self.app.db.games().await? {
            let Ok(st) = serde_json::from_str::<crate::steam::games::GameState>(&state) else {
                continue;
            };
            let public = st
                .branches
                .get("public")
                .map_or("-", |b| b.buildid.as_str());
            let _ = write!(
                s,
                "\n<b>{}</b> ({appid}), public: {}",
                external_html(&st.name),
                escape_html(public)
            );
            let others: Vec<String> = st
                .branches
                .iter()
                .filter(|(n, _)| n.as_str() != "public")
                .map(|(n, b)| {
                    format!(
                        "{n} {}{}",
                        b.buildid,
                        if b.password {
                            " (с паролем)"
                        } else {
                            ""
                        }
                    )
                })
                .collect();
            if !others.is_empty() {
                let _ = write!(s, "\n  ветки: {}", external_html(&others.join(", ")));
            }
            let dlcs: Vec<String> = st
                .dlcs
                .iter()
                .filter(|(_, d)| !d.music)
                .map(|(id, d)| format!("{} ({id})", d.name))
                .collect();
            if !dlcs.is_empty() {
                let _ = write!(s, "\n  DLC: {}", external_html(&dlcs.join(", ")));
            }
        }
        if crate::telegram::render::visible_len(&s) > crate::telegram::render::MAX_MESSAGE {
            s = crate::text::truncate_chars(&s, 3500);
            s = s.rsplit_once('\n').map_or(s.clone(), |(a, _)| a.to_owned());
            s.push_str("\n…список обрезан");
        }
        Ok(s)
    }

    async fn strangers_report(&self) -> Result<String> {
        let rows = self
            .app
            .db
            .strangers_since(crate::now() - 7 * 86_400)
            .await?;
        if rows.is_empty() {
            return Ok("Посторонние за 7 дней не писали.".to_owned());
        }
        let mut s = String::from("<b>Посторонние за 7 дней</b>");
        for r in rows.iter().take(50) {
            let name = r
                .username
                .as_deref()
                .map_or_else(|| "без имени".to_owned(), |n| format!("@{n}"));
            let _ = write!(
                s,
                "\nID {}, {}, попыток: {}, последняя: {}",
                r.user_id,
                external_html(&name),
                r.attempts,
                crate::fmt_time(r.last_at)
            );
        }
        Ok(s)
    }

    pub async fn run(mut self) -> Result<()> {
        let mut offset = 0i64;
        let mut delay = 1u64;
        loop {
            self.app.health.beat("telegram_poll", 180);
            let updates = tokio::select! {
                () = self.app.cancel.cancelled() => return Ok(()),
                r = self.app.telegram.get_updates(offset, 50) => r,
            };
            match updates {
                Ok(list) => {
                    delay = 1;
                    self.app.health.ok("telegram").await;
                    for u in list {
                        offset = offset.max(u.update_id + 1);
                        let id = u.update_id;
                        if let Err(e) = self.handle(u).await {
                            tracing::error!(update = id, error = %e, "update handling failed");
                        }
                    }
                }
                Err(TgError::RetryAfter(s)) => {
                    tokio::time::sleep(Duration::from_secs(s.min(600))).await;
                }
                Err(e) => {
                    self.app.health.record_error("telegram", &e.to_string());
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    delay = (delay * 2).min(60);
                }
            }
        }
    }
}

/// Answers strangers with its own budget, apart from admin delivery.
pub async fn stranger_worker(app: Arc<App>, mut rx: mpsc::Receiver<StrangerHit>) -> Result<()> {
    let mut sent_times: std::collections::VecDeque<i64> = std::collections::VecDeque::new();
    loop {
        let hit = tokio::select! {
            () = app.cancel.cancelled() => return Ok(()),
            h = rx.recv() => match h { Some(h) => h, None => return Ok(()) },
        };
        let cfg = app.cfg();
        let now = crate::now();
        while sent_times.front().is_some_and(|t| now - *t >= 3600) {
            sent_times.pop_front();
        }
        let prev = app.db.stranger(hit.user_id).await?;
        let interval = i64::try_from(cfg.strangers.reply_interval_hours).unwrap_or(24) * 3600;
        let due = prev
            .as_ref()
            .and_then(|p| p.last_reply_at)
            .is_none_or(|t| now - t >= interval);
        let budget =
            sent_times.len() < usize::try_from(cfg.strangers.replies_per_hour).unwrap_or(0);
        let mut replied = false;
        if due && budget && !hit.stale {
            let idx = rand::random_range(0..cfg.strangers.phrases.len().max(1));
            if let Some(phrase) = cfg.strangers.phrases.get(idx) {
                match app.telegram.send_plain(hit.chat_id, phrase).await {
                    Ok(_) => {
                        replied = true;
                        sent_times.push_back(now);
                    }
                    Err(TgError::RetryAfter(s)) => {
                        tokio::time::sleep(Duration::from_secs(s.min(600))).await;
                    }
                    Err(e) => tracing::debug!(error = %e, "stranger reply failed"),
                }
            }
        }
        app.db
            .note_stranger(hit.user_id, hit.username.as_deref(), replied)
            .await?;
        tracing::info!(user = hit.user_id, replied, "message from a stranger");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("2h"), Some(7200));
        assert_eq!(parse_duration("1d"), Some(86_400));
        assert_eq!(parse_duration("1h30m"), Some(5400));
        assert_eq!(parse_duration("1w"), Some(604_800));
        assert_eq!(parse_duration("0h"), None);
        assert_eq!(parse_duration("2"), None);
        assert_eq!(parse_duration("h"), None);
        assert_eq!(parse_duration("400d"), None);
        assert_eq!(parse_duration("99999999999999999999h"), None);
    }

    #[test]
    fn commands() {
        assert_eq!(parse_command("/start"), Ok(Command::Help));
        assert_eq!(parse_command("/status@hq_bot"), Ok(Command::Status));
        assert_eq!(parse_command("/pause"), Ok(Command::Pause(None)));
        assert_eq!(parse_command("/pause 1d"), Ok(Command::Pause(Some(86_400))));
        assert!(matches!(
            parse_command("/pause soon"),
            Err(ParseError::BadArgs(_))
        ));
        assert_eq!(
            parse_command("/check_now games"),
            Ok(Command::CheckNow(Some(Target::Games)))
        );
        assert!(matches!(
            parse_command("/check_now steam"),
            Err(ParseError::BadArgs(_))
        ));
        assert!(matches!(
            parse_command("/status now"),
            Err(ParseError::BadArgs(_))
        ));
        assert_eq!(parse_command("hello"), Err(ParseError::NotACommand));
        assert_eq!(parse_command("/rm -rf"), Err(ParseError::Unknown));
    }

    proptest! {
        #[test]
        fn never_panics(s in any::<String>()) {
            let _ = parse_command(&s);
            let _ = parse_duration(&s);
        }
    }
}
