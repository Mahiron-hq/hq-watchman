// SPDX-License-Identifier: GPL-3.0-or-later
//! User settings kept in the database: sources, event types, games, chats, language.
#![allow(clippy::struct_excessive_bools)]

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::event::{Body, Event, GhCommentKind, GhItemKind, Source};
use crate::i18n::Lang;

pub const MAX_GAMES: usize = 50;
pub const MAX_CHATS: usize = 20;
pub const MAX_EXCLUDED: usize = 1000;
const META_KEY: &str = "settings";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuietHours {
    pub enabled: bool,
    pub start: String,
    pub end: String,
}

impl Default for QuietHours {
    fn default() -> Self {
        Self {
            enabled: true,
            start: "22:00".into(),
            end: "08:00".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Github {
    pub enabled: bool,
    pub issues: bool,
    pub pull_requests: bool,
    pub discussions: bool,
    pub comments: bool,
    pub reviews: bool,
    pub review_comments: bool,
    pub commit_comments: bool,
    pub repositories: bool,
    pub edits: bool,
    pub deletes: bool,
    pub excluded_repos: Vec<String>,
}

impl Default for Github {
    fn default() -> Self {
        Self {
            enabled: true,
            issues: true,
            pull_requests: true,
            discussions: true,
            comments: true,
            reviews: true,
            review_comments: true,
            commit_comments: true,
            repositories: true,
            edits: true,
            deletes: true,
            excluded_repos: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Guides {
    pub enabled: bool,
    pub comments: bool,
    pub deleted_comments: bool,
    pub guide_changes: bool,
}

impl Default for Guides {
    fn default() -> Self {
        Self {
            enabled: true,
            comments: true,
            deleted_comments: true,
            guide_changes: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Game {
    pub appid: u32,
    pub name: String,
    pub builds: bool,
    pub branches: bool,
    pub dlc: bool,
    pub news: bool,
    pub patches: bool,
}

impl Default for Game {
    fn default() -> Self {
        Self {
            appid: 0,
            name: String::new(),
            builds: true,
            branches: true,
            dlc: true,
            news: true,
            patches: true,
        }
    }
}

impl Game {
    pub fn new(appid: u32, name: &str) -> Self {
        Self {
            appid,
            name: crate::text::clean_line(name, 100),
            ..Self::default()
        }
    }

    pub fn label(&self) -> String {
        if self.name.is_empty() {
            format!("App {}", self.appid)
        } else {
            self.name.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Games {
    pub enabled: bool,
    pub list: Vec<Game>,
}

impl Default for Games {
    fn default() -> Self {
        Self {
            enabled: true,
            list: Vec::new(),
        }
    }
}

/// Which sources a chat receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Delivery {
    pub github: bool,
    pub guides: bool,
    pub games: bool,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            github: true,
            guides: true,
            games: true,
        }
    }
}

impl Delivery {
    pub fn wants(self, source: Source) -> bool {
        match source {
            Source::Github => self.github,
            Source::SteamGuide => self.guides,
            Source::SteamUpdate => self.games,
            Source::Bot => false,
        }
    }
}

/// A group or channel the admin added the bot to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chat {
    pub id: i64,
    pub title: String,
    pub kind: String,
    #[serde(default)]
    pub delivery: Delivery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub language: Lang,
    pub timezone: String,
    pub quiet_hours: QuietHours,
    pub github: Github,
    pub guides: Guides,
    pub games: Games,
    pub admin: Delivery,
    pub chats: Vec<Chat>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: Lang::Ru,
            timezone: "UTC".into(),
            quiet_hours: QuietHours::default(),
            github: Github::default(),
            guides: Guides::default(),
            games: Games::default(),
            admin: Delivery::default(),
            chats: Vec::new(),
        }
    }
}

pub fn parse_hhmm(s: &str) -> Option<jiff::civil::Time> {
    let (h, m) = s.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    jiff::civil::Time::new(h.parse().ok()?, m.parse().ok()?, 0, 0).ok()
}

impl Settings {
    /// Initial settings from the keys an older config file may still carry.
    pub fn seed(cfg: &Config) -> Self {
        let mut s = Self {
            language: cfg.language.unwrap_or_default(),
            timezone: cfg
                .timezone_name
                .clone()
                .unwrap_or_else(|| "UTC".to_owned()),
            ..Self::default()
        };
        s.quiet_hours = QuietHours {
            enabled: cfg.quiet_hours.enabled,
            start: cfg.quiet_hours.start.strftime("%H:%M").to_string(),
            end: cfg.quiet_hours.end.strftime("%H:%M").to_string(),
        };
        s.github.edits = cfg.github.notify_edits;
        s.github.deletes = cfg.github.notify_deletes;
        s.games.list = cfg
            .games
            .appids
            .iter()
            .map(|id| Game::new(*id, ""))
            .collect();
        s
    }

    pub fn from_json(text: &str) -> Result<Self> {
        let s: Self = serde_json::from_str(text).map_err(|e| anyhow!("settings: {e}"))?;
        s.validate()?;
        Ok(s)
    }

    pub fn validate(&self) -> Result<()> {
        jiff::tz::TimeZone::get(&self.timezone)
            .map_err(|_| anyhow!("timezone: unknown IANA zone {:?}", self.timezone))?;
        if parse_hhmm(&self.quiet_hours.start).is_none()
            || parse_hhmm(&self.quiet_hours.end).is_none()
        {
            bail!("quiet_hours: times must be HH:MM");
        }
        if self.games.list.len() > MAX_GAMES {
            bail!("games: at most {MAX_GAMES} games");
        }
        let ids: BTreeSet<u32> = self.games.list.iter().map(|g| g.appid).collect();
        if ids.len() != self.games.list.len() || ids.contains(&0) {
            bail!("games: AppIDs must be unique and non-zero");
        }
        if self.games.list.iter().any(|g| g.name.chars().count() > 200) {
            bail!("games: name too long");
        }
        if self.github.excluded_repos.len() > MAX_EXCLUDED
            || !self
                .github
                .excluded_repos
                .iter()
                .all(|r| crate::github::valid_full_name(r))
        {
            bail!("github.excluded_repos: owner/name entries expected");
        }
        if self.chats.len() > MAX_CHATS {
            bail!("chats: at most {MAX_CHATS} chats");
        }
        let chat_ids: BTreeSet<i64> = self.chats.iter().map(|c| c.id).collect();
        if chat_ids.len() != self.chats.len() || self.chats.iter().any(|c| c.id >= 0) {
            bail!("chats: unique group or channel IDs expected");
        }
        if self
            .chats
            .iter()
            .any(|c| c.title.chars().count() > 200 || c.kind.len() > 20)
        {
            bail!("chats: title too long");
        }
        Ok(())
    }

    pub fn tz(&self) -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get(&self.timezone).unwrap_or(jiff::tz::TimeZone::UTC)
    }

    pub fn quiet(&self) -> crate::config::QuietHours {
        crate::config::QuietHours {
            enabled: self.quiet_hours.enabled,
            start: parse_hhmm(&self.quiet_hours.start).unwrap_or(jiff::civil::Time::midnight()),
            end: parse_hhmm(&self.quiet_hours.end).unwrap_or(jiff::civil::Time::midnight()),
        }
    }

    pub fn game(&self, appid: u32) -> Option<&Game> {
        self.games.list.iter().find(|g| g.appid == appid)
    }

    pub fn appids(&self) -> Vec<u32> {
        self.games.list.iter().map(|g| g.appid).collect()
    }

    pub fn source_on(&self, source: Source) -> bool {
        match source {
            Source::Github => self.github.enabled,
            Source::SteamGuide => self.guides.enabled,
            Source::SteamUpdate => self.games.enabled,
            Source::Bot => true,
        }
    }

    /// Whether the event type and object are switched on.
    pub fn wants(&self, ev: &Event) -> bool {
        if !self.source_on(ev.source) {
            return false;
        }
        let g = &self.github;
        let repo_ok = |repo: &str| {
            !g.excluded_repos
                .iter()
                .any(|r| r.eq_ignore_ascii_case(repo))
        };
        match &ev.body {
            Body::Service { .. } => true,
            Body::GithubComment {
                kind, change, repo, ..
            } => {
                let kind_on = match kind {
                    GhCommentKind::Issue
                    | GhCommentKind::PullRequest
                    | GhCommentKind::Discussion => g.comments,
                    GhCommentKind::Review => g.reviews,
                    GhCommentKind::ReviewLine => g.review_comments,
                    GhCommentKind::Commit => g.commit_comments,
                };
                let change_on = match change {
                    crate::event::Change::New => true,
                    crate::event::Change::Edited => g.edits,
                    crate::event::Change::Deleted => g.deletes,
                };
                kind_on && change_on && repo_ok(repo)
            }
            Body::GithubItem { kind, repo, .. } => {
                let on = match kind {
                    GhItemKind::Issue => g.issues,
                    GhItemKind::PullRequest => g.pull_requests,
                    GhItemKind::Discussion => g.discussions,
                };
                on && repo_ok(repo)
            }
            Body::RepoWatched { repo, .. } => g.repositories && repo_ok(repo),
            Body::GuideComment { deleted, .. } => {
                if *deleted {
                    self.guides.deleted_comments
                } else {
                    self.guides.comments
                }
            }
            Body::GuideWatched { .. } => self.guides.guide_changes,
            Body::GameBuild { appid, branch, .. } => self.game(*appid).is_some_and(|gm| {
                if branch == "public" || branch.starts_with("public ") {
                    gm.builds
                } else {
                    gm.branches
                }
            }),
            Body::Dlc { appid, .. } => self.game(*appid).is_some_and(|gm| gm.dlc),
            Body::GameNews { appid, patch, .. } => {
                self.game(*appid)
                    .is_some_and(|gm| if *patch { gm.patches } else { gm.news })
            }
        }
    }

    /// Chats an event goes to: `None` is the admin, `Some(id)` a group or channel.
    pub fn recipients(&self, ev: &Event) -> Vec<Option<i64>> {
        if ev.source == Source::Bot {
            return vec![None];
        }
        if !self.wants(ev) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.admin.wants(ev.source) {
            out.push(None);
        }
        out.extend(
            self.chats
                .iter()
                .filter(|c| c.delivery.wants(ev.source))
                .map(|c| Some(c.id)),
        );
        out
    }
}

static CURRENT: RwLock<Option<Arc<Settings>>> = RwLock::new(None);

pub fn current() -> Arc<Settings> {
    CURRENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_default()
}

/// Makes settings active for the whole process.
pub fn install(s: Settings) {
    crate::i18n::set(s.language);
    crate::set_timezone(s.tz());
    *CURRENT
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(s));
}

pub async fn load(db: &crate::storage::Db) -> Result<Option<(Settings, String)>> {
    let Some(text) = db.meta_get(META_KEY).await? else {
        return Ok(None);
    };
    let s = Settings::from_json(&text)?;
    Ok(Some((s, text)))
}

/// Stores validated settings and returns their JSON.
pub async fn save(db: &crate::storage::Db, s: &Settings) -> Result<String> {
    s.validate()?;
    let text = serde_json::to_string(s)?;
    db.meta_set(META_KEY, &text).await?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Change, Event};

    fn comment(repo: &str, change: Change) -> Event {
        Event::new(
            Source::Github,
            "k",
            1,
            Body::GithubComment {
                kind: GhCommentKind::Issue,
                change,
                repo: repo.into(),
                target: "t".into(),
                file: None,
                line: None,
                review_state: None,
                author: "a".into(),
                text: "x".into(),
                url: String::new(),
            },
        )
    }

    fn news(appid: u32, patch: bool) -> Event {
        Event::new(
            Source::SteamUpdate,
            "n",
            1,
            Body::GameNews {
                game: "G".into(),
                appid,
                patch,
                title: "t".into(),
                date: 1,
                text: String::new(),
                url: String::new(),
                store_url: String::new(),
            },
        )
    }

    #[test]
    fn filters_and_routes() {
        let mut s = Settings::default();
        s.github.excluded_repos = vec!["a/Secret".into()];
        s.github.edits = false;
        s.games.list = vec![Game {
            patches: false,
            ..Game::new(10, "G")
        }];
        s.chats.push(Chat {
            id: -100,
            title: "g".into(),
            kind: "supergroup".into(),
            delivery: Delivery {
                github: false,
                guides: false,
                games: true,
            },
        });
        assert_eq!(s.recipients(&comment("a/b", Change::New)), vec![None]);
        assert_eq!(
            s.recipients(&comment("a/secret", Change::New)),
            [] as [std::option::Option<i64>; 0]
        );
        assert_eq!(
            s.recipients(&comment("a/b", Change::Edited)),
            [] as [std::option::Option<i64>; 0]
        );
        assert_eq!(s.recipients(&news(10, false)), vec![None, Some(-100)]);
        assert_eq!(
            s.recipients(&news(10, true)),
            [] as [std::option::Option<i64>; 0]
        );
        assert_eq!(
            s.recipients(&news(11, false)),
            [] as [std::option::Option<i64>; 0]
        );
        s.games.enabled = false;
        assert_eq!(
            s.recipients(&news(10, false)),
            [] as [std::option::Option<i64>; 0]
        );
        let svc = Event::service("s", "t", "x");
        assert_eq!(s.recipients(&svc), vec![None]);
    }

    #[test]
    fn validation() {
        let mut s = Settings::default();
        assert!(s.validate().is_ok());
        s.timezone = "Mars/Base".into();
        assert!(s.validate().is_err());
        s.timezone = "Europe/Moscow".into();
        s.games.list = vec![Game::new(1, ""), Game::new(1, "")];
        assert!(s.validate().is_err());
        s.games.list.clear();
        s.quiet_hours.start = "25:00".into();
        assert!(s.validate().is_err());
        s.quiet_hours.start = "7:30".into();
        assert!(s.validate().is_ok());
        s.chats.push(Chat {
            id: 5,
            title: String::new(),
            kind: "group".into(),
            delivery: Delivery::default(),
        });
        assert!(s.validate().is_err());
        assert!(Settings::from_json(r#"{"language":"en","bogus":1}"#).is_err());
        let s = Settings::from_json(r#"{"language":"en"}"#).unwrap();
        assert_eq!(s.language, Lang::En);
        assert!(s.github.enabled);
    }
}
