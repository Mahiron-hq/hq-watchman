// SPDX-License-Identifier: GPL-3.0-or-later
//! Webhook endpoint: one POST route, signature before parsing, strict limits.

use std::collections::HashMap;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::HeaderMap;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use secrecy::ExposeSecret;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use super::payload;
use crate::app::App;
use crate::text::log_safe;

const READ_TIMEOUT: Duration = Duration::from_secs(10);
const CONN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TRACKED_IPS: usize = 10_000;

/// Fixed-window counters per IP and in total.
struct Limiter {
    per_minute: u32,
    window: i64,
    total: u32,
    ips: HashMap<IpAddr, u32>,
}

impl Limiter {
    fn allow(&mut self, ip: IpAddr) -> bool {
        let now = crate::now() / 60;
        if now != self.window {
            self.window = now;
            self.total = 0;
            self.ips.clear();
        }
        if self.total >= self.per_minute.saturating_mul(4) {
            return false;
        }
        if self.ips.len() >= MAX_TRACKED_IPS && !self.ips.contains_key(&ip) {
            return false;
        }
        let n = self.ips.entry(ip).or_insert(0);
        if *n >= self.per_minute {
            return false;
        }
        *n += 1;
        self.total += 1;
        true
    }
}

pub struct Endpoint {
    app: Arc<App>,
    path: String,
    max_body: usize,
    ip_filter: bool,
    trusted: Vec<ipnet::IpNet>,
    key: ring::hmac::Key,
    limiter: Mutex<Limiter>,
    bad_signatures: Mutex<(i64, u32, i64)>,
    last_refresh: Mutex<i64>,
}

fn empty(status: StatusCode) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = status;
    r
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
        .collect()
}

/// Constant-time check of `X-Hub-Signature-256`.
pub fn signature_valid(key: &ring::hmac::Key, header_value: Option<&str>, body: &[u8]) -> bool {
    let Some(sig) = header_value
        .and_then(|v| v.strip_prefix("sha256="))
        .filter(|v| v.len() == 64)
        .and_then(decode_hex)
    else {
        return false;
    };
    ring::hmac::verify(key, body, &sig).is_ok()
}

impl Endpoint {
    pub fn new(app: Arc<App>) -> Self {
        let cfg = app.cfg();
        let key = ring::hmac::Key::new(
            ring::hmac::HMAC_SHA256,
            app.secrets.github_webhook_secret.expose_secret(),
        );
        Self {
            path: cfg.webhook.path.clone(),
            max_body: cfg.webhook.max_body_bytes,
            ip_filter: cfg.webhook.ip_filter,
            trusted: cfg.webhook.trusted_proxies.clone(),
            limiter: Mutex::new(Limiter {
                per_minute: cfg.webhook.requests_per_minute,
                window: 0,
                total: 0,
                ips: HashMap::new(),
            }),
            key,
            app,
            bad_signatures: Mutex::new((0, 0, 0)),
            last_refresh: Mutex::new(0),
        }
    }

    /// Client address, taken from proxy headers only when the peer is a trusted proxy.
    fn client_ip(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        if !self.trusted.iter().any(|n| n.contains(&peer)) {
            return peer;
        }
        if let Some(xff) = header(headers, "x-forwarded-for") {
            let hops: Vec<IpAddr> = xff
                .split(',')
                .filter_map(|p| p.trim().parse().ok())
                .collect();
            if let Some(ip) = hops
                .iter()
                .rev()
                .find(|ip| !self.trusted.iter().any(|n| n.contains(*ip)))
            {
                return *ip;
            }
        }
        header(headers, "x-real-ip")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(peer)
    }

    fn ip_allowed(&self, ip: IpAddr) -> bool {
        if !self.ip_filter {
            return true;
        }
        let ranges = self
            .app
            .hook_ranges
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 @ IpAddr::V4(_) => v4,
        };
        match ranges.as_ref() {
            Some(nets) => nets.iter().any(|n| n.contains(&ip)),
            None => true,
        }
    }

    async fn note_bad_signature(&self) {
        let now = crate::now();
        let alert = {
            let mut g = self
                .bad_signatures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (window, count, last_alert) = &mut *g;
            if now - *window >= 3600 {
                *window = now;
                *count = 0;
            }
            *count += 1;
            if *count > 10 && now - *last_alert >= 3600 {
                *last_alert = now;
                true
            } else {
                false
            }
        };
        if alert {
            let ev = crate::event::Event::service(
                format!("bot:badsig:{}", now / 3600),
                "Вебхук: много запросов с неверной подписью",
                "За последний час пришло больше 10 запросов с неверной подписью. Возможна атака или изменился секрет вебхука. Если секрет меняли, обновите HQW_GITHUB_WEBHOOK_SECRET.",
            );
            if let Err(e) = self.app.db.enqueue(&ev).await {
                tracing::error!(error = %e, "cannot queue alert");
            }
        }
    }

    async fn refresh_installations_once_a_minute(&self) {
        let now = crate::now();
        {
            let mut last = self
                .last_refresh
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if now - *last < 60 {
                return;
            }
            *last = now;
        }
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            super::reconcile::refresh_installations(&self.app),
        )
        .await;
    }

    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<Full<Bytes>> {
        if req.method() != Method::POST || req.uri().path() != self.path {
            return empty(StatusCode::NOT_FOUND);
        }
        let ip = self.client_ip(peer.ip(), req.headers());
        if !self.ip_allowed(ip) {
            tracing::warn!(ip = %ip, "webhook from outside GitHub ranges");
            return empty(StatusCode::NOT_FOUND);
        }
        let allowed = self
            .limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .allow(ip);
        if !allowed {
            return empty(StatusCode::TOO_MANY_REQUESTS);
        }
        let headers = req.headers().clone();
        if header(&headers, "content-length")
            .and_then(|v| v.parse::<usize>().ok())
            .is_some_and(|l| l > self.max_body)
        {
            return empty(StatusCode::PAYLOAD_TOO_LARGE);
        }
        let body = match tokio::time::timeout(
            READ_TIMEOUT,
            Limited::new(req.into_body(), self.max_body).collect(),
        )
        .await
        {
            Ok(Ok(b)) => b.to_bytes(),
            Ok(Err(_)) => return empty(StatusCode::PAYLOAD_TOO_LARGE),
            Err(_) => return empty(StatusCode::REQUEST_TIMEOUT),
        };
        if !signature_valid(&self.key, header(&headers, "x-hub-signature-256"), &body) {
            tracing::warn!(ip = %ip, "webhook signature invalid");
            self.note_bad_signature().await;
            return empty(StatusCode::UNAUTHORIZED);
        }
        let event = header(&headers, "x-github-event").filter(|e| {
            !e.is_empty() && e.len() <= 64 && e.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        });
        let guid = header(&headers, "x-github-delivery").filter(|g| {
            !g.is_empty() && g.len() <= 64 && g.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        });
        let (Some(event), Some(guid)) = (event, guid) else {
            return empty(StatusCode::BAD_REQUEST);
        };
        self.app.health.ok("webhook").await;
        match self.process(event, guid, &body).await {
            Ok(()) => empty(StatusCode::OK),
            Err(e) => {
                tracing::error!(delivery = %guid, error = %e, "webhook not stored");
                empty(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }

    async fn process(&self, event: &str, guid: &str, body: &[u8]) -> Result<()> {
        let cfg = self.app.cfg();
        let mut known = self.app.known_installations();
        let mut outcome = None;
        for attempt in 0..2 {
            let ctx = payload::Context {
                admin_id: self.app.secrets.github_admin_id,
                options: &cfg.github,
                known_installations: &known,
            };
            match payload::parse(event, body, &ctx) {
                Ok(o) if o.unknown_installation && attempt == 0 => {
                    self.refresh_installations_once_a_minute().await;
                    known = self.app.known_installations();
                }
                Ok(o) => {
                    outcome = Some(o);
                    break;
                }
                Err(e) => {
                    tracing::warn!(delivery = %guid, event = %event, error = %log_safe(&e.to_string()), "webhook payload rejected");
                    self.app.db.accept_delivery(guid, &[]).await?;
                    return Ok(());
                }
            }
        }
        let Some(outcome) = outcome else {
            return Ok(());
        };
        if outcome.unknown_installation {
            tracing::warn!(delivery = %guid, "webhook from unknown installation dropped");
        }
        if let Some(reason) = outcome.ignored {
            tracing::debug!(delivery = %guid, event = %event, reason, "webhook ignored");
        }
        let fresh = self.app.db.accept_delivery(guid, &outcome.events).await?;
        if fresh && (!outcome.repos_added.is_empty() || !outcome.repos_removed.is_empty()) {
            self.app
                .db
                .apply_repos(&outcome.repos_added, &outcome.repos_removed, &[])
                .await?;
        }
        if fresh && outcome.installation_changed {
            let app = Arc::clone(&self.app);
            tokio::spawn(async move {
                if let Err(e) = super::reconcile::refresh_installations(&app).await {
                    tracing::warn!(error = %e, "installation refresh failed");
                }
            });
        }
        tracing::info!(delivery = %guid, event = %event, events = outcome.events.len(), fresh, "webhook accepted");
        Ok(())
    }
}

/// Reloads the certificate when the files change.
#[derive(Debug)]
struct CertResolver {
    current: RwLock<Arc<rustls::sign::CertifiedKey>>,
}

impl rustls::server::ResolvesServerCert for CertResolver {
    fn resolve(
        &self,
        _hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(Arc::clone(
            &self
                .current
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        ))
    }
}

fn load_cert(cert: &Path, key: &Path) -> Result<rustls::sign::CertifiedKey> {
    use rustls_pki_types::pem::PemObject;
    let certs: Vec<_> = rustls_pki_types::CertificateDer::pem_reader_iter(BufReader::new(
        std::fs::File::open(cert).with_context(|| format!("cannot open {}", cert.display()))?,
    ))
    .collect::<Result<_, _>>()
    .map_err(|_| anyhow!("bad certificate file"))?;
    let key = rustls_pki_types::PrivateKeyDer::from_pem_file(key)
        .map_err(|_| anyhow!("bad TLS key file"))?;
    let provider = rustls::crypto::ring::default_provider();
    let signing = provider
        .key_provider
        .load_private_key(key)
        .map_err(|_| anyhow!("unsupported TLS key"))?;
    Ok(rustls::sign::CertifiedKey::new(certs, signing))
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn tls_acceptor(app: &Arc<App>) -> Result<Option<tokio_rustls::TlsAcceptor>> {
    let cfg = app.cfg();
    let Some(tls) = cfg.webhook.tls.clone() else {
        return Ok(None);
    };
    let resolver = Arc::new(CertResolver {
        current: RwLock::new(Arc::new(load_cert(&tls.cert_file, &tls.key_file)?)),
    });
    let mut server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_cert_resolver(Arc::clone(&resolver) as Arc<dyn rustls::server::ResolvesServerCert>);
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    let cancel = app.cancel.clone();
    tokio::spawn(async move {
        let mut stamp = (mtime(&tls.cert_file), mtime(&tls.key_file));
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
            let now = (mtime(&tls.cert_file), mtime(&tls.key_file));
            if now == stamp {
                continue;
            }
            match load_cert(&tls.cert_file, &tls.key_file) {
                Ok(k) => {
                    *resolver
                        .current
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(k);
                    stamp = now;
                    tracing::info!("TLS certificate reloaded");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "TLS certificate reload failed, keeping the old one");
                }
            }
        }
    });
    Ok(Some(tokio_rustls::TlsAcceptor::from(Arc::new(server))))
}

/// Runs until cancelled; in-flight requests get up to 10 seconds.
pub async fn serve(app: Arc<App>) -> Result<()> {
    let cfg = app.cfg();
    let listener = TcpListener::bind(cfg.webhook.listen)
        .await
        .with_context(|| format!("cannot listen on {}", cfg.webhook.listen))?;
    let tls = tls_acceptor(&app)?;
    let endpoint = Arc::new(Endpoint::new(Arc::clone(&app)));
    let slots = Arc::new(Semaphore::new(cfg.webhook.max_connections));
    tracing::info!(addr = %cfg.webhook.listen, tls = tls.is_some(), "webhook listening");
    loop {
        let (stream, peer) = tokio::select! {
            () = app.cancel.cancelled() => break,
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let endpoint = Arc::clone(&endpoint);
        let tls = tls.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = hyper::service::service_fn(move |req| {
                let endpoint = Arc::clone(&endpoint);
                async move { Ok::<_, std::convert::Infallible>(endpoint.handle(req, peer).await) }
            });
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(READ_TIMEOUT)
                .max_buf_size(64 * 1024)
                .keep_alive(true);
            let fut = async {
                match tls {
                    Some(acceptor) => {
                        let Ok(Ok(s)) =
                            tokio::time::timeout(READ_TIMEOUT, acceptor.accept(stream)).await
                        else {
                            return;
                        };
                        let _ = builder.serve_connection(TokioIo::new(s), service).await;
                    }
                    None => {
                        let _ = builder
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    }
                }
            };
            let _ = tokio::time::timeout(CONN_TIMEOUT, fut).await;
        });
    }
    let all = u32::try_from(cfg.webhook.max_connections).unwrap_or(u32::MAX);
    let _ = tokio::time::timeout(Duration::from_secs(10), slots.acquire_many(all)).await;
    tracing::info!("webhook stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_check() {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"It's a Secret to Everybody");
        let body = b"Hello, World!";
        let good = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(signature_valid(&key, Some(good), body));
        assert!(!signature_valid(&key, Some(good), b"Hello, World?"));
        assert!(!signature_valid(&key, None, body));
        assert!(!signature_valid(&key, Some(""), body));
        assert!(!signature_valid(&key, Some("sha1=abc"), body));
        assert!(!signature_valid(&key, Some(&good.replace('7', "z")), body));
    }

    #[test]
    fn limiter_caps_per_ip() {
        let mut l = Limiter {
            per_minute: 2,
            window: crate::now() / 60,
            total: 0,
            ips: HashMap::new(),
        };
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert!(l.allow(ip));
        assert!(l.allow(ip));
        assert!(!l.allow(ip));
        assert!(l.allow("1.2.3.5".parse().unwrap()));
    }
}
