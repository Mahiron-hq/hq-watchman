// SPDX-License-Identifier: GPL-3.0-or-later
//! Steam guides of one profile and full comment snapshots.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::anyhow;
use serde::Deserialize;

use super::comments::{self, ParseError, ParsedComment};
use super::{SteamError, SteamHttp};
use crate::event::{Body, Event, Source};
use crate::health::Health;
use crate::storage::{CommentRow, Db, GuideRow};
use crate::text;

const PAGE: usize = 50;
const MAX_PAGES: usize = 200;
const FULL_READ_EVERY: i64 = 3600;

pub fn guide_url(id: i64) -> String {
    format!("https://steamcommunity.com/sharedfiles/filedetails/?id={id}")
}

pub fn profile_url(steamid: i64) -> String {
    format!("https://steamcommunity.com/profiles/{steamid}")
}

#[derive(Debug, Clone)]
pub struct ListedGuide {
    pub id: i64,
    pub title: String,
    pub game: String,
    pub comments: Option<i64>,
}

#[derive(Deserialize)]
struct UserFiles {
    response: UserFilesBody,
}

#[derive(Deserialize)]
struct UserFilesBody {
    #[serde(default)]
    total: usize,
    #[serde(default)]
    publishedfiledetails: Vec<FileDetails>,
}

#[derive(Deserialize)]
struct FileDetails {
    publishedfileid: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    app_name: String,
    #[serde(default)]
    num_comments_public: Option<i64>,
    #[serde(default)]
    visibility: i64,
    #[serde(default)]
    creator: String,
}

#[derive(Deserialize)]
struct RenderResponse {
    success: bool,
    #[serde(default)]
    total_count: Option<usize>,
    #[serde(default)]
    comments_html: String,
}

#[derive(Debug)]
enum ReadError {
    Steam(SteamError),
    Parser(String),
}

impl From<SteamError> for ReadError {
    fn from(e: SteamError) -> Self {
        Self::Steam(e)
    }
}

impl From<ParseError> for ReadError {
    fn from(e: ParseError) -> Self {
        Self::Parser(e.to_string())
    }
}

/// Result of one diff between a stored snapshot and a full read.
#[derive(Debug, Default)]
pub struct SnapshotDiff {
    pub upsert: Vec<CommentRow>,
    pub remove: Vec<String>,
    pub events: Vec<Event>,
}

/// Compares the stored snapshot with a complete read.
pub fn diff_comments(
    guide: &GuideRow,
    stored: &[CommentRow],
    read: &[ParsedComment],
    admin: u64,
    notify: bool,
) -> SnapshotDiff {
    let mut d = SnapshotDiff::default();
    let fresh: BTreeMap<&str, &ParsedComment> = read.iter().map(|c| (c.id.as_str(), c)).collect();
    let old: BTreeMap<&str, &CommentRow> =
        stored.iter().map(|c| (c.comment_id.as_str(), c)).collect();
    let url = guide_url(guide.id);
    for c in read {
        let steamid = i64::try_from(c.author_steamid).unwrap_or_default();
        let row = CommentRow {
            comment_id: c.id.clone(),
            author: c.author.clone(),
            author_url: profile_url(steamid),
            author_steamid: steamid,
            posted_at: c.posted_at,
            text: c.text.clone(),
            missing_reads: 0,
        };
        let is_new = !old.contains_key(c.id.as_str());
        let changed = old
            .get(c.id.as_str())
            .is_none_or(|o| o.missing_reads != 0 || o.text != row.text || o.author != row.author);
        if is_new && notify && c.author_steamid != admin {
            d.events.push(Event::new(
                Source::SteamGuide,
                format!("steam:gcomment:new:{}:{}", guide.id, c.id),
                c.posted_at,
                Body::GuideComment {
                    deleted: false,
                    guide: guide.title.clone(),
                    guide_url: url.clone(),
                    author: row.author.clone(),
                    author_url: row.author_url.clone(),
                    posted_at: c.posted_at,
                    text: row.text.clone(),
                },
            ));
        }
        if changed {
            d.upsert.push(row);
        }
    }
    for o in stored {
        if fresh.contains_key(o.comment_id.as_str()) {
            continue;
        }
        if o.missing_reads + 1 >= 2 {
            d.remove.push(o.comment_id.clone());
            if notify && u64::try_from(o.author_steamid).ok() != Some(admin) {
                d.events.push(Event::new(
                    Source::SteamGuide,
                    format!("steam:gcomment:del:{}:{}", guide.id, o.comment_id),
                    crate::now(),
                    Body::GuideComment {
                        deleted: true,
                        guide: guide.title.clone(),
                        guide_url: url.clone(),
                        author: o.author.clone(),
                        author_url: o.author_url.clone(),
                        posted_at: o.posted_at,
                        text: o.text.clone(),
                    },
                ));
            }
        } else {
            let mut row = o.clone();
            row.missing_reads += 1;
            d.upsert.push(row);
        }
    }
    d
}

pub struct GuidesPoller {
    pub profile: u64,
    pub steam: std::sync::Arc<SteamHttp>,
}

impl GuidesPoller {
    async fn list_api(&self, steam: &SteamHttp) -> Result<(Vec<ListedGuide>, bool), SteamError> {
        let mut out = Vec::new();
        for page in 1..=20u32 {
            let resp: UserFiles = steam
                .api(
                    "IPublishedFileService/GetUserFiles/v1/",
                    &[
                        ("steamid", self.profile.to_string()),
                        ("appid", "0".into()),
                        ("filetype", "10".into()),
                        ("numperpage", "100".into()),
                        ("page", page.to_string()),
                        ("return_short_description", "false".into()),
                    ],
                    true,
                )
                .await?;
            let count = resp.response.publishedfiledetails.len();
            for f in resp.response.publishedfiledetails {
                let Ok(id) = f.publishedfileid.parse::<i64>() else {
                    continue;
                };
                if f.visibility != 0
                    || (!f.creator.is_empty() && f.creator != self.profile.to_string())
                {
                    continue;
                }
                out.push(ListedGuide {
                    id,
                    title: text::clean_line(&f.title, 300),
                    game: text::clean_line(&f.app_name, 200),
                    comments: f.num_comments_public,
                });
            }
            if count == 0 || out.len() >= resp.response.total {
                return Ok((out, true));
            }
        }
        Ok((out, false))
    }

    async fn list_html(&self, steam: &SteamHttp) -> Result<(Vec<ListedGuide>, bool), ReadError> {
        let mut out = Vec::new();
        for page in 1..=50u32 {
            let url = format!(
                "https://steamcommunity.com/profiles/{}/myworkshopfiles/?section=guides&numperpage=30&l=english&p={page}",
                self.profile
            );
            let body = steam.community_get(&url, comments::MAX_HTML).await?;
            let html = String::from_utf8_lossy(&body);
            let (items, total) = comments::parse_guides_page(&html)?;
            let n = items.len();
            out.extend(items.into_iter().filter_map(|g| {
                Some(ListedGuide {
                    id: i64::try_from(g.id).ok()?,
                    title: g.title,
                    game: g.game,
                    comments: None,
                })
            }));
            if n == 0 || total.is_none_or(|t| out.len() >= t) {
                return Ok((out, true));
            }
        }
        Ok((out, false))
    }

    async fn read_comments(
        &self,
        steam: &SteamHttp,
        guide: i64,
    ) -> Result<Vec<ParsedComment>, ReadError> {
        let url = format!(
            "https://steamcommunity.com/comment/PublishedFile_Public/render/{}/{guide}/",
            self.profile
        );
        let mut all: Vec<ParsedComment> = Vec::new();
        let mut total = None;
        for page in 0..MAX_PAGES {
            let start = page * PAGE;
            let body = steam
                .community_post(
                    &url,
                    &[("start", start.to_string()), ("count", PAGE.to_string())],
                    comments::MAX_HTML,
                )
                .await?;
            let resp: RenderResponse = serde_json::from_slice(&body).map_err(|_| {
                ReadError::Steam(SteamError::Other(anyhow!(
                    "comment page is not JSON (captcha or error page)"
                )))
            })?;
            if !resp.success {
                return Err(ReadError::Steam(SteamError::Other(anyhow!(
                    "Steam refused the comment page"
                ))));
            }
            let t = resp
                .total_count
                .ok_or_else(|| ReadError::Parser("total_count missing".into()))?;
            total.get_or_insert(t);
            let items = comments::parse_comments(&resp.comments_html)?;
            if items.is_empty() {
                break;
            }
            all.extend(items);
            if all.len() >= t {
                break;
            }
        }
        let total = total.unwrap_or(0);
        let unique: BTreeSet<&str> = all.iter().map(|c| c.id.as_str()).collect();
        if unique.len() != total || all.len() != total {
            return Err(ReadError::Parser(format!(
                "read {} comments, page says {total}",
                unique.len()
            )));
        }
        Ok(all)
    }

    /// One check of all guides.
    pub async fn poll(&self, db: &Db, health: &Health) -> anyhow::Result<()> {
        let listed = match self.list_api_or_html(health).await {
            Ok(l) => l,
            Err(ReadError::Parser(e)) => {
                health
                    .fail(
                        "guides_parser",
                        &e,
                        3,
                        crate::tr!(
                            "вёрстка Steam изменилась, нужен новый парсер",
                            "Steam page layout changed, the parser needs an update"
                        ),
                    )
                    .await;
                return Ok(());
            }
            Err(ReadError::Steam(e)) => {
                health
                    .fail(
                        "guides",
                        &e.to_string(),
                        3,
                        crate::tr!(
                            "проверьте доступность Steam; бот повторит сам",
                            "check that Steam is reachable; the bot retries by itself"
                        ),
                    )
                    .await;
                return Ok(());
            }
        };
        let first_run = db.meta_get("guides_baseline").await?.is_none();
        let stored: BTreeMap<i64, GuideRow> =
            db.guides().await?.into_iter().map(|g| (g.id, g)).collect();
        let (listed, complete) = listed;
        if !complete {
            tracing::warn!(
                listed = listed.len(),
                "guide list is longer than the page limit, missing guides are not counted"
            );
        }
        let listed_ids: BTreeSet<i64> = listed.iter().map(|g| g.id).collect();
        for (id, g) in &stored {
            if listed_ids.contains(id) || !complete {
                continue;
            }
            let mut g = g.clone();
            g.missing_checks += 1;
            if g.missing_checks >= 2 {
                let ev = Event::new(
                    Source::SteamGuide,
                    format!("steam:guide:gone:{id}:{}", crate::now()),
                    crate::now(),
                    Body::GuideWatched {
                        title: g.title.clone(),
                        game: g.game.clone(),
                        gone: true,
                        url: guide_url(*id),
                    },
                );
                db.delete_guide(*id, &[ev]).await?;
            } else {
                db.upsert_guide(&g).await?;
            }
        }
        let mut network_error = None;
        let mut parser_error = None;
        let now = crate::now();
        for l in &listed {
            let mut g = stored.get(&l.id).cloned().unwrap_or(GuideRow {
                id: l.id,
                title: l.title.clone(),
                game: l.game.clone(),
                comment_count: -1,
                missing_checks: 0,
                last_full_read: None,
                baseline_done: false,
            });
            let is_new = !stored.contains_key(&l.id);
            g.title.clone_from(&l.title);
            if !l.game.is_empty() {
                g.game.clone_from(&l.game);
            }
            g.missing_checks = 0;
            let snapshot = db.guide_comments(g.id).await?;
            let pending_delete = snapshot.iter().any(|c| c.missing_reads > 0);
            let need_read = !g.baseline_done
                || l.comments.is_none_or(|c| c != g.comment_count)
                || g.last_full_read.is_none_or(|t| now - t >= FULL_READ_EVERY)
                || pending_delete;
            let mut events = Vec::new();
            if is_new && !first_run {
                events.push(Event::new(
                    Source::SteamGuide,
                    format!("steam:guide:new:{}", g.id),
                    now,
                    Body::GuideWatched {
                        title: g.title.clone(),
                        game: g.game.clone(),
                        gone: false,
                        url: guide_url(g.id),
                    },
                ));
            }
            if !need_read {
                db.upsert_guide(&g).await?;
                continue;
            }
            match self.read_comments(&self.steam, g.id).await {
                Ok(read) => {
                    let notify = g.baseline_done;
                    let d = diff_comments(&g, &snapshot, &read, self.profile, notify);
                    events.extend(d.events);
                    g.comment_count = l.comments.unwrap_or(i64::try_from(read.len()).unwrap_or(0));
                    g.last_full_read = Some(now);
                    g.baseline_done = true;
                    db.apply_comment_snapshot(&g, &d.upsert, &d.remove, &events)
                        .await?;
                }
                Err(e) => {
                    db.apply_comment_snapshot(&g, &[], &[], &events).await?;
                    match e {
                        ReadError::Parser(m) => parser_error = Some(format!("«{}»: {m}", g.title)),
                        ReadError::Steam(s) => network_error = Some(s.to_string()),
                    }
                }
            }
        }
        if first_run {
            db.meta_set("guides_baseline", "1").await?;
            let ev = Event::service(
                format!("bot:guides_baseline:{now}"),
                crate::tr!("Руководства взяты под наблюдение", "Guides are now watched"),
                crate::trf!(
                    "Руководств: {}. Существующие комментарии сохранены без уведомлений, дальше приходят только новые и удалённые.",
                    "Guides: {}. Existing comments were saved silently, only new and deleted ones are reported from now on.",
                    listed.len()
                ),
            );
            db.enqueue(&ev).await?;
        }
        match parser_error {
            Some(e) => {
                health
                    .fail(
                        "guides_parser",
                        &e,
                        3,
                        crate::tr!(
                            "вёрстка Steam могла измениться, проверьте страницу руководства",
                            "Steam page layout may have changed, check the guide page"
                        ),
                    )
                    .await;
            }
            None => health.ok("guides_parser").await,
        }
        match network_error {
            Some(e) => {
                health
                    .fail(
                        "guides",
                        &e,
                        3,
                        crate::tr!(
                            "проверьте доступность steamcommunity.com; бот повторит сам",
                            "check that steamcommunity.com is reachable; the bot retries by itself"
                        ),
                    )
                    .await;
            }
            None => health.ok("guides").await,
        }
        Ok(())
    }

    /// Guides of the profile and whether the list is complete.
    async fn list_api_or_html(
        &self,
        health: &Health,
    ) -> Result<(Vec<ListedGuide>, bool), ReadError> {
        let steam = &self.steam;
        if !steam.has_key() {
            return self.list_html(steam).await;
        }
        match self.list_api(steam).await {
            Ok(l) => Ok(l),
            Err(SteamError::BadKey) => {
                health
                    .fail(
                        "guides",
                        crate::tr!("ключ Steam Web API отклонён", "Steam Web API key rejected"),
                        1,
                        crate::tr!("проверьте HQW_STEAM_API_KEY; пока список берётся со страницы профиля", "check HQW_STEAM_API_KEY; meanwhile the list is read from the profile page"),
                    )
                    .await;
                self.list_html(steam).await
            }
            Err(e) => {
                tracing::warn!(error = %e, "guide list API failed, using profile page");
                self.list_html(steam).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guide() -> GuideRow {
        GuideRow {
            id: 5,
            title: "G".into(),
            game: "Game".into(),
            comment_count: 0,
            missing_checks: 0,
            last_full_read: None,
            baseline_done: true,
        }
    }

    fn parsed(id: &str, author: u64) -> ParsedComment {
        ParsedComment {
            id: id.into(),
            author: format!("a{id}"),
            author_steamid: author,
            posted_at: 10,
            text: format!("text {id}"),
        }
    }

    fn stored(id: &str, missing: i64) -> CommentRow {
        CommentRow {
            comment_id: id.into(),
            author: format!("a{id}"),
            author_url: profile_url(7),
            author_steamid: 7,
            posted_at: 10,
            text: format!("text {id}"),
            missing_reads: missing,
        }
    }

    #[test]
    fn new_comment_notifies_except_admin() {
        let d = diff_comments(&guide(), &[], &[parsed("1", 7), parsed("2", 99)], 99, true);
        assert_eq!(d.events.len(), 1);
        assert_eq!(d.upsert.len(), 2);
    }

    #[test]
    fn baseline_is_silent() {
        let d = diff_comments(&guide(), &[], &[parsed("1", 7)], 99, false);
        assert!(d.events.is_empty());
        assert_eq!(d.upsert.len(), 1);
    }

    #[test]
    fn deletion_needs_two_reads() {
        let first = diff_comments(&guide(), &[stored("1", 0)], &[], 99, true);
        assert!(first.events.is_empty());
        assert_eq!(first.remove.len(), 0);
        assert_eq!(first.upsert[0].missing_reads, 1);
        let second = diff_comments(&guide(), &[stored("1", 1)], &[], 99, true);
        assert_eq!(second.remove, vec!["1".to_owned()]);
        match &second.events[0].body {
            Body::GuideComment { deleted, text, .. } => {
                assert!(deleted);
                assert_eq!(text, "text 1");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn reappearing_comment_resets() {
        let d = diff_comments(&guide(), &[stored("1", 1)], &[parsed("1", 7)], 99, true);
        assert!(d.events.is_empty());
        assert_eq!(d.upsert[0].missing_reads, 0);
    }

    #[test]
    fn unchanged_snapshot_writes_nothing() {
        let mut p = parsed("1", 7);
        p.author = "a1".into();
        let d = diff_comments(&guide(), &[stored("1", 0)], &[p], 99, true);
        assert!(d.upsert.is_empty() && d.events.is_empty() && d.remove.is_empty());
    }
}
