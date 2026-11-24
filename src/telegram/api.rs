// SPDX-License-Identifier: GPL-3.0-or-later
//! Thin Telegram Bot API client. The token lives only inside the request URL.

use std::time::Duration;

use reqwest::multipart::{Form, Part};
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

use crate::net::HttpClient;

#[derive(Debug, thiserror::Error)]
pub enum TgError {
    #[error("Telegram asks to wait {0} s")]
    RetryAfter(u64),
    #[error("chat unavailable: {0}")]
    Blocked(String),
    #[error("request rejected: {0}")]
    BadRequest(String),
    #[error("token rejected")]
    Unauthorized,
    #[error("group became a supergroup {0}")]
    Migrated(i64),
    #[error("{0}")]
    Network(String),
}

#[derive(Deserialize)]
struct Reply<T> {
    ok: bool,
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    parameters: Option<ReplyParams>,
}

#[derive(Deserialize)]
struct ReplyParams {
    retry_after: Option<u64>,
    migrate_to_chat_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub date: i64,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub migrate_to_chat_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    #[serde(default)]
    pub message: Option<CallbackMessage>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackMessage {
    pub message_id: i64,
    pub chat: Chat,
}

/// Inline keyboard: rows of (label, callback data).
pub type Keyboard = Vec<Vec<(String, String)>>;

fn markup(kb: &Keyboard) -> serde_json::Value {
    let rows: Vec<Vec<serde_json::Value>> = kb
        .iter()
        .map(|row| {
            row.iter()
                .map(|(label, data)| json!({"text": label, "callback_data": data}))
                .collect()
        })
        .collect();
    json!({ "inline_keyboard": rows })
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemberStatus {
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMemberUpdated {
    pub chat: Chat,
    pub from: User,
    pub new_chat_member: MemberStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub my_chat_member: Option<ChatMemberUpdated>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Deserialize)]
struct Sent {
    message_id: i64,
}

pub struct TelegramApi {
    http: HttpClient,
    token: SecretString,
}

pub const MAX_DOCUMENT: usize = 49 * 1024 * 1024;

impl TelegramApi {
    pub fn new(http: HttpClient, token: SecretString) -> Self {
        Self { http, token }
    }

    fn url(&self, method: &str) -> String {
        format!(
            "https://api.telegram.org/bot{}/{method}",
            self.token.expose_secret()
        )
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        build: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<T, TgError> {
        let req = self
            .http
            .request(Method::POST, &self.url(method))
            .map_err(|e| TgError::Network(e.to_string()))?
            .timeout(timeout);
        let (status, _, body) = self
            .http
            .send_full(build(req), 16 * 1024 * 1024)
            .await
            .map_err(|e| TgError::Network(e.to_string()))?;
        let reply: Reply<T> = serde_json::from_slice(&body)
            .map_err(|_| TgError::Network(format!("bad reply, HTTP {status}")))?;
        if reply.ok {
            return reply
                .result
                .ok_or_else(|| TgError::Network("empty result".into()));
        }
        let desc =
            crate::text::clean_line(reply.description.as_deref().unwrap_or("unknown error"), 200);
        if let Some(to) = reply.parameters.as_ref().and_then(|p| p.migrate_to_chat_id) {
            return Err(TgError::Migrated(to));
        }
        Err(
            match reply.error_code.unwrap_or(i64::from(status.as_u16())) {
                429 => {
                    TgError::RetryAfter(reply.parameters.and_then(|p| p.retry_after).unwrap_or(5))
                }
                401 | 404 => TgError::Unauthorized,
                403 => TgError::Blocked(desc),
                400 if desc.contains("chat not found") => TgError::Blocked(desc),
                400 => TgError::BadRequest(desc),
                _ => TgError::Network(desc),
            },
        )
    }

    pub async fn get_me(&self) -> Result<User, TgError> {
        self.call("getMe", |r| r, Duration::from_secs(20)).await
    }

    pub async fn get_updates(&self, offset: i64, wait: u64) -> Result<Vec<Update>, TgError> {
        let body = json!({
            "offset": offset,
            "timeout": wait,
            "limit": 100,
            "allowed_updates": ["message", "my_chat_member", "callback_query"],
        });
        self.call(
            "getUpdates",
            |r| r.json(&body),
            Duration::from_secs(wait + 15),
        )
        .await
    }

    pub async fn send_message(
        &self,
        chat: i64,
        html: &str,
        silent: bool,
        reply_to: Option<i64>,
    ) -> Result<i64, TgError> {
        let mut body = json!({
            "chat_id": chat,
            "text": html,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
            "disable_notification": silent,
        });
        if let (Some(id), Some(obj)) = (reply_to, body.as_object_mut()) {
            obj.insert(
                "reply_parameters".into(),
                json!({"message_id": id, "allow_sending_without_reply": true}),
            );
        }
        let sent: Sent = self
            .call("sendMessage", |r| r.json(&body), Duration::from_secs(30))
            .await?;
        Ok(sent.message_id)
    }

    /// Plain text without markup, used for replies to strangers.
    pub async fn send_plain(&self, chat: i64, text: &str) -> Result<i64, TgError> {
        let body =
            json!({"chat_id": chat, "text": text, "link_preview_options": {"is_disabled": true}});
        let sent: Sent = self
            .call("sendMessage", |r| r.json(&body), Duration::from_secs(30))
            .await?;
        Ok(sent.message_id)
    }

    pub async fn send_document(
        &self,
        chat: i64,
        file_name: &str,
        data: Vec<u8>,
        caption_html: &str,
        silent: bool,
        reply_to: Option<i64>,
    ) -> Result<i64, TgError> {
        let part = Part::bytes(data)
            .file_name(file_name.to_owned())
            .mime_str("text/markdown")
            .map_err(|_| TgError::BadRequest("bad mime".into()))?;
        let mut form = Form::new()
            .text("chat_id", chat.to_string())
            .text("caption", caption_html.to_owned())
            .text("parse_mode", "HTML")
            .text("disable_notification", silent.to_string())
            .part("document", part);
        if let Some(id) = reply_to {
            form = form.text(
                "reply_parameters",
                json!({"message_id": id, "allow_sending_without_reply": true}).to_string(),
            );
        }
        let sent: Sent = self
            .call(
                "sendDocument",
                |r| r.multipart(form),
                Duration::from_secs(120),
            )
            .await?;
        Ok(sent.message_id)
    }

    /// Message with an inline keyboard, sent at once (not through the queue).
    pub async fn send_menu(&self, chat: i64, html: &str, kb: &Keyboard) -> Result<i64, TgError> {
        let body = json!({
            "chat_id": chat,
            "text": html,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
            "reply_markup": markup(kb),
        });
        let sent: Sent = self
            .call("sendMessage", |r| r.json(&body), Duration::from_secs(30))
            .await?;
        Ok(sent.message_id)
    }

    /// Replaces the text and keyboard of a menu message.
    pub async fn edit_menu(
        &self,
        chat: i64,
        message_id: i64,
        html: &str,
        kb: &Keyboard,
    ) -> Result<(), TgError> {
        let body = json!({
            "chat_id": chat,
            "message_id": message_id,
            "text": html,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
            "reply_markup": markup(kb),
        });
        match self
            .call::<serde_json::Value>(
                "editMessageText",
                |r| r.json(&body),
                Duration::from_secs(30),
            )
            .await
        {
            Err(TgError::BadRequest(e)) if e.contains("message is not modified") => Ok(()),
            other => other.map(|_| ()),
        }
    }

    pub async fn answer_callback(&self, id: &str, text: Option<&str>) -> Result<(), TgError> {
        let mut body = json!({ "callback_query_id": id });
        if let (Some(t), Some(obj)) = (text, body.as_object_mut()) {
            obj.insert("text".into(), json!(t));
        }
        let _: bool = self
            .call(
                "answerCallbackQuery",
                |r| r.json(&body),
                Duration::from_secs(20),
            )
            .await?;
        Ok(())
    }

    pub async fn leave_chat(&self, chat: i64) -> Result<bool, TgError> {
        let body = json!({"chat_id": chat});
        self.call("leaveChat", |r| r.json(&body), Duration::from_secs(20))
            .await
    }

    /// Commands visible only in the admin chat; the default list stays empty.
    pub async fn register_commands(
        &self,
        admin: i64,
        commands: &[(&str, &str)],
    ) -> Result<(), TgError> {
        let list: Vec<_> = commands
            .iter()
            .map(|(c, d)| json!({"command": c, "description": d}))
            .collect();
        let _: bool = self
            .call(
                "deleteMyCommands",
                |r| r.json(&json!({"scope": {"type": "default"}})),
                Duration::from_secs(20),
            )
            .await?;
        let body = json!({"commands": list, "scope": {"type": "chat", "chat_id": admin}});
        let _: bool = self
            .call("setMyCommands", |r| r.json(&body), Duration::from_secs(20))
            .await?;
        Ok(())
    }
}
