// SPDX-License-Identifier: GPL-3.0-or-later
//! The only path to Telegram: queue, digest, quiet hours, pause, limits and retries.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use jiff::civil::Time;

use super::api::TgError;
use super::render::{self, Item, MAX_MESSAGE};
use crate::app::App;
use crate::config::QuietHours;
use crate::event::{Body, Event, Source};
use crate::storage::{OutboxRow, KIND_EVENT, KIND_PARTS, KIND_REPLY, STATUS_PENDING};

const SEND_GAP: Duration = Duration::from_millis(1100);
const MAX_AGE: i64 = 30 * 86_400;
const MAX_ATTEMPTS_BAD_REQUEST: i64 = 5;

/// Quiet hours check in the admin time zone; the window may cross midnight.
pub fn is_quiet(q: &QuietHours, ts: i64, tz: &jiff::tz::TimeZone) -> bool {
    if !q.enabled || q.start == q.end {
        return false;
    }
    let Ok(t) = jiff::Timestamp::from_second(ts) else {
        return false;
    };
    let local: Time = t.to_zoned_datetime(tz.clone()).time();
    if q.start < q.end {
        local >= q.start && local < q.end
    } else {
        local >= q.start || local < q.end
    }
}

/// Pause state: `None` when not paused, `Some(None)` forever, `Some(Some(ts))` until ts.
pub async fn pause_state(app: &App) -> Result<Option<Option<i64>>> {
    let Some(v) = app.db.meta_get("paused_until").await? else {
        return Ok(None);
    };
    if v == "forever" {
        return Ok(Some(None));
    }
    match v.parse::<i64>() {
        Ok(ts) if ts > crate::now() => Ok(Some(Some(ts))),
        _ => {
            app.db.meta_del("paused_until").await?;
            Ok(None)
        }
    }
}

struct Decoded {
    row: OutboxRow,
    source: Source,
    body: Body,
}

fn decode(row: OutboxRow) -> Option<Decoded> {
    let body: Body = serde_json::from_str(&row.body).ok()?;
    let source = match row.source.as_str() {
        "github" => Source::Github,
        "steam_guide" => Source::SteamGuide,
        "steam_update" => Source::SteamUpdate,
        _ => Source::Bot,
    };
    Some(Decoded { row, source, body })
}

/// One file of a long message.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Part {
    name: String,
    caption: String,
    text: String,
}

/// Files of a long message that are still to be sent, queued as their own row.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Rest {
    parts: Vec<Part>,
    reply_to: Option<i64>,
    silent: bool,
}

enum Outcome {
    Sent(Option<i64>),
    /// Some files went out; the rest is queued separately.
    Partial(Option<i64>, Rest),
    Retry(String),
    Drop(String),
    Wait(u64),
    Blocked(String),
    Migrated(i64),
}

pub struct Sender {
    app: Arc<App>,
    blocked_logged: bool,
}

impl Sender {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            blocked_logged: false,
        }
    }

    fn chat(&self, chat: Option<i64>) -> i64 {
        chat.unwrap_or(self.app.secrets.telegram_admin_id)
    }

    fn quiet_now() -> bool {
        let s = crate::settings::current();
        is_quiet(&s.quiet(), crate::now(), &s.tz())
    }

    async fn reply_target(&self, row: &OutboxRow) -> Result<Option<i64>> {
        match row.reply_to {
            Some(id) => Ok(self.app.db.outbox_state(id).await?.and_then(|(_, m)| m)),
            None => Ok(None),
        }
    }

    async fn send_text(
        &self,
        chat: Option<i64>,
        html: &str,
        silent: bool,
        reply_to: Option<i64>,
    ) -> Outcome {
        classify(
            self.app
                .telegram
                .send_message(self.chat(chat), html, silent, reply_to)
                .await,
        )
    }

    async fn send_file(
        &self,
        chat: Option<i64>,
        name: &str,
        data: Vec<u8>,
        caption: &str,
        silent: bool,
        reply_to: Option<i64>,
    ) -> Outcome {
        classify(
            self.app
                .telegram
                .send_document(self.chat(chat), name, data, caption, silent, reply_to)
                .await,
        )
    }

    /// One event as a message, or a summary with a Markdown file when too long.
    async fn send_single(&self, d: &Decoded, silent: bool) -> Result<Outcome> {
        let reply_to = self.reply_target(&d.row).await?;
        let v = render::view(d.source, &d.body);
        let html = v.html();
        if render::visible_len(&html) <= MAX_MESSAGE {
            return Ok(self.send_text(d.row.chat_id, &html, silent, reply_to).await);
        }
        let md = format!(
            "# {}\n\n{}",
            crate::text::md_inline(crate::event::type_title(&d.body)),
            render::md_entry(&d.body, d.row.occurred_at)
        );
        let caption = v.summary_html(render::MAX_CAPTION);
        let texts = render::split_parts(&md, super::api::MAX_DOCUMENT);
        let n = texts.len();
        let parts: Vec<Part> = texts
            .into_iter()
            .enumerate()
            .map(|(i, text)| Part {
                name: render::file_name("event", d.row.occurred_at, Some((i + 1, n))),
                caption: if i == 0 {
                    caption.clone()
                } else {
                    crate::trf!("Часть {} из {n}", "Part {} of {n}", i + 1)
                },
                text,
            })
            .collect();
        Ok(self
            .send_parts(d.row.chat_id, parts, silent, reply_to)
            .await)
    }

    /// Sends files in order, later ones as replies to the first. After a failure past the
    /// first file the unsent ones are handed back to be queued, so nothing goes out twice.
    async fn send_parts(
        &self,
        chat: Option<i64>,
        parts: Vec<Part>,
        silent: bool,
        reply_to: Option<i64>,
    ) -> Outcome {
        let mut first = None;
        for (i, part) in parts.iter().enumerate() {
            let target = if i == 0 { reply_to } else { reply_to.or(first) };
            match self
                .send_file(
                    chat,
                    &part.name,
                    part.text.clone().into_bytes(),
                    &part.caption,
                    silent,
                    target,
                )
                .await
            {
                Outcome::Sent(id) => first = first.or(id),
                other if i == 0 => return other,
                _ => {
                    return Outcome::Partial(
                        first,
                        Rest {
                            parts: parts.get(i..).unwrap_or_default().to_vec(),
                            reply_to: reply_to.or(first),
                            silent,
                        },
                    )
                }
            }
            if i + 1 < parts.len() {
                tokio::time::sleep(SEND_GAP).await;
            }
        }
        Outcome::Sent(first)
    }

    /// The queued rest of a long message.
    async fn send_rest(&self, row: &OutboxRow) -> Result<Outcome> {
        let Ok(rest) = serde_json::from_str::<Rest>(&row.body) else {
            return Ok(Outcome::Drop("cannot decode queued parts".into()));
        };
        Ok(
            match self
                .send_parts(row.chat_id, rest.parts, rest.silent, rest.reply_to)
                .await
            {
                Outcome::Partial(_, left) => {
                    let n = left.parts.len();
                    self.app
                        .db
                        .set_body(row.id, &serde_json::to_string(&left)?)
                        .await?;
                    Outcome::Retry(format!("{n} part(s) of a long message left"))
                }
                other => other,
            },
        )
    }

    async fn send_digest(&self, chat: Option<i64>, list: &[Decoded], silent: bool) -> Outcome {
        let items: Vec<Item<'_>> = list
            .iter()
            .map(|d| Item {
                source: d.source,
                body: &d.body,
                occurred_at: d.row.occurred_at,
            })
            .collect();
        let from = list
            .iter()
            .map(|d| d.row.occurred_at)
            .min()
            .unwrap_or_else(crate::now);
        let to = list
            .iter()
            .map(|d| d.row.occurred_at)
            .max()
            .unwrap_or_else(crate::now);
        let md = render::digest_markdown(&items, from, to);
        let caption = render::digest_summary(&items);
        let texts = render::split_parts(&md, super::api::MAX_DOCUMENT);
        let n = texts.len();
        let now = crate::now();
        let parts: Vec<Part> = texts
            .into_iter()
            .enumerate()
            .map(|(i, text)| Part {
                name: render::file_name("digest", now, Some((i + 1, n))),
                caption: if i == 0 {
                    caption.clone()
                } else {
                    crate::trf!("Дайджест, часть {} из {n}", "Digest, part {} of {n}", i + 1)
                },
                text,
            })
            .collect();
        self.send_parts(chat, parts, silent, None).await
    }

    async fn apply(
        &mut self,
        chat: Option<i64>,
        ids: &[i64],
        attempts: i64,
        outcome: Outcome,
    ) -> Result<bool> {
        match outcome {
            Outcome::Migrated(to) => {
                if let Some(from) = chat {
                    self.app.move_chat(from, to).await?;
                }
                Ok(false)
            }
            Outcome::Blocked(e) if chat.is_some() => {
                tracing::warn!(reason = %e, "group or channel unavailable, removed from delivery");
                if let Some(c) = chat {
                    let title = crate::settings::current()
                        .chats
                        .iter()
                        .find(|x| x.id == c)
                        .map(|x| crate::text::clean_line(&x.title, 100));
                    self.app
                        .update_settings(|s| s.chats.retain(|x| x.id != c))
                        .await?;
                    self.app.db.drop_chat(c).await?;
                    if let Some(title) = title {
                        self.app
                            .db
                            .enqueue(&Event::service(
                                format!("bot:unavailable:{c}:{}", crate::now() / 60),
                                crate::tr!("Чат недоступен", "Chat unavailable"),
                                crate::trf!(
                                    "«{title}»: Telegram не даёт боту туда писать, чат удалён из списка доставки. Добавьте бота заново, если это ошибка.",
                                    "\"{title}\": Telegram does not let the bot post there, the chat was removed from delivery. Add the bot again if this is a mistake."
                                ),
                            ))
                            .await?;
                    }
                }
                Ok(false)
            }
            Outcome::Partial(msg, rest) => {
                let key = format!(
                    "parts:{}:{}",
                    ids.first().copied().unwrap_or(0),
                    crate::now()
                );
                self.app
                    .db
                    .mark_sent_with_rest(ids, msg, &key, chat, &serde_json::to_string(&rest)?)
                    .await?;
                Ok(true)
            }
            Outcome::Sent(msg) => {
                self.app.db.mark_sent(ids, msg).await?;
                if self.blocked_logged {
                    tracing::info!("Telegram delivery restored");
                    self.blocked_logged = false;
                }
                self.app.health.ok("telegram").await;
                Ok(true)
            }
            Outcome::Wait(secs) => {
                tracing::warn!(seconds = secs, "Telegram rate limit");
                tokio::time::sleep(Duration::from_secs(secs.min(3600))).await;
                Ok(false)
            }
            Outcome::Retry(e) => {
                let delay = 5i64.saturating_mul(1 << attempts.clamp(0, 7)).min(600);
                self.app.db.mark_retry(ids, crate::now() + delay).await?;
                self.app.health.record_error("telegram", &e);
                Ok(false)
            }
            Outcome::Blocked(e) => {
                if !self.blocked_logged {
                    tracing::warn!(reason = %e, "admin chat unavailable (bot blocked or not started); events stay queued up to 30 days");
                    self.blocked_logged = true;
                }
                self.app.db.mark_retry(ids, crate::now() + 600).await?;
                Ok(false)
            }
            Outcome::Drop(e) => {
                if attempts + 1 >= MAX_ATTEMPTS_BAD_REQUEST {
                    tracing::error!(error = %e, ids = ?ids, "Telegram rejected the message, dropped");
                    self.app.db.mark_dropped(ids).await?;
                } else {
                    self.app.db.mark_retry(ids, crate::now() + 30).await?;
                }
                Ok(false)
            }
        }
    }

    async fn report_expired(&self) -> Result<()> {
        let expired = self.app.db.drop_expired(crate::now() - MAX_AGE).await?;
        if expired > 0 {
            let prev: u64 = self
                .app
                .db
                .meta_get("expired_unreported")
                .await?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            self.app
                .db
                .meta_set("expired_unreported", &(prev + expired).to_string())
                .await?;
            tracing::warn!(count = expired, "queued events older than 30 days dropped");
        }
        Ok(())
    }

    async fn queue_expired_notice(&self) -> Result<()> {
        if let Some(n) = self.app.db.meta_get("expired_unreported").await? {
            self.app.db.meta_del("expired_unreported").await?;
            let ev = Event::service(
                format!("bot:expired:{}", crate::now()),
                crate::tr!("Часть уведомлений не доставлена", "Some notifications were not delivered"),
                crate::trf!(
                    "{n} событий ждали отправки дольше 30 дней (бот был заблокирован или чат не начат) и удалены.",
                    "{n} events waited longer than 30 days (the bot was blocked or the chat not started) and were removed."
                ),
            );
            self.app.db.enqueue(&ev).await?;
        }
        Ok(())
    }

    /// One pass over the queue. Returns true if something was sent.
    async fn tick(&mut self) -> Result<bool> {
        self.report_expired().await?;
        let now = crate::now();
        let rows = self.app.db.due(now).await?;
        if rows.is_empty() {
            return Ok(false);
        }
        let silent_hours = Self::quiet_now();
        let mut sent_any = false;
        let mut events: std::collections::BTreeMap<Option<i64>, Vec<OutboxRow>> =
            std::collections::BTreeMap::new();
        for row in rows {
            let attempts = row.attempts;
            if row.kind == KIND_EVENT {
                events.entry(row.chat_id).or_default().push(row);
                continue;
            }
            let (id, chat) = (row.id, row.chat_id);
            let outcome = if row.kind == KIND_REPLY {
                self.send_text(chat, &row.body, false, None).await
            } else if row.kind == KIND_PARTS {
                self.send_rest(&row).await?
            } else {
                match decode(row) {
                    Some(d) => self.send_single(&d, silent_hours).await?,
                    None => Outcome::Drop("cannot decode queued event".into()),
                }
            };
            let ok = self.apply(chat, &[id], attempts, outcome).await?;
            sent_any |= ok;
            if !ok {
                return Ok(sent_any);
            }
            tokio::time::sleep(SEND_GAP).await;
        }
        if sent_any {
            self.queue_expired_notice().await?;
        }
        if events.is_empty() || pause_state(&self.app).await?.is_some() {
            return Ok(sent_any);
        }
        for (chat, rows) in events {
            sent_any |= self.send_events(chat, rows, now, silent_hours).await?;
        }
        Ok(sent_any)
    }

    /// Sends the due events of one chat, as single messages or one digest.
    async fn send_events(
        &mut self,
        chat: Option<i64>,
        rows: Vec<OutboxRow>,
        now: i64,
        silent_hours: bool,
    ) -> Result<bool> {
        let mut sent_any = false;
        let mut ready = Vec::new();
        for row in rows {
            if let Some(partner) = row.reply_to {
                if self
                    .app
                    .db
                    .outbox_state(partner)
                    .await?
                    .is_some_and(|(status, _)| status == STATUS_PENDING)
                {
                    continue;
                }
            }
            match decode(row.clone()) {
                Some(d) => ready.push(d),
                None => {
                    self.app.db.mark_dropped(&[row.id]).await?;
                }
            }
        }
        let Some(oldest) = ready.iter().map(|d| d.row.created_at).min() else {
            return Ok(false);
        };
        let cfg = self.app.cfg();
        let window = i64::try_from(cfg.digest.window_seconds).unwrap_or(20);
        if now - oldest < window {
            return Ok(false);
        }
        if ready.len() > cfg.digest.threshold {
            let ids: Vec<i64> = ready.iter().map(|d| d.row.id).collect();
            let attempts = ready.iter().map(|d| d.row.attempts).max().unwrap_or(0);
            let outcome = self.send_digest(chat, &ready, silent_hours).await;
            return self.apply(chat, &ids, attempts, outcome).await;
        }
        for d in &ready {
            let outcome = self.send_single(d, silent_hours).await?;
            let ok = self
                .apply(chat, &[d.row.id], d.row.attempts, outcome)
                .await?;
            sent_any |= ok;
            if !ok {
                break;
            }
            tokio::time::sleep(SEND_GAP).await;
        }
        Ok(sent_any)
    }

    pub async fn run(mut self) -> Result<()> {
        loop {
            self.app.health.beat("sender", 300);
            if let Err(e) = self.tick().await {
                tracing::error!(error = %e, "sender pass failed");
            }
            tokio::select! {
                () = self.app.cancel.cancelled() => return Ok(()),
                () = self.app.outbox_signal.notified() => {}
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }
}

fn classify(r: Result<i64, TgError>) -> Outcome {
    match r {
        Ok(id) => Outcome::Sent(Some(id)),
        Err(TgError::RetryAfter(s)) => Outcome::Wait(s),
        Err(TgError::Blocked(e)) => Outcome::Blocked(e),
        Err(TgError::Migrated(id)) => Outcome::Migrated(id),
        Err(TgError::BadRequest(e)) => Outcome::Drop(e),
        Err(e @ (TgError::Network(_) | TgError::Unauthorized)) => Outcome::Retry(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(start: (i8, i8), end: (i8, i8)) -> QuietHours {
        QuietHours {
            enabled: true,
            start: Time::new(start.0, start.1, 0, 0).unwrap(),
            end: Time::new(end.0, end.1, 0, 0).unwrap(),
        }
    }

    fn at(tz: &jiff::tz::TimeZone, s: &str) -> i64 {
        let dt: jiff::civil::DateTime = s.parse().unwrap();
        dt.to_zoned_datetime(tz.clone())
            .unwrap()
            .timestamp()
            .as_second()
    }

    #[test]
    fn quiet_hours_cross_midnight() {
        let tz = jiff::tz::TimeZone::get("Europe/Moscow").unwrap();
        let qh = q((22, 0), (8, 0));
        assert!(is_quiet(&qh, at(&tz, "2026-11-24T23:30"), &tz));
        assert!(is_quiet(&qh, at(&tz, "2026-11-25T07:59"), &tz));
        assert!(!is_quiet(&qh, at(&tz, "2026-11-25T08:01"), &tz));
        assert!(!is_quiet(&qh, at(&tz, "2026-11-25T21:59"), &tz));
    }

    #[test]
    fn quiet_hours_with_dst() {
        let tz = jiff::tz::TimeZone::get("Europe/Riga").unwrap();
        let qh = q((22, 0), (8, 0));
        assert!(is_quiet(&qh, at(&tz, "2026-03-29T03:30"), &tz));
        assert!(!is_quiet(&qh, at(&tz, "2026-10-25T12:00"), &tz));
        assert!(is_quiet(&qh, at(&tz, "2026-10-25T03:30"), &tz));
    }

    #[test]
    fn quiet_hours_disabled() {
        let tz = jiff::tz::TimeZone::UTC;
        let mut qh = q((22, 0), (8, 0));
        qh.enabled = false;
        assert!(!is_quiet(&qh, 0, &tz));
    }
}
