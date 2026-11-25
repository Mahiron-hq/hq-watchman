// SPDX-License-Identifier: GPL-3.0-or-later
//! Online checks and helpers: Telegram bot, GitHub App, Steam profile and key.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use hq_watchman::net::HttpClient;
use hq_watchman::telegram::api::{TelegramApi, TgError};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

/// Set to true to stop a waiting operation.
pub type Cancel = std::sync::Arc<std::sync::atomic::AtomicBool>;

fn secret(s: &str) -> secrecy::SecretString {
    secrecy::SecretString::from(s.to_owned())
}

pub fn http() -> Result<HttpClient> {
    HttpClient::new(&format!("hq-watchman-setup/{}", env!("CARGO_PKG_VERSION")))
}

pub fn cancelled(c: &Cancel) -> bool {
    c.load(std::sync::atomic::Ordering::Relaxed)
}

fn tg_error(e: TgError) -> anyhow::Error {
    match e {
        TgError::Unauthorized => anyhow!("Telegram rejected the token"),
        other => anyhow!("{other}"),
    }
}

/// Returns the bot user name.
pub async fn telegram_bot(token: &str) -> Result<String> {
    if !hq_watchman::config::valid_telegram_token(token) {
        bail!("the token format is wrong");
    }
    let api = TelegramApi::new(http()?, secret(token));
    let me = api.get_me().await.map_err(tg_error)?;
    me.username
        .ok_or_else(|| anyhow!("the bot has no user name"))
}

/// Waits for the one-time code in a private chat, alone or as `/start <code>`; returns the
/// sender's ID and user name. Other private messages get a hint once.
pub async fn telegram_wait_start(
    token: &str,
    code: &str,
    cancel: &Cancel,
) -> Result<(i64, String)> {
    let api = TelegramApi::new(http()?, secret(token));
    let mut offset = 0i64;
    let mut hinted = std::collections::BTreeSet::new();
    while !cancelled(cancel) {
        let updates = match api.get_updates(offset, 10).await {
            Ok(u) => u,
            Err(TgError::RetryAfter(s)) => {
                tokio::time::sleep(Duration::from_secs(s.min(30))).await;
                continue;
            }
            Err(TgError::BadRequest(e) | TgError::Blocked(e)) => {
                bail!("Telegram: {e}. If the bot is already running somewhere, stop it first.")
            }
            Err(e) => return Err(tg_error(e)),
        };
        for u in updates {
            offset = offset.max(u.update_id + 1);
            let Some(m) = u.message else { continue };
            let Some(from) = m.from else { continue };
            if m.chat.kind != "private" || from.is_bot {
                continue;
            }
            if m.text.as_deref().is_some_and(|t| has_code(t, code)) {
                let _ = api.get_updates(offset, 0).await;
                let name = from
                    .username
                    .map_or_else(|| format!("ID {}", from.id), |n| format!("@{n}"));
                return Ok((from.id, name));
            }
            if hinted.insert(m.chat.id) {
                let _ = api
                    .send_plain(
                        m.chat.id,
                        hq_watchman::tr!(
                            "HQ Watchman Setup: чтобы подтвердить, что вы владелец, отправьте сюда код из окна программы установки.",
                            "HQ Watchman Setup: to confirm you are the owner, send the code shown in the setup window here."
                        ),
                    )
                    .await;
            }
        }
    }
    bail!("cancelled")
}

/// The message is the code itself or `/start <code>`.
fn has_code(text: &str, code: &str) -> bool {
    let mut words = text.split_whitespace();
    let first = words.next().unwrap_or_default();
    if first == code {
        return words.next().is_none();
    }
    (first == "/start" || first.starts_with("/start@"))
        && words.next() == Some(code)
        && words.next().is_none()
}

pub async fn telegram_send(token: &str, chat: i64, text: &str) -> Result<()> {
    let api = TelegramApi::new(http()?, secret(token));
    api.send_plain(chat, text).await.map_err(tg_error)?;
    Ok(())
}

#[derive(Deserialize)]
struct GhUser {
    id: u64,
    login: String,
    #[serde(rename = "type")]
    kind: String,
}

/// GitHub account ID by login; returns (id, login, `is_organization`).
pub async fn github_user(login: &str) -> Result<(u64, String, bool)> {
    let login = login.trim().trim_start_matches('@');
    if login.is_empty()
        || login.len() > 39
        || !login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        bail!("not a GitHub user name");
    }
    let u: GhUser = http()?
        .get_json(&format!("https://api.github.com/users/{login}"))
        .await
        .context("GitHub account not found")?;
    Ok((u.id, u.login, u.kind == "Organization"))
}

/// Result of the App manifest flow.
pub struct CreatedApp {
    pub id: u64,
    pub slug: String,
    pub pem: Zeroizing<String>,
    pub webhook_secret: Zeroizing<String>,
}

#[derive(Deserialize)]
struct Conversion {
    id: u64,
    slug: String,
    pem: String,
    #[serde(default)]
    webhook_secret: Option<String>,
}

fn manifest(login: &str, hook_url: &str, redirect: &str) -> serde_json::Value {
    let name: String = format!("HQ Watchman {login}").chars().take(34).collect();
    serde_json::json!({
        "name": name,
        "url": "https://github.com/Mahiron-hq/hq-watchman",
        "hook_attributes": { "url": hook_url, "active": true },
        "redirect_url": redirect,
        "public": false,
        "default_permissions": {
            "metadata": "read",
            "issues": "read",
            "pull_requests": "read",
            "contents": "read",
            "discussions": "read"
        },
        "default_events": [
            "issues", "issue_comment", "pull_request", "pull_request_review",
            "pull_request_review_comment", "commit_comment", "discussion", "discussion_comment"
        ]
    })
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn page(body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>HQ Watchman Setup</title></head><body style=\"font-family:sans-serif;max-width:640px;margin:60px auto\">{body}</body></html>"
    )
}

async fn respond(stream: &mut tokio::net::TcpStream, html: &str) {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
}

/// Opens the browser on a local page that creates the App on GitHub, then exchanges the code.
pub async fn github_create_app(
    login: &str,
    org: bool,
    hook_url: &str,
    cancel: &Cancel,
    on_open: impl FnOnce(String),
    done_text: &str,
) -> Result<CreatedApp> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let state = crate::plan::random_token(24);
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let target = if org {
        format!("https://github.com/organizations/{login}/settings/apps/new?state={state}")
    } else {
        format!("https://github.com/settings/apps/new?state={state}")
    };
    let form = page(&format!(
        "<form id=\"f\" action=\"{}\" method=\"post\"><input type=\"hidden\" name=\"manifest\" value=\"{}\"><button type=\"submit\" style=\"font-size:18px;padding:10px 20px\">GitHub</button></form><script>document.getElementById('f').submit()</script>",
        html_escape(&target),
        html_escape(&manifest(login, hook_url, &redirect).to_string())
    ));
    let local = format!("http://127.0.0.1:{port}/");
    on_open(local.clone());
    open::that(local).context("cannot open the browser")?;
    let deadline = tokio::time::Instant::now() + Duration::from_mins(15);
    loop {
        if cancelled(cancel) {
            bail!("cancelled");
        }
        let accepted = tokio::time::timeout(Duration::from_millis(500), listener.accept()).await;
        let Ok(Ok((mut stream, _))) = accepted else {
            if tokio::time::Instant::now() > deadline {
                bail!("no answer from GitHub within 15 minutes");
            }
            continue;
        };
        let mut buf = vec![0u8; 8192];
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(0);
        let head = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
        let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
        if !path.starts_with("/callback") {
            respond(&mut stream, &form).await;
            continue;
        }
        let query = path.split_once('?').map_or("", |(_, q)| q);
        let param = |k: &str| {
            query
                .split('&')
                .filter_map(|p| p.split_once('='))
                .find(|(a, _)| *a == k)
                .map(|(_, v)| v.to_owned())
        };
        let code =
            param("code").filter(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_alphanumeric()));
        if param("state").as_deref() != Some(state.as_str()) || code.is_none() {
            respond(&mut stream, &page("<h2>Wrong or missing code</h2>")).await;
            continue;
        }
        let code = code.unwrap_or_default();
        let conv: Result<Conversion> = async {
            let client = http()?;
            let req = client
                .request(
                    reqwest::Method::POST,
                    &format!("https://api.github.com/app-manifests/{code}/conversions"),
                )?
                .header("Accept", "application/vnd.github+json")
                .timeout(Duration::from_secs(30));
            let body = client.send(req, 1024 * 1024).await?;
            Ok(serde_json::from_slice(&body)?)
        }
        .await;
        let conv = match conv {
            Ok(c) => c,
            Err(e) => {
                respond(
                    &mut stream,
                    &page("<h2>GitHub did not confirm the App</h2>"),
                )
                .await;
                return Err(e.context("App code exchange failed"));
            }
        };
        respond(
            &mut stream,
            &page(&format!("<h2>{}</h2>", html_escape(done_text))),
        )
        .await;
        return Ok(CreatedApp {
            id: conv.id,
            slug: conv.slug,
            pem: Zeroizing::new(conv.pem),
            webhook_secret: Zeroizing::new(conv.webhook_secret.unwrap_or_default()),
        });
    }
}

/// Waits until the App is installed on the account; returns the repository selection.
pub async fn github_wait_install(
    app_id: u64,
    pem: &str,
    account_id: u64,
    cancel: &Cancel,
) -> Result<()> {
    let key = hq_watchman::config::parse_private_key(pem)?;
    let api = hq_watchman::github::GithubApi::new(
        http()?,
        app_id,
        &secrecy::SecretBox::new(Box::new(key)),
    )?;
    while !cancelled(cancel) {
        match api.installations().await {
            Ok(list) => {
                if list
                    .iter()
                    .any(|i| i.account.as_ref().is_some_and(|a| a.id == account_id))
                {
                    return Ok(());
                }
            }
            Err(e) => bail!("GitHub: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    bail!("cancelled")
}

fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml.get(start..)?.find(&format!("</{tag}>"))? + start;
    let v = xml.get(start..end)?.trim();
    Some(
        v.strip_prefix("<![CDATA[")
            .and_then(|x| x.strip_suffix("]]>"))
            .unwrap_or(v),
    )
}

/// Steam profile from a `SteamID64`, a profile link or a custom URL name.
pub async fn steam_profile(input: &str) -> Result<(u64, String)> {
    let t = input.trim().trim_end_matches('/');
    let path = if let Some(rest) = t.split("steamcommunity.com/profiles/").nth(1) {
        format!("profiles/{}", rest.split('/').next().unwrap_or_default())
    } else if let Some(rest) = t.split("steamcommunity.com/id/").nth(1) {
        format!("id/{}", rest.split('/').next().unwrap_or_default())
    } else if t.len() == 17 && t.bytes().all(|b| b.is_ascii_digit()) {
        format!("profiles/{t}")
    } else {
        format!("id/{t}")
    };
    let name = path.split('/').nth(1).unwrap_or_default();
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        bail!("not a Steam profile link or ID");
    }
    let body = http()?
        .get_bytes(
            &format!("https://steamcommunity.com/{path}/?xml=1"),
            1024 * 1024,
        )
        .await?;
    let xml = String::from_utf8_lossy(&body);
    let id: u64 = xml_tag(&xml, "steamID64")
        .and_then(|v| v.parse().ok())
        .filter(|v| hq_watchman::steam::is_individual_steam_id(*v))
        .ok_or_else(|| anyhow!("Steam profile not found"))?;
    let persona = xml_tag(&xml, "steamID").unwrap_or("?");
    Ok((id, hq_watchman::text::clean_line(persona, 64)))
}

#[derive(Deserialize)]
struct Summaries {
    response: SummaryList,
}

#[derive(Deserialize)]
struct SummaryList {
    #[serde(default)]
    players: Vec<serde_json::Value>,
}

/// Checks a Steam Web API key against the profile.
pub async fn steam_key(key: &str, profile: u64) -> Result<()> {
    let key = key.trim();
    if key.len() != 32 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("a Steam Web API key has 32 hex characters");
    }
    let steam = hq_watchman::steam::SteamHttp::new(http()?, Some(secret(key)));
    let s: Summaries = steam
        .api(
            "ISteamUser/GetPlayerSummaries/v2/",
            &[("steamids", profile.to_string())],
            true,
        )
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if s.response.players.is_empty() {
        bail!("the key works, but the profile was not found");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_needs_the_code() {
        assert!(has_code("/start Ab12", "Ab12"));
        assert!(has_code(" /start@my_bot Ab12 ", "Ab12"));
        assert!(has_code(" Ab12\n", "Ab12"));
        assert!(!has_code("/start", "Ab12"));
        assert!(!has_code("/start Ab13", "Ab12"));
        assert!(!has_code("/start Ab12 x", "Ab12"));
        assert!(!has_code("/startAb12", "Ab12"));
        assert!(!has_code("Ab12 x", "Ab12"));
    }

    #[test]
    fn xml_values() {
        let x = "<profile><steamID64>76561199508874215</steamID64><steamID><![CDATA[Name]]></steamID></profile>";
        assert_eq!(xml_tag(x, "steamID64"), Some("76561199508874215"));
        assert_eq!(xml_tag(x, "steamID"), Some("Name"));
        assert_eq!(xml_tag(x, "none"), None);
    }

    #[test]
    fn manifest_has_read_only_permissions() {
        let m = manifest(
            "someone-with-a-very-long-login-name",
            "https://x.org/a",
            "http://127.0.0.1:1/callback",
        );
        assert!(m["name"].as_str().is_some_and(|n| n.chars().count() <= 34));
        for (_, v) in m["default_permissions"].as_object().into_iter().flatten() {
            assert_eq!(v, "read");
        }
    }
}
