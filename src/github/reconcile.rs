// SPDX-License-Identifier: GPL-3.0-or-later
//! Periodic GitHub reconciliation: installations, repositories, failed deliveries.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Result;

use super::GhError;
use crate::app::App;
use crate::event::{Body, Event, Source};
use crate::storage::RepoRow;

const DELIVERY_HISTORY: i64 = 3 * 86_400;

pub async fn refresh_installations(app: &App) -> Result<(), GhError> {
    let list = app.github.installations().await?;
    let admin = app.secrets.github_admin_id;
    let mine: Vec<(i64, String, bool)> = list
        .into_iter()
        .filter(|i| i.account.as_ref().is_some_and(|a| a.id == admin))
        .filter_map(|i| {
            let login = i.account.map(|a| a.login).unwrap_or_default();
            Some((
                i64::try_from(i.id).ok()?,
                crate::text::clean_line(&login, 60),
                i.suspended_at.is_some(),
            ))
        })
        .collect();
    app.db.replace_installations(&mine).await?;
    app.set_installations(
        mine.iter()
            .filter(|(_, _, s)| !s)
            .map(|(id, _, _)| *id)
            .collect(),
    );
    Ok(())
}

async fn sync_repos(app: &App) -> Result<(), GhError> {
    let installations = app.db.installations().await?;
    let mut fresh: BTreeMap<i64, RepoRow> = BTreeMap::new();
    for (inst, _, suspended) in &installations {
        if *suspended {
            continue;
        }
        let Ok(id) = u64::try_from(*inst) else {
            continue;
        };
        for r in app.github.repositories(id).await? {
            let Ok(rid) = i64::try_from(r.id) else {
                continue;
            };
            fresh.insert(
                rid,
                RepoRow {
                    id: rid,
                    full_name: r.full_name,
                    private: r.private,
                    installation_id: *inst,
                },
            );
        }
    }
    let stored: BTreeMap<i64, RepoRow> = app
        .db
        .repos()
        .await?
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
    let baseline = app.db.meta_get("repos_baseline").await?.is_none();
    let now = crate::now();
    let mut events = Vec::new();
    let mut upsert = Vec::new();
    for (id, r) in &fresh {
        match stored.get(id) {
            Some(s)
                if s.full_name == r.full_name
                    && s.private == r.private
                    && s.installation_id == r.installation_id => {}
            Some(_) => upsert.push(r.clone()),
            None => {
                upsert.push(r.clone());
                if !baseline {
                    events.push(Event::new(
                        Source::Github,
                        format!("gh:repo:add:{id}:{now}"),
                        now,
                        Body::RepoWatched {
                            repo: r.full_name.clone(),
                            private: r.private,
                            removed: false,
                            url: format!("https://github.com/{}", r.full_name),
                        },
                    ));
                }
            }
        }
    }
    let removed: Vec<i64> = stored
        .keys()
        .filter(|id| !fresh.contains_key(id))
        .copied()
        .collect();
    for id in &removed {
        if let Some(r) = stored.get(id) {
            events.push(Event::service(
                format!("gh:repo:rm:{id}:{now}"),
                "Репозиторий снят с наблюдения",
                format!("Репозиторий {} больше не доступен App (удалён, переименован или исключён из установки).", r.full_name),
            ));
        }
    }
    if baseline {
        let private = fresh.values().filter(|r| r.private).count();
        events.push(Event::service(
            "bot:repos_baseline",
            "Репозитории GitHub взяты под наблюдение",
            format!(
                "Репозиториев: {} (приватных: {private}). Новые репозитории подключаются автоматически.",
                fresh.len()
            ),
        ));
    }
    app.db.apply_repos(&upsert, &removed, &events).await?;
    if baseline {
        app.db.meta_set("repos_baseline", "1").await?;
    }
    Ok(())
}

/// Asks GitHub to redeliver deliveries that never reached the bot. Returns how many.
async fn redeliver_failed(app: &App) -> Result<usize, GhError> {
    let deliveries = app
        .github
        .deliveries(crate::now() - DELIVERY_HISTORY)
        .await?;
    let mut by_guid: BTreeMap<String, Vec<&super::Delivery>> = BTreeMap::new();
    for d in &deliveries {
        by_guid.entry(d.guid.clone()).or_default().push(d);
    }
    let mut requested = 0usize;
    let mut failed_guids = BTreeSet::new();
    for (guid, attempts) in &by_guid {
        if attempts.iter().any(|d| (200..300).contains(&d.status_code))
            || app.db.has_delivery(guid).await?
        {
            continue;
        }
        failed_guids.insert(guid.clone());
        if let Some((at, n)) = app.db.redelivery_requested(guid).await? {
            if n >= 5 || crate::now() - at < 1800 {
                continue;
            }
        }
        let Some(latest) = attempts.iter().max_by_key(|d| d.id) else {
            continue;
        };
        app.github.redeliver(latest.id).await?;
        app.db.note_redelivery(guid).await?;
        requested += 1;
    }
    if failed_guids.is_empty() {
        app.health.ok("deliveries").await;
    } else {
        app.health
            .fail(
                "deliveries",
                &format!("в журнале GitHub неудачных доставок: {}, запрошена повторная доставка: {requested}", failed_guids.len()),
                1,
                "проверьте, что адрес вебхука доступен снаружи (DNS, nginx, сертификат)",
            )
            .await;
    }
    Ok(requested)
}

/// One full reconciliation pass.
pub async fn run_once(app: &App) {
    let result: Result<(), GhError> = async {
        refresh_installations(app).await?;
        sync_repos(app).await?;
        redeliver_failed(app).await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => app.health.ok("github").await,
        Err(GhError::Unauthorized) => {
            app.health
                .fail(
                    "github",
                    "GitHub отклонил ключ или ID App (401)",
                    1,
                    "проверьте HQW_GITHUB_APP_ID и HQW_GITHUB_APP_KEY",
                )
                .await;
        }
        Err(GhError::Forbidden(s)) => {
            app.health
                .fail(
                    "github",
                    &format!("нет доступа ({s}): установка App отозвана или не хватает прав"),
                    1,
                    "проверьте установку App и права Read для Metadata, Issues, Pull requests, Contents, Discussions",
                )
                .await;
        }
        Err(GhError::RateLimited(s)) => {
            tracing::warn!(seconds = s, "GitHub rate limit, reconciliation postponed");
        }
        Err(e @ GhError::Other(_)) => {
            app.health
                .fail(
                    "github",
                    &e.to_string(),
                    3,
                    "проверьте доступность api.github.com; бот повторит сам",
                )
                .await;
        }
    }
}

/// Runs at start, on schedule and on `/check_now github`.
pub async fn run(app: Arc<App>) -> Result<()> {
    loop {
        run_once(&app).await;
        let minutes = app.cfg().intervals.github_reconcile_minutes;
        app.health
            .beat("github_loop", i64::try_from(minutes)? * 60 + 600);
        tokio::select! {
            () = app.cancel.cancelled() => return Ok(()),
            () = app.check_now.github.notified() => {}
            () = tokio::time::sleep(std::time::Duration::from_secs(minutes * 60)) => {}
        }
    }
}
