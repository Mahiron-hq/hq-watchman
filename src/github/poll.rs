// SPDX-License-Identifier: GPL-3.0-or-later
//! Polling mode: GitHub events read from the App delivery log, no public endpoint needed.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use super::webhook::{process, valid_event_name, valid_guid};
use super::GhError;
use crate::app::App;

const HISTORY: i64 = 3 * 86_400;
const OVERLAP: i64 = 600;
const MAX_PER_PASS: usize = 300;

/// Reads deliveries newer than the last pass and processes those not seen yet.
async fn poll_once(app: &Arc<App>) -> Result<usize, GhError> {
    let Some(gh) = app.github.as_ref() else {
        return Ok(0);
    };
    let now = crate::now();
    let since = app
        .db
        .meta_get("poll_since")
        .await?
        .and_then(|v| v.parse::<i64>().ok())
        .map_or(now - HISTORY, |t| (t - OVERLAP).max(now - HISTORY));
    let (mut list, whole) = gh.deliveries(since).await?;
    if !whole {
        tracing::warn!("GitHub delivery log longer than the page limit, the polling mark is kept");
    }
    list.sort_by_key(|d| d.id);
    let mut newest = since + OVERLAP;
    let mut done = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    let mut complete = true;
    for d in list {
        let ts = super::parse_ts(&d.delivered_at).unwrap_or(now);
        if !seen.insert(d.guid.clone()) || app.db.has_delivery(&d.guid).await? {
            newest = newest.max(ts);
            continue;
        }
        if done >= MAX_PER_PASS {
            complete = false;
            break;
        }
        let detail = gh.delivery(d.id).await?;
        let event = detail.event.as_str();
        if !valid_guid(&detail.guid) || !valid_event_name(event) {
            app.db.accept_delivery(&d.guid, &[]).await?;
            continue;
        }
        let body = if let Some(v) = detail.request.payload {
            serde_json::to_vec(&v).map_err(|e| GhError::Other(e.to_string()))?
        } else {
            app.db.accept_delivery(&detail.guid, &[]).await?;
            continue;
        };
        process(app, event, &detail.guid, &body).await?;
        newest = newest.max(ts);
        done += 1;
    }
    if complete {
        newest = newest.max(now);
    }
    if whole {
        app.db.meta_set("poll_since", &newest.to_string()).await?;
    }
    Ok(done)
}

pub async fn run(app: Arc<App>) -> Result<()> {
    loop {
        let interval = app.cfg().github.poll_seconds;
        if crate::settings::current().github.enabled {
            match poll_once(&app).await {
                Ok(n) => {
                    app.health.ok("deliveries").await;
                    if n > 0 {
                        app.outbox_signal.notify_one();
                    }
                }
                Err(GhError::RateLimited(s)) => {
                    tracing::warn!(seconds = s, "GitHub rate limit, polling postponed");
                    tokio::time::sleep(Duration::from_secs(s.min(900))).await;
                }
                Err(e) => {
                    app.health
                        .fail(
                            "deliveries",
                            &e.to_string(),
                            5,
                            crate::tr!(
                                "проверьте доступность api.github.com и ключ App; бот повторит сам",
                                "check that api.github.com is reachable and the App key; the bot retries by itself"
                            ),
                        )
                        .await;
                }
            }
        }
        app.health
            .beat("github_poll", i64::try_from(interval)? * 3 + 600);
        tokio::select! {
            () = app.cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep(Duration::from_secs(interval)) => {}
        }
    }
}
