// SPDX-License-Identifier: GPL-3.0-or-later
//! Long polling, admin commands, groups and strangers.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;

use super::api::{CallbackQuery, Message, TgError, Update};
use super::menu::{self, Action, Avail, Prompt, View};
use crate::app::App;
use crate::event::Event;
use crate::text::{escape_html, external_html};

const STALE_AFTER: i64 = 300;
const PROMPT_TTL: i64 = 600;

/// Command menu in the current language.
pub fn commands() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "settings",
            crate::tr!("настройки с кнопками", "settings with buttons"),
        ),
        (
            "status",
            crate::tr!("состояние бота и источников", "bot and source state"),
        ),
        (
            "sources",
            crate::tr!("что отслеживается", "what is watched"),
        ),
        (
            "pause",
            crate::tr!(
                "пауза уведомлений, например /pause 2h",
                "pause notifications, e.g. /pause 2h"
            ),
        ),
        ("resume", crate::tr!("снять паузу", "resume notifications")),
        (
            "check_now",
            crate::tr!(
                "проверить сейчас: github, guides или games",
                "check now: github, guides or games"
            ),
        ),
        (
            "reload",
            crate::tr!("перечитать конфиг", "re-read the config file"),
        ),
        (
            "strangers",
            crate::tr!("посторонние за 7 дней", "strangers in the last 7 days"),
        ),
        ("help", crate::tr!("список команд", "list of commands")),
    ]
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Settings,
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
        return Err(ParseError::BadArgs(crate::tr!(
            "слишком много аргументов",
            "too many arguments"
        )));
    }
    let arg = args.first().copied();
    let no_args = |c: Command| {
        if arg.is_some() {
            Err(ParseError::BadArgs(crate::tr!(
                "команда без аргументов",
                "the command takes no arguments"
            )))
        } else {
            Ok(c)
        }
    };
    match name.as_str() {
        "start" | "help" => no_args(Command::Help),
        "settings" => no_args(Command::Settings),
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
                    .ok_or(ParseError::BadArgs(crate::tr!(
                        "длительность вида 30m, 2h, 1d, 1w или 1h30m, не больше года",
                        "a duration like 30m, 2h, 1d, 1w or 1h30m, at most a year"
                    )))
            }
        },
        "check_now" => match arg.map(str::to_ascii_lowercase).as_deref() {
            None => Ok(Command::CheckNow(None)),
            Some("github") => Ok(Command::CheckNow(Some(Target::Github))),
            Some("guides") => Ok(Command::CheckNow(Some(Target::Guides))),
            Some("games") => Ok(Command::CheckNow(Some(Target::Games))),
            Some(_) => Err(ParseError::BadArgs(crate::tr!(
                "источник: github, guides или games",
                "source: github, guides or games"
            ))),
        },
        _ => Err(ParseError::Unknown),
    }
}

fn help_text() -> String {
    let mut s = String::from(crate::tr!("<b>Команды</b>", "<b>Commands</b>"));
    for (c, d) in commands() {
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
    awaiting: Option<(Prompt, i64)>,
}

impl Poller {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            last_check: std::collections::HashMap::new(),
            awaiting: None,
        }
    }

    fn admin(&self) -> i64 {
        self.app.secrets.telegram_admin_id
    }

    fn avail(&self) -> Avail {
        Avail {
            github: self.app.github.is_some(),
            guides: self.app.secrets.steam_profile_id.is_some(),
        }
    }

    async fn repo_names(&self) -> Result<Vec<String>> {
        Ok(self
            .app
            .db
            .repos()
            .await?
            .into_iter()
            .map(|r| r.full_name)
            .collect())
    }

    async fn screen(&self, v: View) -> Result<(String, super::api::Keyboard)> {
        let repos = if matches!(v, View::Repos(_)) {
            self.repo_names().await?
        } else {
            Vec::new()
        };
        let s = crate::settings::current();
        Ok(menu::render(v, &s, self.avail(), &repos))
    }

    async fn show_menu(&self, v: View) -> Result<()> {
        let (html, kb) = self.screen(v).await?;
        match self.app.telegram.send_menu(self.admin(), &html, &kb).await {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "menu not sent");
                Ok(())
            }
        }
    }

    async fn edit_menu(&self, message_id: i64, v: View) -> Result<()> {
        let (html, kb) = self.screen(v).await?;
        if let Err(e) = self
            .app
            .telegram
            .edit_menu(self.admin(), message_id, &html, &kb)
            .await
        {
            tracing::warn!(error = %e, "menu not updated");
        }
        Ok(())
    }

    async fn on_callback(&mut self, q: CallbackQuery) -> Result<()> {
        let admin = self.admin();
        let in_admin_chat = q.message.as_ref().is_some_and(|m| m.chat.id == admin);
        if q.from.id != admin || !in_admin_chat {
            let _ = self.app.telegram.answer_callback(&q.id, None).await;
            return Ok(());
        }
        let Some(msg) = q.message else { return Ok(()) };
        let action = q.data.as_deref().and_then(menu::parse);
        let mut note = None;
        let next = match action {
            None => Some(View::Main),
            Some(Action::Show(v)) => Some(v),
            Some(Action::Toggle(key)) => {
                let mut target = None;
                self.app
                    .update_settings(|s| target = menu::toggle(s, &key))
                    .await?;
                Some(target.unwrap_or(View::Main))
            }
            Some(Action::Repo(page, idx)) => {
                let repos = self.repo_names().await?;
                self.app
                    .update_settings(|s| {
                        menu::toggle_repo(s, &repos, page, idx);
                    })
                    .await?;
                Some(View::Repos(page))
            }
            Some(Action::Ask(p)) => {
                self.awaiting = Some((p, crate::now() + PROMPT_TTL));
                let _ = self
                    .app
                    .telegram
                    .send_plain(admin, menu::prompt_text(p))
                    .await;
                None
            }
            Some(Action::RemoveGame(id)) => {
                self.app
                    .update_settings(|s| s.games.list.retain(|g| g.appid != id))
                    .await?;
                note = Some(crate::tr!("Игра убрана", "Game removed"));
                Some(View::Games)
            }
            Some(Action::PickGame(id)) => {
                note = Some(self.add_game(id, None).await?);
                Some(View::Games)
            }
            Some(Action::Leave(id)) => {
                match self.app.telegram.leave_chat(id).await {
                    Ok(_) | Err(TgError::Blocked(_)) => {}
                    Err(e) => tracing::warn!(error = %e, "leaveChat failed"),
                }
                self.forget_chat(id).await?;
                Some(View::Chats)
            }
            Some(Action::Close) => {
                let _ = self
                    .app
                    .telegram
                    .edit_menu(
                        admin,
                        msg.message_id,
                        crate::tr!("Настройки закрыты.", "Settings closed."),
                        &Vec::new(),
                    )
                    .await;
                None
            }
        };
        let _ = self.app.telegram.answer_callback(&q.id, note).await;
        if let Some(v) = next {
            self.edit_menu(msg.message_id, v).await?;
        }
        Ok(())
    }

    async fn forget_chat(&self, id: i64) -> Result<()> {
        self.app
            .update_settings(|s| s.chats.retain(|c| c.id != id))
            .await?;
        self.app.db.drop_chat(id).await
    }

    /// Adds a game by `AppID`; the name comes from Steam unless given.
    async fn add_game(&self, appid: u32, name: Option<String>) -> Result<&'static str> {
        let s = crate::settings::current();
        if s.game(appid).is_some() {
            return Ok(crate::tr!(
                "Эта игра уже в списке",
                "This game is already listed"
            ));
        }
        if s.games.list.len() >= crate::settings::MAX_GAMES {
            return Ok(crate::tr!("Список игр заполнен", "The game list is full"));
        }
        let name = match name {
            Some(n) => n,
            None => crate::steam::store::name(&self.app.http, appid)
                .await
                .ok()
                .flatten()
                .unwrap_or_default(),
        };
        self.app
            .update_settings(|s| {
                s.games.list.push(crate::settings::Game::new(appid, &name));
                s.games.enabled = true;
            })
            .await?;
        Ok(crate::tr!(
            "Игра добавлена, первая проверка запущена",
            "Game added, the first check has started"
        ))
    }

    /// Text sent after a button asked for input.
    async fn on_input(&mut self, prompt: Prompt, text: &str) -> Result<String> {
        match prompt {
            Prompt::AddGame => {
                if let Some(id) = crate::steam::store::parse_appid(text) {
                    let note = self.add_game(id, None).await?;
                    self.show_menu(View::Games).await?;
                    return Ok(note.to_owned());
                }
                let found = match crate::steam::store::search(&self.app.http, text).await {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(error = %e, "store search failed");
                        return Ok(crate::tr!(
                            "Поиск в Steam не ответил. Попробуйте ещё раз или пришлите AppID.",
                            "Steam search did not answer. Try again or send the AppID."
                        )
                        .to_owned());
                    }
                };
                if found.is_empty() {
                    self.awaiting = Some((Prompt::AddGame, crate::now() + PROMPT_TTL));
                    return Ok(crate::tr!(
                        "Ничего не найдено. Пришлите другое название, ссылку или AppID.",
                        "Nothing found. Send another name, a link or the AppID."
                    )
                    .to_owned());
                }
                let kb: super::api::Keyboard = found
                    .iter()
                    .map(|f| {
                        let label: String = format!("{} ({})", f.name, f.appid)
                            .chars()
                            .take(60)
                            .collect();
                        vec![(label, format!("pick:{}", f.appid))]
                    })
                    .chain(std::iter::once(vec![(
                        crate::tr!("Отмена", "Cancel").to_owned(),
                        "m:gm".to_owned(),
                    )]))
                    .collect();
                let _ = self
                    .app
                    .telegram
                    .send_menu(
                        self.admin(),
                        crate::tr!("Выберите игру:", "Choose the game:"),
                        &kb,
                    )
                    .await;
                Ok(String::new())
            }
            Prompt::QuietHours => {
                if let Some((a, z)) = menu::parse_window(text) {
                    self.app
                        .update_settings(|s| {
                            s.quiet_hours.start = a;
                            s.quiet_hours.end = z;
                            s.quiet_hours.enabled = true;
                        })
                        .await?;
                    self.show_menu(View::Quiet).await?;
                    Ok(String::new())
                } else {
                    self.awaiting = Some((Prompt::QuietHours, crate::now() + PROMPT_TTL));
                    Ok(crate::tr!(
                        "Не понял. Нужно окно вида 22:00-08:00.",
                        "Not understood. A window like 22:00-08:00 is needed."
                    )
                    .to_owned())
                }
            }
            Prompt::Timezone => {
                let tz = text.trim();
                if tz.len() <= 64 && jiff::tz::TimeZone::get(tz).is_ok() {
                    let tz = tz.to_owned();
                    self.app.update_settings(|s| s.timezone = tz).await?;
                    self.show_menu(View::Quiet).await?;
                    Ok(String::new())
                } else {
                    self.awaiting = Some((Prompt::Timezone, crate::now() + PROMPT_TTL));
                    Ok(crate::tr!(
                        "Такого часового пояса нет. Пример: Europe/Moscow.",
                        "Unknown time zone. Example: Europe/Berlin."
                    )
                    .to_owned())
                }
            }
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

    /// The bot was put into a group or channel. Only the admin may do that.
    async fn on_joined(&self, chat: &super::api::Chat, by: &super::api::User) -> Result<()> {
        if by.id != self.admin() {
            return self
                .leave_foreign(chat.id, chat.title.as_deref(), &chat.kind, Some(by))
                .await;
        }
        let title = crate::text::clean_line(chat.title.as_deref().unwrap_or("-"), 100);
        let kind = chat.kind.clone();
        let id = chat.id;
        let known = crate::settings::current().chats.iter().any(|c| c.id == id);
        if !known {
            let full = crate::settings::current().chats.len() >= crate::settings::MAX_CHATS;
            if full {
                let _ = self.app.telegram.leave_chat(id).await;
                let ev = Event::service(
                    format!("bot:chats_full:{id}"),
                    crate::tr!("Слишком много чатов", "Too many chats"),
                    crate::tr!(
                        "Бот уже работает в 20 группах и каналах и вышел из нового. Уберите лишние в /settings.",
                        "The bot already serves 20 groups and channels and left the new one. Remove some in /settings."
                    ),
                );
                self.app.db.enqueue(&ev).await?;
                return Ok(());
            }
            self.app
                .update_settings(|s| {
                    s.chats.push(crate::settings::Chat {
                        id,
                        title: title.clone(),
                        kind,
                        delivery: crate::settings::Delivery::default(),
                    });
                })
                .await?;
        }
        let ev = Event::service(
            format!("bot:joined:{id}:{}", crate::now() / 60),
            crate::tr!("Бот добавлен в чат", "Bot added to a chat"),
            crate::trf!(
                "«{title}»: сюда будут приходить уведомления. Что именно присылать, выберите в /settings, раздел «Куда слать».",
                "\"{title}\": notifications will be sent there. Choose which ones in /settings, section \"Delivery\"."
            ),
        );
        self.app.db.enqueue(&ev).await?;
        self.app.outbox_signal.notify_one();
        Ok(())
    }

    async fn on_removed(&self, chat: &super::api::Chat) -> Result<()> {
        let known = crate::settings::current()
            .chats
            .iter()
            .any(|c| c.id == chat.id);
        if !known {
            return Ok(());
        }
        self.forget_chat(chat.id).await?;
        let title = crate::text::clean_line(chat.title.as_deref().unwrap_or("-"), 100);
        let ev = Event::service(
            format!("bot:removed:{}:{}", chat.id, crate::now() / 60),
            crate::tr!("Бот убран из чата", "Bot removed from a chat"),
            crate::trf!(
                "«{title}»: бот больше не может туда писать, чат удалён из списка доставки.",
                "\"{title}\": the bot can no longer post there, the chat was removed from delivery."
            ),
        );
        self.app.db.enqueue(&ev).await?;
        Ok(())
    }

    async fn leave_foreign(
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
            let name = u.username.as_deref().map_or_else(
                || crate::tr!("без имени пользователя", "no username").to_owned(),
                |n| format!("@{}", crate::text::clean_line(n, 64)),
            );
            let id = u.id;
            crate::trf!(
                " Добавил: {name} (ID {id}).",
                " Added by: {name} (ID {id})."
            )
        });
        let place = match kind {
            "channel" => crate::tr!("канал", "channel"),
            _ => crate::tr!("группу", "group"),
        };
        let title = crate::text::clean_line(title.unwrap_or("-"), 100);
        let ev = Event::service(
            format!("bot:left:{chat_id}:{}", crate::now() / 60),
            crate::tr!("Бот вышел из чата", "Bot left a chat"),
            crate::trf!(
                "Бота добавили в {place} «{title}», он сразу вышел: добавлять его может только владелец.{who}",
                "The bot was added to the {place} \"{title}\" and left at once: only the owner may add it.{who}"
            ),
        );
        self.app.db.enqueue(&ev).await?;
        Ok(())
    }

    async fn handle(&mut self, u: Update) -> Result<()> {
        if let Some(q) = u.callback_query {
            return self.on_callback(q).await;
        }
        if let Some(m) = &u.my_chat_member {
            if m.chat.kind != "private" {
                match m.new_chat_member.status.as_str() {
                    "member" | "administrator" | "restricted" => {
                        self.on_joined(&m.chat, &m.from).await?;
                    }
                    "left" | "kicked" => self.on_removed(&m.chat).await?,
                    _ => {}
                }
            }
            return Ok(());
        }
        let Some(msg) = u.message else { return Ok(()) };
        if msg.chat.kind != "private" {
            if let Some(to) = msg.migrate_to_chat_id {
                return self.app.move_chat(msg.chat.id, to).await;
            }
            let known = crate::settings::current()
                .chats
                .iter()
                .any(|c| c.id == msg.chat.id);
            if !known {
                self.leave_foreign(
                    msg.chat.id,
                    msg.chat.title.as_deref(),
                    &msg.chat.kind,
                    msg.from.as_ref(),
                )
                .await?;
            }
            return Ok(());
        }
        let Some(from) = &msg.from else { return Ok(()) };
        let stale = crate::now() - msg.date > STALE_AFTER;
        if from.id != self.admin() || msg.chat.id != from.id {
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
                self.reply(
                    u.update_id,
                    crate::tr!(
                        "Команда устарела (отправлена больше 5 минут назад) и не выполнена. Повторите её.",
                        "The command is stale (sent more than 5 minutes ago) and was not run. Send it again."
                    ),
                )
                .await?;
            }
            return Ok(());
        }
        self.admin_message(u.update_id, &msg).await
    }

    async fn admin_message(&mut self, update_id: i64, msg: &Message) -> Result<()> {
        let text = msg.text.as_deref().unwrap_or_default();
        let cmd = match parse_command(text) {
            Ok(c) => {
                self.awaiting = None;
                c
            }
            Err(ParseError::BadArgs(why)) => {
                let why = escape_html(why);
                let help = help_text();
                return self
                    .reply(
                        update_id,
                        &crate::trf!(
                            "Неверные аргументы: {why}.\n\n{help}",
                            "Wrong arguments: {why}.\n\n{help}"
                        ),
                    )
                    .await;
            }
            Err(ParseError::NotACommand) => {
                if let Some((p, until)) = self.awaiting.take() {
                    if until >= crate::now() && !text.trim().is_empty() {
                        let answer = self.on_input(p, text).await?;
                        if answer.is_empty() {
                            return Ok(());
                        }
                        return self.reply(update_id, &escape_html(&answer)).await;
                    }
                }
                return self.reply(update_id, &help_text()).await;
            }
            Err(ParseError::Unknown) => return self.reply(update_id, &help_text()).await,
        };
        let answer = match cmd {
            Command::Help => help_text(),
            Command::Settings => {
                self.show_menu(View::Main).await?;
                return Ok(());
            }
            Command::Status => self.status().await?,
            Command::Sources => self.sources().await?,
            Command::Pause(d) => {
                let value =
                    d.map_or_else(|| "forever".to_owned(), |d| (crate::now() + d).to_string());
                self.app.db.meta_set("paused_until", &value).await?;
                match d {
                    Some(d) => {
                        let (until, len) =
                            (crate::fmt_time(crate::now() + d), crate::fmt_duration(d));
                        crate::trf!(
                            "Пауза до {until} ({len}). События собираются и придут после паузы.",
                            "Paused until {until} ({len}). Events are collected and arrive after the pause."
                        )
                    }
                    None => crate::tr!(
                        "Пауза без срока. События собираются, снять паузу: /resume.",
                        "Paused without a time limit. Events are collected, resume with /resume."
                    )
                    .to_owned(),
                }
            }
            Command::Resume => {
                let was = super::sender::pause_state(&self.app).await?.is_some();
                self.app.db.meta_del("paused_until").await?;
                let pending = self.app.db.pending_count().await?;
                if was {
                    crate::trf!(
                        "Пауза снята. В очереди: {pending}.",
                        "Resumed. Queued: {pending}."
                    )
                } else {
                    crate::tr!("Паузы не было.", "There was no pause.").to_owned()
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
                Target::Guides => (
                    "guides",
                    crate::tr!("руководства", "guides"),
                    &self.app.check_now.guides,
                ),
                Target::Games => (
                    "games",
                    crate::tr!("игры", "games"),
                    &self.app.check_now.games,
                ),
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
            let list = started.join(", ");
            s.push_str(&crate::trf!(
                "Проверка запущена: {list}.",
                "Check started: {list}."
            ));
        }
        if !skipped.is_empty() {
            let list = skipped.join(", ");
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&crate::trf!(
                "Не чаще раза в минуту, пропущено: {list}.",
                "At most once a minute, skipped: {list}."
            ));
        }
        s
    }

    async fn status(&self) -> Result<String> {
        let st = crate::settings::current();
        let now = crate::now();
        let version = env!("CARGO_PKG_VERSION");
        let commit = escape_html(option_env!("HQW_COMMIT").unwrap_or("dev"));
        let up = crate::fmt_duration(now - self.app.health.started());
        let mut s = crate::trf!(
            "<b>Статус</b>\nВерсия: {version} ({commit})\nРаботает: {up}",
            "<b>Status</b>\nVersion: {version} ({commit})\nUptime: {up}"
        );
        let pause = match super::sender::pause_state(&self.app).await? {
            None => crate::tr!("нет", "no").to_owned(),
            Some(None) => crate::tr!("без срока", "without a time limit").to_owned(),
            Some(Some(t)) => crate::trf!("до {}", "until {}", crate::fmt_time(t)),
        };
        let quiet = if super::sender::is_quiet(&st.quiet(), now, &st.tz()) {
            crate::tr!("сейчас активны", "active now")
        } else {
            crate::tr!("сейчас нет", "not now")
        };
        let queue = self.app.db.pending_count().await?;
        let mode = match self.app.cfg().github.mode {
            crate::config::GithubMode::Webhook => crate::tr!("вебхук", "webhook"),
            crate::config::GithubMode::Polling => crate::tr!("опрос", "polling"),
        };
        let _ = write!(
            s,
            "{}",
            crate::trf!(
                "\nПауза: {pause}\nОчередь: {queue}\nТихие часы: {quiet}\nGitHub: {mode}",
                "\nPause: {pause}\nQueue: {queue}\nQuiet hours: {quiet}\nGitHub: {mode}"
            )
        );
        s.push_str(crate::tr!("\n\n<b>Источники</b>", "\n\n<b>Sources</b>"));
        let snap = self.app.health.snapshot();
        for name in crate::health::SUBSYSTEMS {
            let Some(state) = snap.get(name) else {
                continue;
            };
            let ok = state.last_success.map_or_else(
                || crate::tr!("ещё не было", "not yet").to_owned(),
                crate::fmt_time,
            );
            let title = escape_html(crate::health::title(name));
            let _ = write!(
                s,
                "{}",
                crate::trf!("\n{title}: успех {ok}", "\n{title}: success {ok}")
            );
            if let Some((t, e)) = &state.last_error {
                let (when, what) = (
                    crate::fmt_time(*t),
                    external_html(&crate::text::truncate_chars(e, 200)),
                );
                let _ = write!(
                    s,
                    "{}",
                    crate::trf!("; ошибка {when}: {what}", "; error {when}: {what}")
                );
            }
            if state.alerted {
                s.push_str(crate::tr!(" [сбой]", " [failing]"));
            }
        }
        Ok(s)
    }

    async fn sources(&self) -> Result<String> {
        let st = crate::settings::current();
        let none = crate::tr!("\nпока нет", "\nnone yet");
        let mut s = String::from(crate::tr!(
            "<b>Репозитории GitHub</b>",
            "<b>GitHub repositories</b>"
        ));
        if self.app.github.is_none() {
            s.push_str(crate::tr!("\nне настроено", "\nnot configured"));
        } else {
            let repos = self.app.db.repos().await?;
            if repos.is_empty() {
                s.push_str(none);
            }
            for r in &repos {
                let excluded = st
                    .github
                    .excluded_repos
                    .iter()
                    .any(|x| x.eq_ignore_ascii_case(&r.full_name));
                let _ = write!(
                    s,
                    "\n<code>{}</code>{}{}",
                    escape_html(&r.full_name),
                    if r.private {
                        crate::tr!(" (приватный)", " (private)")
                    } else {
                        ""
                    },
                    if excluded {
                        crate::tr!(" [исключён]", " [excluded]")
                    } else {
                        ""
                    }
                );
            }
        }
        s.push_str(crate::tr!(
            "\n\n<b>Руководства Steam</b>",
            "\n\n<b>Steam guides</b>"
        ));
        if self.app.secrets.steam_profile_id.is_none() {
            s.push_str(crate::tr!("\nне настроено", "\nnot configured"));
        } else {
            let guides = self.app.db.guides().await?;
            if guides.is_empty() {
                s.push_str(none);
            }
            for g in &guides {
                let (t, game, n) = (
                    external_html(&g.title),
                    external_html(&g.game),
                    g.comment_count.max(0),
                );
                let _ = write!(
                    s,
                    "{}",
                    crate::trf!(
                        "\n{t} ({game}, комментариев: {n})",
                        "\n{t} ({game}, comments: {n})"
                    )
                );
            }
        }
        s.push_str(crate::tr!(
            "\n\n<b>Игры Steam</b>",
            "\n\n<b>Steam games</b>"
        ));
        if st.games.list.is_empty() {
            s.push_str(none);
        }
        let states: std::collections::BTreeMap<i64, String> = self
            .app
            .db
            .games()
            .await?
            .into_iter()
            .map(|(id, _, state)| (id, state))
            .collect();
        for g in &st.games.list {
            let appid = g.appid;
            let st = states
                .get(&i64::from(appid))
                .and_then(|x| serde_json::from_str::<crate::steam::games::GameState>(x).ok());
            let Some(st) = st else {
                let _ = write!(
                    s,
                    "\n<b>{}</b> ({appid}){}",
                    external_html(&g.label()),
                    crate::tr!(", ещё не проверялась", ", not checked yet")
                );
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
                            crate::tr!(" (с паролем)", " (password)")
                        } else {
                            ""
                        }
                    )
                })
                .collect();
            if !others.is_empty() {
                let _ = write!(
                    s,
                    "{}",
                    crate::trf!(
                        "\n  ветки: {}",
                        "\n  branches: {}",
                        external_html(&others.join(", "))
                    )
                );
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
            s.push_str(crate::tr!("\n…список обрезан", "\n…list truncated"));
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
            return Ok(crate::tr!(
                "Посторонние за 7 дней не писали.",
                "No strangers wrote in the last 7 days."
            )
            .to_owned());
        }
        let mut s = String::from(crate::tr!(
            "<b>Посторонние за 7 дней</b>",
            "<b>Strangers in the last 7 days</b>"
        ));
        for r in rows.iter().take(50) {
            let name = r.username.as_deref().map_or_else(
                || crate::tr!("без имени", "no username").to_owned(),
                |n| format!("@{n}"),
            );
            let (id, name, n, last) = (
                r.user_id,
                external_html(&name),
                r.attempts,
                crate::fmt_time(r.last_at),
            );
            let _ = write!(
                s,
                "{}",
                crate::trf!(
                    "\nID {id}, {name}, попыток: {n}, последняя: {last}",
                    "\nID {id}, {name}, attempts: {n}, last: {last}"
                )
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
            let phrases = crate::config::stranger_phrases(&cfg.strangers.phrases);
            let idx = rand::random_range(0..phrases.len().max(1));
            if let Some(phrase) = phrases.get(idx) {
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
