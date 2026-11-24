// SPDX-License-Identifier: GPL-3.0-or-later
//! SQLite storage: schema, queue, snapshots and backups.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{SqliteConnection, SqlitePool};

use crate::event::{Body, Event};

pub const KIND_EVENT: i64 = 0;
pub const KIND_SERVICE: i64 = 1;
pub const KIND_REPLY: i64 = 2;

pub const STATUS_PENDING: i64 = 0;
pub const STATUS_SENT: i64 = 1;
pub const STATUS_DROPPED: i64 = 2;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub id: i64,
    pub kind: i64,
    pub source: String,
    pub body: String,
    pub occurred_at: i64,
    pub created_at: i64,
    pub attempts: i64,
    pub reply_to: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct RepoRow {
    pub id: i64,
    pub full_name: String,
    pub private: bool,
    pub installation_id: i64,
}

#[derive(Debug, Clone)]
pub struct GuideRow {
    pub id: i64,
    pub title: String,
    pub game: String,
    pub comment_count: i64,
    pub missing_checks: i64,
    pub last_full_read: Option<i64>,
    pub baseline_done: bool,
}

#[derive(Debug, Clone)]
pub struct CommentRow {
    pub comment_id: String,
    pub author: String,
    pub author_url: String,
    pub author_steamid: i64,
    pub posted_at: i64,
    pub text: String,
    pub missing_reads: i64,
}

#[derive(Debug, Clone)]
pub struct StrangerRow {
    pub user_id: i64,
    pub username: Option<String>,
    pub attempts: i64,
    pub last_at: i64,
    pub last_reply_at: Option<i64>,
}

fn create_private_file(path: &Path) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot create {}", path.display())),
    }
}

impl Db {
    pub async fn open(path: &Path, backups: usize) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let existed = path.exists();
        create_private_file(path)?;
        let opts = SqliteConnectOptions::from_str("sqlite:")
            .map_err(|e| anyhow!("{e}"))?
            .filename(path)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .context("cannot open database")?;
        let db = Self {
            pool,
            path: path.to_owned(),
        };
        if existed && db.has_pending_migrations().await? {
            db.backup(backups)
                .await
                .context("backup before migration")?;
        }
        MIGRATOR
            .run(&db.pool)
            .await
            .context("database migration failed")?;
        Ok(db)
    }

    #[cfg(test)]
    pub async fn memory() -> Result<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|e| anyhow!("{e}"))?
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            path: PathBuf::new(),
        })
    }

    async fn has_pending_migrations(&self) -> Result<bool> {
        let table: Option<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
        )
        .fetch_optional(&self.pool)
        .await?;
        if table.is_none() {
            return Ok(false);
        }
        let applied: Vec<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
            .fetch_all(&self.pool)
            .await?;
        Ok(MIGRATOR.iter().any(|m| !applied.contains(&m.version)))
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    // meta

    pub async fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar!("SELECT value FROM meta WHERE key = ?", key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query!(
            "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            key,
            value
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn meta_del(&self, key: &str) -> Result<()> {
        sqlx::query!("DELETE FROM meta WHERE key = ?", key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // outbox

    pub async fn enqueue(&self, ev: &Event) -> Result<Option<i64>> {
        let mut tx = self.pool.begin().await?;
        let id = insert_event(&mut tx, ev, 0).await?;
        tx.commit().await?;
        Ok(id)
    }

    pub async fn enqueue_all(&self, events: &[Event], merge_window_secs: i64) -> Result<usize> {
        let mut tx = self.pool.begin().await?;
        let mut n = 0;
        for ev in events {
            if insert_event(&mut tx, ev, merge_window_secs)
                .await?
                .is_some()
            {
                n += 1;
            }
        }
        tx.commit().await?;
        Ok(n)
    }

    pub async fn enqueue_reply(&self, key: &str, html: &str) -> Result<()> {
        let now = crate::now();
        sqlx::query!(
            "INSERT OR IGNORE INTO outbox (dedup_key, kind, source, body, occurred_at, created_at) VALUES (?, ?, 'bot', ?, ?, ?)",
            key,
            KIND_REPLY,
            html,
            now,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn due(&self, now: i64) -> Result<Vec<OutboxRow>> {
        let rows = sqlx::query_as!(
            OutboxRow,
            r#"SELECT id, kind, source, body, occurred_at, created_at, attempts, reply_to
               FROM outbox WHERE status = 0 AND next_attempt_at <= ? ORDER BY id LIMIT 500"#,
            now
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn pending_count(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar!("SELECT COUNT(*) FROM outbox WHERE status = 0")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn mark_sent(&self, ids: &[i64], message_id: Option<i64>) -> Result<()> {
        let now = crate::now();
        let mut tx = self.pool.begin().await?;
        for id in ids {
            sqlx::query!(
                "UPDATE outbox SET status = 1, sent_at = ?, message_id = ? WHERE id = ?",
                now,
                message_id,
                id
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn mark_retry(&self, ids: &[i64], next_attempt_at: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for id in ids {
            sqlx::query!(
                "UPDATE outbox SET attempts = attempts + 1, next_attempt_at = ? WHERE id = ?",
                next_attempt_at,
                id
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn mark_dropped(&self, ids: &[i64]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for id in ids {
            sqlx::query!("UPDATE outbox SET status = 2 WHERE id = ?", id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Drops pending events created before `cutoff`, returns how many.
    pub async fn drop_expired(&self, cutoff: i64) -> Result<u64> {
        let r = sqlx::query!(
            "UPDATE outbox SET status = 2 WHERE status = 0 AND kind <> 2 AND created_at < ?",
            cutoff
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Status and Telegram message of a queued row.
    pub async fn outbox_state(&self, id: i64) -> Result<Option<(i64, Option<i64>)>> {
        let row = sqlx::query!("SELECT status, message_id FROM outbox WHERE id = ?", id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| (r.status, r.message_id)))
    }

    // GitHub

    pub async fn installations(&self) -> Result<Vec<(i64, String, bool)>> {
        let rows =
            sqlx::query!("SELECT id, account_login, suspended FROM installations ORDER BY id")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.id, r.account_login, r.suspended != 0))
            .collect())
    }

    pub async fn replace_installations(&self, list: &[(i64, String, bool)]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query!("DELETE FROM installations")
            .execute(&mut *tx)
            .await?;
        for (id, login, suspended) in list {
            let s = i64::from(*suspended);
            sqlx::query!(
                "INSERT INTO installations (id, account_login, suspended) VALUES (?, ?, ?)",
                id,
                login,
                s
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn repos(&self) -> Result<Vec<RepoRow>> {
        let rows = sqlx::query!(
            "SELECT id, full_name, private, installation_id FROM repos ORDER BY full_name"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| RepoRow {
                id: r.id,
                full_name: r.full_name,
                private: r.private != 0,
                installation_id: r.installation_id,
            })
            .collect())
    }

    /// Applies a repository list change and queues the given events in one transaction.
    pub async fn apply_repos(
        &self,
        upsert: &[RepoRow],
        remove: &[i64],
        events: &[Event],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for r in upsert {
            let p = i64::from(r.private);
            sqlx::query!(
                "INSERT INTO repos (id, full_name, private, installation_id) VALUES (?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET full_name = excluded.full_name, private = excluded.private, installation_id = excluded.installation_id",
                r.id,
                r.full_name,
                p,
                r.installation_id
            )
            .execute(&mut *tx)
            .await?;
        }
        for id in remove {
            sqlx::query!("DELETE FROM repos WHERE id = ?", id)
                .execute(&mut *tx)
                .await?;
        }
        for ev in events {
            insert_event(&mut tx, ev, 0).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Records a webhook delivery and its events atomically. Returns false for a repeat.
    pub async fn accept_delivery(&self, guid: &str, events: &[Event]) -> Result<bool> {
        let now = crate::now();
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query!(
            "INSERT OR IGNORE INTO deliveries (guid, received_at) VALUES (?, ?)",
            guid,
            now
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if inserted {
            for ev in events {
                insert_event(&mut tx, ev, 0).await?;
            }
        }
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn has_delivery(&self, guid: &str) -> Result<bool> {
        Ok(
            sqlx::query_scalar!("SELECT COUNT(*) FROM deliveries WHERE guid = ?", guid)
                .fetch_one(&self.pool)
                .await?
                > 0,
        )
    }

    pub async fn redelivery_requested(&self, guid: &str) -> Result<Option<(i64, i64)>> {
        let row = sqlx::query!(
            "SELECT requested_at, attempts FROM redeliveries WHERE guid = ?",
            guid
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.requested_at, r.attempts)))
    }

    pub async fn note_redelivery(&self, guid: &str) -> Result<()> {
        let now = crate::now();
        sqlx::query!(
            "INSERT INTO redeliveries (guid, requested_at, attempts) VALUES (?, ?, 1)
             ON CONFLICT(guid) DO UPDATE SET requested_at = excluded.requested_at, attempts = attempts + 1",
            guid,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // Steam guides

    pub async fn guides(&self) -> Result<Vec<GuideRow>> {
        let rows = sqlx::query!(
            "SELECT id, title, game, comment_count, missing_checks, last_full_read, baseline_done FROM guides ORDER BY id"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| GuideRow {
                id: r.id,
                title: r.title,
                game: r.game,
                comment_count: r.comment_count,
                missing_checks: r.missing_checks,
                last_full_read: r.last_full_read,
                baseline_done: r.baseline_done != 0,
            })
            .collect())
    }

    pub async fn upsert_guide(&self, g: &GuideRow) -> Result<()> {
        let b = i64::from(g.baseline_done);
        sqlx::query!(
            "INSERT INTO guides (id, title, game, comment_count, missing_checks, last_full_read, baseline_done)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET title = excluded.title, game = excluded.game,
               comment_count = excluded.comment_count, missing_checks = excluded.missing_checks,
               last_full_read = excluded.last_full_read, baseline_done = excluded.baseline_done",
            g.id,
            g.title,
            g.game,
            g.comment_count,
            g.missing_checks,
            g.last_full_read,
            b
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_guide(&self, id: i64, events: &[Event]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query!("DELETE FROM guides WHERE id = ?", id)
            .execute(&mut *tx)
            .await?;
        for ev in events {
            insert_event(&mut tx, ev, 0).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn guide_comments(&self, guide_id: i64) -> Result<Vec<CommentRow>> {
        let rows = sqlx::query_as!(
            CommentRow,
            "SELECT comment_id, author, author_url, author_steamid, posted_at, text, missing_reads
             FROM guide_comments WHERE guide_id = ?",
            guide_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Writes the new comment snapshot of a guide and its events in one transaction.
    pub async fn apply_comment_snapshot(
        &self,
        guide: &GuideRow,
        upsert: &[CommentRow],
        remove: &[String],
        events: &[Event],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let b = i64::from(guide.baseline_done);
        sqlx::query!(
            "INSERT INTO guides (id, title, game, comment_count, missing_checks, last_full_read, baseline_done)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET title = excluded.title, game = excluded.game,
               comment_count = excluded.comment_count, missing_checks = excluded.missing_checks,
               last_full_read = excluded.last_full_read, baseline_done = excluded.baseline_done",
            guide.id,
            guide.title,
            guide.game,
            guide.comment_count,
            guide.missing_checks,
            guide.last_full_read,
            b
        )
        .execute(&mut *tx)
        .await?;
        for c in upsert {
            sqlx::query!(
                "INSERT INTO guide_comments (guide_id, comment_id, author, author_url, author_steamid, posted_at, text, missing_reads)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(guide_id, comment_id) DO UPDATE SET author = excluded.author, author_url = excluded.author_url,
                   author_steamid = excluded.author_steamid, posted_at = excluded.posted_at, text = excluded.text,
                   missing_reads = excluded.missing_reads",
                guide.id,
                c.comment_id,
                c.author,
                c.author_url,
                c.author_steamid,
                c.posted_at,
                c.text,
                c.missing_reads
            )
            .execute(&mut *tx)
            .await?;
        }
        for id in remove {
            sqlx::query!(
                "DELETE FROM guide_comments WHERE guide_id = ? AND comment_id = ?",
                guide.id,
                id
            )
            .execute(&mut *tx)
            .await?;
        }
        for ev in events {
            insert_event(&mut tx, ev, 0).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // Steam games

    pub async fn game_state(&self, appid: i64) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar!("SELECT state FROM games WHERE appid = ?", appid)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn games(&self) -> Result<Vec<(i64, String, String)>> {
        let rows = sqlx::query!("SELECT appid, name, state FROM games ORDER BY appid")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.appid, r.name, r.state))
            .collect())
    }

    pub async fn save_game(
        &self,
        appid: i64,
        name: &str,
        state: &str,
        events: &[Event],
        merge_window_secs: i64,
    ) -> Result<()> {
        let now = crate::now();
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "INSERT INTO games (appid, name, state, updated_at) VALUES (?, ?, ?, ?)
             ON CONFLICT(appid) DO UPDATE SET name = excluded.name, state = excluded.state, updated_at = excluded.updated_at",
            appid,
            name,
            state,
            now
        )
        .execute(&mut *tx)
        .await?;
        for ev in events {
            insert_event(&mut tx, ev, merge_window_secs).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn forget_games_except(&self, keep: &[u32]) -> Result<()> {
        let known: Vec<i64> = sqlx::query_scalar!("SELECT appid FROM games")
            .fetch_all(&self.pool)
            .await?;
        for appid in known {
            if !keep.iter().any(|k| i64::from(*k) == appid) {
                sqlx::query!("DELETE FROM games WHERE appid = ?", appid)
                    .execute(&self.pool)
                    .await?;
                sqlx::query!("DELETE FROM news_seen WHERE appid = ?", appid)
                    .execute(&self.pool)
                    .await?;
                self.meta_del(&format!("news_baseline:{appid}")).await?;
            }
        }
        Ok(())
    }

    pub async fn news_seen(&self, appid: i64) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar!("SELECT gid FROM news_seen WHERE appid = ?", appid)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn save_news(
        &self,
        appid: i64,
        gids: &[String],
        events: &[Event],
        merge_window_secs: i64,
    ) -> Result<()> {
        let now = crate::now();
        let mut tx = self.pool.begin().await?;
        for gid in gids {
            sqlx::query!(
                "INSERT OR IGNORE INTO news_seen (appid, gid, seen_at) VALUES (?, ?, ?)",
                appid,
                gid,
                now
            )
            .execute(&mut *tx)
            .await?;
        }
        for ev in events {
            insert_event(&mut tx, ev, merge_window_secs).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // strangers

    pub async fn stranger(&self, user_id: i64) -> Result<Option<StrangerRow>> {
        Ok(sqlx::query_as!(
            StrangerRow,
            "SELECT user_id, username, attempts, last_at, last_reply_at FROM strangers WHERE user_id = ?",
            user_id
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn note_stranger(
        &self,
        user_id: i64,
        username: Option<&str>,
        replied: bool,
    ) -> Result<()> {
        let now = crate::now();
        let reply_at = replied.then_some(now);
        sqlx::query!(
            "INSERT INTO strangers (user_id, username, attempts, last_at, last_reply_at) VALUES (?, ?, 1, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET username = excluded.username, attempts = attempts + 1,
               last_at = excluded.last_at, last_reply_at = COALESCE(excluded.last_reply_at, last_reply_at)",
            user_id,
            username,
            now,
            reply_at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn strangers_since(&self, since: i64) -> Result<Vec<StrangerRow>> {
        Ok(sqlx::query_as!(
            StrangerRow,
            "SELECT user_id, username, attempts, last_at, last_reply_at FROM strangers WHERE last_at >= ? ORDER BY last_at DESC LIMIT 100",
            since
        )
        .fetch_all(&self.pool)
        .await?)
    }

    // maintenance

    pub async fn cleanup(&self, retention_days: u64, stranger_days: u64) -> Result<()> {
        let now = crate::now();
        let cutoff = now - i64::try_from(retention_days)? * 86_400;
        let stranger_cutoff = now - i64::try_from(stranger_days)? * 86_400;
        let redelivery_cutoff = now - 4 * 86_400;
        sqlx::query!("DELETE FROM deliveries WHERE received_at < ?", cutoff)
            .execute(&self.pool)
            .await?;
        sqlx::query!(
            "DELETE FROM redeliveries WHERE requested_at < ?",
            redelivery_cutoff
        )
        .execute(&self.pool)
        .await?;
        sqlx::query!(
            "UPDATE outbox SET reply_to = NULL WHERE reply_to IN (SELECT id FROM outbox WHERE status <> 0 AND created_at < ?)",
            cutoff
        )
        .execute(&self.pool)
        .await?;
        sqlx::query!(
            "DELETE FROM outbox WHERE status <> 0 AND created_at < ?",
            cutoff
        )
        .execute(&self.pool)
        .await?;
        sqlx::query!("DELETE FROM strangers WHERE last_at < ?", stranger_cutoff)
            .execute(&self.pool)
            .await?;
        sqlx::query!("DELETE FROM news_seen WHERE seen_at < ?", cutoff)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Consistent snapshot next to the database, keeps the newest `keep` copies.
    pub async fn backup(&self, keep: usize) -> Result<PathBuf> {
        let dir = self
            .path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let stem = self
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("hq-watchman");
        let stamp = jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S").to_string();
        let target = dir.join(format!("{stem}.backup-{stamp}.db"));
        create_private_file(&target)?;
        let target_str = target
            .to_str()
            .ok_or_else(|| anyhow!("backup path is not UTF-8"))?
            .to_owned();
        sqlx::query("VACUUM INTO ?")
            .bind(&target_str)
            .execute(&self.pool)
            .await
            .context("VACUUM INTO failed")?;
        let prefix = format!("{stem}.backup-");
        let mut old: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
                    && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("db"))
            })
            .collect();
        old.sort();
        if old.len() > keep {
            for p in old.iter().take(old.len() - keep) {
                std::fs::remove_file(p)
                    .with_context(|| format!("cannot remove {}", p.display()))?;
            }
        }
        Ok(target)
    }

    /// Free space check by a tiny write.
    pub async fn write_probe(&self) -> Result<()> {
        let now = crate::now().to_string();
        self.meta_set("last_alive", &now).await
    }
}

fn merge_kind(body: &Body) -> Option<&'static str> {
    match body {
        Body::GameBuild {
            branch,
            change,
            is_dlc: false,
            ..
        } if branch == "public" && *change == crate::event::BuildChange::Updated => Some("build"),
        Body::GameNews { patch: true, .. } => Some("patch"),
        _ => None,
    }
}

/// Inserts an event unless its dedup key is known. Links a build/patch pair for 5.4.
pub async fn insert_event(
    conn: &mut SqliteConnection,
    ev: &Event,
    merge_window_secs: i64,
) -> Result<Option<i64>> {
    let body = serde_json::to_string(&ev.body)?;
    let kind = if ev.is_service() {
        KIND_SERVICE
    } else {
        KIND_EVENT
    };
    let source = ev.source.as_str();
    let now = crate::now();
    let merge_app = ev.merge_app.map(i64::from);
    let mkind = ev.merge_app.and(merge_kind(&ev.body));
    let reply_to = match (merge_app, mkind) {
        (Some(app), Some(k)) if merge_window_secs > 0 => {
            let other = if k == "build" { "patch" } else { "build" };
            let since = now - merge_window_secs;
            sqlx::query_scalar!(
                r#"SELECT o.id FROM outbox o
                   WHERE o.merge_app = ? AND o.merge_kind = ? AND o.created_at >= ? AND o.reply_to IS NULL
                     AND NOT EXISTS (SELECT 1 FROM outbox r WHERE r.reply_to = o.id AND r.merge_kind = ?)
                   ORDER BY o.id DESC LIMIT 1"#,
                app,
                other,
                since,
                k
            )
            .fetch_optional(&mut *conn)
            .await?
        }
        _ => None,
    };
    let result = sqlx::query!(
        "INSERT OR IGNORE INTO outbox (dedup_key, kind, source, body, occurred_at, created_at, merge_app, merge_kind, reply_to)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        ev.dedup_key,
        kind,
        source,
        body,
        ev.occurred_at,
        now,
        merge_app,
        mkind,
        reply_to
    )
    .execute(&mut *conn)
    .await?;
    Ok((result.rows_affected() == 1).then(|| result.last_insert_rowid()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{BuildChange, Source};

    fn build_event(app: u32, key: &str) -> Event {
        let mut ev = Event::new(
            Source::SteamUpdate,
            key,
            1,
            Body::GameBuild {
                game: "G".into(),
                appid: app,
                is_dlc: false,
                change: BuildChange::Updated,
                branch: "public".into(),
                old_build: Some("1".into()),
                new_build: Some("2".into()),
                updated_at: None,
                store_url: String::new(),
            },
        );
        ev.merge_app = Some(app);
        ev
    }

    fn patch_event(app: u32, key: &str) -> Event {
        let mut ev = Event::new(
            Source::SteamUpdate,
            key,
            1,
            Body::GameNews {
                game: "G".into(),
                appid: app,
                patch: true,
                title: "t".into(),
                date: 1,
                text: String::new(),
                url: String::new(),
                store_url: String::new(),
            },
        );
        ev.merge_app = Some(app);
        ev
    }

    #[tokio::test]
    async fn dedup_by_key() {
        let db = Db::memory().await.unwrap();
        let ev = Event::service("k1", "t", "x");
        assert!(db.enqueue(&ev).await.unwrap().is_some());
        assert!(db.enqueue(&ev).await.unwrap().is_none());
        assert_eq!(db.pending_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn build_and_patch_are_linked() {
        let db = Db::memory().await.unwrap();
        db.enqueue_all(&[build_event(10, "b1")], 7200)
            .await
            .unwrap();
        db.enqueue_all(&[patch_event(10, "p1")], 7200)
            .await
            .unwrap();
        db.enqueue_all(&[patch_event(10, "p2")], 7200)
            .await
            .unwrap();
        db.enqueue_all(&[patch_event(11, "p3")], 7200)
            .await
            .unwrap();
        let rows = db.due(i64::MAX).await.unwrap();
        assert_eq!(rows[0].reply_to, None);
        assert_eq!(rows[1].reply_to, Some(rows[0].id));
        assert_eq!(rows[2].reply_to, None);
        assert_eq!(rows[3].reply_to, None);
    }

    #[tokio::test]
    async fn delivery_is_idempotent() {
        let db = Db::memory().await.unwrap();
        let ev = Event::service("d1", "t", "x");
        assert!(db
            .accept_delivery("g1", std::slice::from_ref(&ev))
            .await
            .unwrap());
        assert!(!db.accept_delivery("g1", &[ev]).await.unwrap());
        assert_eq!(db.pending_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn removed_game_forgets_news_baseline() {
        let db = Db::memory().await.unwrap();
        db.save_game(10, "G", "{}", &[], 0).await.unwrap();
        db.save_news(10, &["1".to_owned()], &[], 0).await.unwrap();
        db.meta_set("news_baseline:10", "1").await.unwrap();
        db.forget_games_except(&[11]).await.unwrap();
        assert!(db.game_state(10).await.unwrap().is_none());
        assert_eq!(db.news_seen(10).await.unwrap().len(), 0);
        assert!(db.meta_get("news_baseline:10").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn expired_are_dropped() {
        let db = Db::memory().await.unwrap();
        db.enqueue(&Event::service("e", "t", "x")).await.unwrap();
        assert_eq!(db.drop_expired(crate::now() + 10).await.unwrap(), 1);
        assert_eq!(db.pending_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn backup_keeps_newest() {
        let dir = std::env::temp_dir().join(format!("hqw-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = Db::open(&dir.join("t.db"), 2).await.unwrap();
        db.meta_set("a", "b").await.unwrap();
        for i in 0..3 {
            let p = dir.join(format!("t.backup-2000010{i}-000000.db"));
            std::fs::write(&p, b"").unwrap();
        }
        let made = db.backup(2).await.unwrap();
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".backup-"))
            .collect();
        assert_eq!(left.len(), 2);
        assert!(made.exists());
        db.pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
