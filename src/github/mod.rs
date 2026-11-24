// SPDX-License-Identifier: GPL-3.0-or-later
//! GitHub App client: JWT, installation tokens, repositories and deliveries.

pub mod payload;
pub mod reconcile;
pub mod webhook;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use reqwest::{Method, StatusCode};
use secrecy::{ExposeSecret, SecretBox, SecretString};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::net::{HttpClient, HttpError};

const API: &str = "https://api.github.com";

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("GitHub App key or ID rejected (401)")]
    Unauthorized,
    #[error("installation is gone or lacks permissions ({0})")]
    Forbidden(StatusCode),
    #[error("rate limited for {0} s")]
    RateLimited(u64),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for GhError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(format!("{e:#}"))
    }
}

pub fn valid_full_name(s: &str) -> bool {
    let Some((owner, repo)) = s.split_once('/') else {
        return false;
    };
    let ok = |p: &str| {
        !p.is_empty()
            && p.len() <= 100
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    };
    ok(owner) && ok(repo)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub id: u64,
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Installation {
    pub id: u64,
    pub account: Option<Account>,
    #[serde(default)]
    pub suspended_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiRepo {
    pub id: u64,
    pub full_name: String,
    #[serde(default)]
    pub private: bool,
}

#[derive(Deserialize)]
struct RepoPage {
    total_count: usize,
    repositories: Vec<ApiRepo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Delivery {
    pub id: u64,
    pub guid: String,
    pub delivered_at: String,
    #[serde(default)]
    pub status_code: u16,
    #[serde(default)]
    pub redelivery: bool,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

pub struct GithubApi {
    http: HttpClient,
    app_id: u64,
    key: ring::rsa::KeyPair,
    tokens: Mutex<HashMap<u64, (SecretString, i64)>>,
}

impl GithubApi {
    /// `key` holds a format byte (1 = PKCS#8, 0 = PKCS#1) followed by DER.
    pub fn new(http: HttpClient, app_id: u64, key: &SecretBox<Vec<u8>>) -> Result<Self> {
        let raw = key.expose_secret();
        let (fmt, der) = raw.split_first().ok_or_else(|| anyhow!("empty key"))?;
        let key = if *fmt == 1 {
            ring::rsa::KeyPair::from_pkcs8(der)
        } else {
            ring::rsa::KeyPair::from_der(der)
        }
        .map_err(|_| anyhow!("unusable GitHub App key"))?;
        Ok(Self {
            http,
            app_id,
            key,
            tokens: Mutex::new(HashMap::new()),
        })
    }

    fn jwt(&self) -> Result<SecretString> {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let now = crate::now();
        let header = b64.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = b64.encode(format!(
            r#"{{"iat":{},"exp":{},"iss":"{}"}}"#,
            now - 60,
            now + 540,
            self.app_id
        ));
        let msg = format!("{header}.{claims}");
        let mut sig = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                msg.as_bytes(),
                &mut sig,
            )
            .map_err(|_| anyhow!("JWT signing failed"))?;
        Ok(SecretString::from(format!("{msg}.{}", b64.encode(sig))))
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        bearer: &SecretString,
        body: Option<&serde_json::Value>,
    ) -> Result<(reqwest::header::HeaderMap, bytes::Bytes), GhError> {
        let url = format!("{API}{path}");
        let mut req = self
            .http
            .request(method, &url)?
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(bearer.expose_secret())
            .timeout(Duration::from_secs(20));
        if let Some(b) = body {
            req = req.json(b);
        }
        let (status, headers, bytes) = self
            .http
            .send_full(req, crate::net::DEFAULT_LIMIT)
            .await
            .map_err(|e| GhError::Other(e.to_string()))?;
        if status.is_success() {
            return Ok((headers, bytes));
        }
        if let Some(wait) =
            crate::net::retry_after(&headers).filter(|_| status == 403 || status == 429)
        {
            return Err(GhError::RateLimited(wait));
        }
        Err(match status.as_u16() {
            401 => GhError::Unauthorized,
            403 | 404 => GhError::Forbidden(status),
            _ => GhError::Other(
                HttpError::Status {
                    status,
                    retry_after: None,
                }
                .to_string(),
            ),
        })
    }

    async fn app_get<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<(reqwest::header::HeaderMap, T), GhError> {
        let jwt = self.jwt()?;
        let (h, b) = self.call(Method::GET, path, &jwt, None).await?;
        let v = serde_json::from_slice(&b).map_err(|e| GhError::Other(format!("bad JSON: {e}")))?;
        Ok((h, v))
    }

    pub async fn installations(&self) -> Result<Vec<Installation>, GhError> {
        let mut out = Vec::new();
        for page in 1..=10 {
            let (_, list): (_, Vec<Installation>) = self
                .app_get(&format!("/app/installations?per_page=100&page={page}"))
                .await?;
            let n = list.len();
            out.extend(list);
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }

    async fn installation_token(&self, installation: u64) -> Result<SecretString, GhError> {
        let mut tokens = self.tokens.lock().await;
        let now = crate::now();
        if let Some((t, exp)) = tokens.get(&installation) {
            if *exp - now > 300 {
                return Ok(t.clone());
            }
        }
        let jwt = self.jwt()?;
        let (_, body) = self
            .call(
                Method::POST,
                &format!("/app/installations/{installation}/access_tokens"),
                &jwt,
                None,
            )
            .await?;
        let resp: TokenResponse =
            serde_json::from_slice(&body).map_err(|e| GhError::Other(format!("bad JSON: {e}")))?;
        let token = SecretString::from(resp.token);
        tokens.insert(installation, (token.clone(), now + 3300));
        Ok(token)
    }

    pub async fn repositories(&self, installation: u64) -> Result<Vec<ApiRepo>, GhError> {
        let token = self.installation_token(installation).await?;
        let mut out = Vec::new();
        for page in 1..=50 {
            let (_, body) = self
                .call(
                    Method::GET,
                    &format!("/installation/repositories?per_page=100&page={page}"),
                    &token,
                    None,
                )
                .await?;
            let p: RepoPage = serde_json::from_slice(&body)
                .map_err(|e| GhError::Other(format!("bad JSON: {e}")))?;
            let n = p.repositories.len();
            out.extend(
                p.repositories
                    .into_iter()
                    .filter(|r| valid_full_name(&r.full_name)),
            );
            if n == 0 || out.len() >= p.total_count {
                break;
            }
        }
        Ok(out)
    }

    /// Webhook deliveries newer than `since` (unix seconds), newest first.
    pub async fn deliveries(&self, since: i64) -> Result<Vec<Delivery>, GhError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..30 {
            let path = match &cursor {
                Some(c) => format!("/app/hook/deliveries?per_page=100&cursor={c}"),
                None => "/app/hook/deliveries?per_page=100".to_owned(),
            };
            let (headers, page): (_, Vec<Delivery>) = self.app_get(&path).await?;
            let mut done = page.is_empty();
            for d in page {
                let ts = parse_ts(&d.delivered_at);
                if ts.is_some_and(|t| t < since) {
                    done = true;
                    continue;
                }
                out.push(d);
            }
            cursor = next_cursor(&headers);
            if done || cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    pub async fn redeliver(&self, delivery_id: u64) -> Result<(), GhError> {
        let jwt = self.jwt()?;
        self.call(
            Method::POST,
            &format!("/app/hook/deliveries/{delivery_id}/attempts"),
            &jwt,
            None,
        )
        .await?;
        Ok(())
    }

    /// Webhook IP ranges from the public meta endpoint.
    pub async fn hook_ranges(&self) -> Result<Vec<ipnet::IpNet>> {
        #[derive(Deserialize)]
        struct Meta {
            hooks: Vec<String>,
        }
        let meta: Meta = self.http.get_json(&format!("{API}/meta")).await?;
        let nets: Vec<ipnet::IpNet> = meta.hooks.iter().filter_map(|h| h.parse().ok()).collect();
        if nets.is_empty() {
            bail!("GitHub meta has no hook ranges");
        }
        Ok(nets)
    }
}

pub fn parse_ts(s: &str) -> Option<i64> {
    s.parse::<jiff::Timestamp>()
        .ok()
        .map(jiff::Timestamp::as_second)
}

fn next_cursor(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let link = headers.get("link")?.to_str().ok()?;
    link.split(',')
        .find(|part| part.contains("rel=\"next\""))
        .and_then(|part| part.split("cursor=").nth(1))
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '=' | '_' | '-' | '%'))
                .collect::<String>()
        })
        .filter(|c| !c.is_empty() && c.len() < 512)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_names() {
        assert!(valid_full_name("Mahiron-hq/hq-watchman"));
        assert!(valid_full_name("a/b.c_d"));
        assert!(!valid_full_name("a/../b"));
        assert!(!valid_full_name("a/.."));
        assert!(!valid_full_name("a"));
        assert!(!valid_full_name("a/b c"));
        assert!(!valid_full_name("a/<b>"));
    }

    #[test]
    fn cursor_from_link() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "link",
            "<https://api.github.com/app/hook/deliveries?per_page=100&cursor=v1_123>; rel=\"next\""
                .parse()
                .unwrap(),
        );
        assert_eq!(next_cursor(&h).as_deref(), Some("v1_123"));
        h.insert("link", "<https://x>; rel=\"prev\"".parse().unwrap());
        assert_eq!(next_cursor(&h), None);
    }
}
