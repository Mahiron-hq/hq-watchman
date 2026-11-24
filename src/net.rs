// SPDX-License-Identifier: GPL-3.0-or-later
//! Outgoing HTTP: allow-listed hosts, timeouts, size caps, no URLs in errors.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use reqwest::header::HeaderMap;
use reqwest::{redirect, Method, RequestBuilder, StatusCode, Url};
use serde::de::DeserializeOwned;

const ALLOWED_HOSTS: &[&str] = &[
    "api.telegram.org",
    "api.github.com",
    "api.steampowered.com",
    "steamcommunity.com",
    "store.steampowered.com",
];

pub const DEFAULT_LIMIT: usize = 8 * 1024 * 1024;

pub fn host_allowed(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url
            .host_str()
            .is_some_and(|h| ALLOWED_HOSTS.iter().any(|a| h.eq_ignore_ascii_case(a)))
}

/// HTTP error with the status kept for retry decisions.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("HTTP {status}")]
    Status {
        status: StatusCode,
        retry_after: Option<u64>,
    },
    #[error("{0}")]
    Other(String),
}

impl HttpError {
    pub fn status(&self) -> Option<StatusCode> {
        match self {
            Self::Status { status, .. } => Some(*status),
            Self::Other(_) => None,
        }
    }
}

#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
}

impl HttpClient {
    pub fn new(user_agent: &str) -> Result<Self> {
        let policy = redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !host_allowed(attempt.url()) {
                attempt.stop()
            } else {
                attempt.follow()
            }
        });
        let inner = reqwest::Client::builder()
            .user_agent(user_agent)
            .https_only(true)
            .redirect(policy)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| anyhow!("HTTP client: {}", e.without_url()))?;
        Ok(Self { inner })
    }

    pub fn request(&self, method: Method, url: &str) -> Result<RequestBuilder> {
        let parsed = Url::parse(url).map_err(|_| anyhow!("invalid URL"))?;
        if !host_allowed(&parsed) {
            bail!("host not allowed: {}", parsed.host_str().unwrap_or("?"));
        }
        Ok(self.inner.request(method, parsed))
    }

    /// Sends the request; any HTTP status is returned as data, only transport errors fail.
    pub async fn send_full(
        &self,
        req: RequestBuilder,
        limit: usize,
    ) -> Result<(StatusCode, HeaderMap, Bytes), HttpError> {
        let mut resp = req
            .send()
            .await
            .map_err(|e| HttpError::Other(describe(&e)))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        if resp.content_length().is_some_and(|l| l > limit as u64) {
            return Err(HttpError::Other("response too large".into()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| HttpError::Other(describe(&e)))?
        {
            if body.len() + chunk.len() > limit {
                return Err(HttpError::Other("response too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, headers, Bytes::from(body)))
    }

    /// Sends the request and returns the body of a successful response.
    pub async fn send(&self, req: RequestBuilder, limit: usize) -> Result<Bytes, HttpError> {
        let (status, headers, body) = self.send_full(req, limit).await?;
        if !status.is_success() {
            return Err(HttpError::Status {
                status,
                retry_after: retry_after(&headers),
            });
        }
        Ok(body)
    }

    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let body = self
            .send(self.request(Method::GET, url)?, DEFAULT_LIMIT)
            .await?;
        serde_json::from_slice(&body).map_err(|e| anyhow!("bad JSON: {e}"))
    }

    pub async fn get_bytes(&self, url: &str, limit: usize) -> Result<Bytes> {
        Ok(self.send(self.request(Method::GET, url)?, limit).await?)
    }

    /// For the PICS content host that Steam names at run time.
    pub async fn get_bytes_any_host(&self, url: &str) -> Result<Bytes> {
        let parsed = Url::parse(url).map_err(|_| anyhow!("invalid URL"))?;
        if parsed.scheme() != "https" {
            bail!("only https is allowed");
        }
        Ok(self.send(self.inner.get(parsed), DEFAULT_LIMIT).await?)
    }
}

/// Seconds to wait from `Retry-After` or an exhausted GitHub rate limit.
pub fn retry_after(headers: &HeaderMap) -> Option<u64> {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    if let Some(v) = get("retry-after").and_then(|v| v.parse().ok()) {
        return Some(v);
    }
    if get("x-ratelimit-remaining") == Some("0") {
        let reset: i64 = get("x-ratelimit-reset")?.parse().ok()?;
        return u64::try_from((reset - crate::now()).max(1)).ok();
    }
    None
}

fn describe(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_redirect() {
        "redirect refused"
    } else if e.is_body() || e.is_decode() {
        "bad response body"
    } else {
        "request failed"
    };
    let mut source = std::error::Error::source(e);
    let mut detail = String::new();
    while let Some(s) = source {
        detail = s.to_string();
        source = s.source();
    }
    if detail.is_empty() || detail.contains("http") {
        kind.to_owned()
    } else {
        format!("{kind}: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn errors_never_contain_the_url() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let token = "123456:SECRETtokenSECRETtokenSECRETtoken12";
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        let err = client
            .get(format!("https://127.0.0.1:9/bot{token}/getMe?key=STEAMKEY"))
            .send()
            .await
            .unwrap_err();
        let text = describe(&err);
        assert!(
            !text.contains("SECRET") && !text.contains("STEAMKEY"),
            "{text}"
        );
        let e = HttpError::Other(text);
        assert!(!format!("{e} {e:?}").contains("SECRET"));
    }

    #[test]
    fn allow_list() {
        let ok = |s: &str| host_allowed(&Url::parse(s).unwrap());
        assert!(ok("https://api.github.com/app"));
        assert!(ok("https://steamcommunity.com/x"));
        assert!(!ok("http://api.github.com/app"));
        assert!(!ok("https://api.github.com:8443/"));
        assert!(!ok("https://evil.com/"));
        assert!(!ok("https://api.github.com.evil.com/"));
        assert!(!ok("https://user@evil.com/"));
    }
}
