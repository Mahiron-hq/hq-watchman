// SPDX-License-Identifier: GPL-3.0-or-later
pub mod bbcode;
pub mod cm;
pub mod comments;
pub mod games;
pub mod guides;
pub mod store;
pub mod vdf;

use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::net::{HttpClient, HttpError};

const STEAM_ID_BASE: u64 = 76_561_197_960_265_728;

pub fn is_individual_steam_id(id: u64) -> bool {
    id > STEAM_ID_BASE && id < STEAM_ID_BASE + (1 << 32)
}

pub fn account_to_steam_id(account: u32) -> u64 {
    STEAM_ID_BASE + u64::from(account)
}

pub fn store_url(appid: u32) -> String {
    format!("https://store.steampowered.com/app/{appid}/")
}

/// Sequential, paced access to Steam web hosts with backoff on 429 and 5xx.
pub struct SteamHttp {
    http: HttpClient,
    key: Option<SecretString>,
    last: Mutex<Option<Instant>>,
}

#[derive(Debug, thiserror::Error)]
pub enum SteamError {
    #[error("Steam Web API key rejected")]
    BadKey,
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl SteamHttp {
    pub fn new(http: HttpClient, key: Option<SecretString>) -> Self {
        Self {
            http,
            key,
            last: Mutex::new(None),
        }
    }

    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    async fn pace(&self) -> tokio::sync::MutexGuard<'_, Option<Instant>> {
        let last = self.last.lock().await;
        if let Some(prev) = *last {
            let jitter = rand::random_range(0..500u64);
            let ready = prev + Duration::from_millis(1000 + jitter);
            tokio::time::sleep_until(ready).await;
        }
        last
    }

    async fn run(
        &self,
        build: impl Fn() -> Result<reqwest::RequestBuilder>,
        limit: usize,
    ) -> Result<Bytes, SteamError> {
        let mut delay = Duration::from_secs(2);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut guard = self.pace().await;
            let result = self.http.send(build()?, limit).await;
            *guard = Some(Instant::now());
            drop(guard);
            match result {
                Ok(b) => return Ok(b),
                Err(HttpError::Status { status, .. })
                    if status.as_u16() == 401 || status.as_u16() == 403 =>
                {
                    return Err(SteamError::BadKey);
                }
                Err(HttpError::Status {
                    status,
                    retry_after,
                }) if (status.as_u16() == 429 || status.is_server_error()) && attempt < 4 => {
                    let wait = retry_after.map_or(delay, |s| Duration::from_secs(s.min(300)));
                    tokio::time::sleep(wait).await;
                    delay *= 2;
                }
                Err(HttpError::Other(_)) if attempt < 3 => {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(e) => return Err(SteamError::Other(anyhow!("Steam request failed: {e}"))),
            }
        }
    }

    /// GET a Web API method; `query` must not contain the key.
    pub async fn api<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
        with_key: bool,
    ) -> Result<T, SteamError> {
        let url = format!("https://api.steampowered.com/{path}");
        if with_key && self.key.is_none() {
            return Err(SteamError::BadKey);
        }
        let body = self
            .run(
                || {
                    let mut req = self.http.request(Method::GET, &url)?.query(query);
                    if let (true, Some(key)) = (with_key, &self.key) {
                        req = req.query(&[("key", key.expose_secret())]);
                    }
                    Ok(req)
                },
                crate::net::DEFAULT_LIMIT,
            )
            .await?;
        serde_json::from_slice(&body).map_err(|e| SteamError::Other(anyhow!("bad Steam JSON: {e}")))
    }

    pub async fn community_get(&self, url: &str, limit: usize) -> Result<Bytes, SteamError> {
        self.run(|| self.http.request(Method::GET, url), limit)
            .await
    }

    pub async fn community_post(
        &self,
        url: &str,
        form: &[(&str, String)],
        limit: usize,
    ) -> Result<Bytes, SteamError> {
        self.run(
            || Ok(self.http.request(Method::POST, url)?.form(form)),
            limit,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_ids() {
        assert!(is_individual_steam_id(76_561_199_508_874_215));
        assert!(!is_individual_steam_id(5));
        assert_eq!(account_to_steam_id(1_674_619_998), 76_561_199_634_885_726);
    }
}
