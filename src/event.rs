// SPDX-License-Identifier: GPL-3.0-or-later
//! Normalized event shared by all sources and the sender.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Github,
    SteamGuide,
    SteamUpdate,
    Bot,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::SteamGuide => "steam_guide",
            Self::SteamUpdate => "steam_update",
            Self::Bot => "bot",
        }
    }

    pub fn hashtag(self) -> &'static str {
        match self {
            Self::Github => "#github",
            Self::SteamGuide => "#steam_guide",
            Self::SteamUpdate => "#steam_update",
            Self::Bot => "#bot",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Github => "GitHub",
            Self::SteamGuide => crate::tr!("Руководства Steam", "Steam guides"),
            Self::SteamUpdate => crate::tr!("Игры Steam", "Steam games"),
            Self::Bot => crate::tr!("Бот", "Bot"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    New,
    Edited,
    Deleted,
}

impl Change {
    pub fn word(self) -> &'static str {
        match self {
            Self::New => crate::tr!("новый", "new"),
            Self::Edited => crate::tr!("изменён", "edited"),
            Self::Deleted => crate::tr!("удалён", "deleted"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhCommentKind {
    Issue,
    PullRequest,
    ReviewLine,
    Review,
    Commit,
    Discussion,
}

impl GhCommentKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Issue => crate::tr!("Комментарий в Issue", "Issue comment"),
            Self::PullRequest => crate::tr!("Комментарий в Pull Request", "Pull request comment"),
            Self::ReviewLine => crate::tr!(
                "Комментарий к коду в Pull Request",
                "Pull request code comment"
            ),
            Self::Review => crate::tr!("Ревью Pull Request", "Pull request review"),
            Self::Commit => crate::tr!("Комментарий к коммиту", "Commit comment"),
            Self::Discussion => crate::tr!("Комментарий в обсуждении", "Discussion comment"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhItemKind {
    Issue,
    PullRequest,
    Discussion,
}

impl GhItemKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Issue => "Issue",
            Self::PullRequest => crate::tr!("Pull Request", "pull request"),
            Self::Discussion => crate::tr!("Обсуждение", "discussion"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildChange {
    Updated,
    BranchAdded,
    BranchRemoved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DlcChange {
    Added,
    Removed,
    ContentChanged,
}

/// What happened. All strings from outside are already cleaned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Body {
    GithubComment {
        kind: GhCommentKind,
        change: Change,
        repo: String,
        target: String,
        file: Option<String>,
        line: Option<u64>,
        review_state: Option<String>,
        author: String,
        text: String,
        url: String,
    },
    GithubItem {
        kind: GhItemKind,
        reopened: bool,
        repo: String,
        number: u64,
        title: String,
        author: String,
        text: String,
        url: String,
    },
    RepoWatched {
        repo: String,
        private: bool,
        removed: bool,
        url: String,
    },
    GuideWatched {
        title: String,
        game: String,
        gone: bool,
        url: String,
    },
    GuideComment {
        deleted: bool,
        guide: String,
        guide_url: String,
        author: String,
        author_url: String,
        posted_at: i64,
        text: String,
    },
    GameNews {
        game: String,
        appid: u32,
        patch: bool,
        title: String,
        date: i64,
        text: String,
        url: String,
        store_url: String,
    },
    GameBuild {
        game: String,
        appid: u32,
        is_dlc: bool,
        change: BuildChange,
        branch: String,
        old_build: Option<String>,
        new_build: Option<String>,
        updated_at: Option<i64>,
        store_url: String,
    },
    Dlc {
        game: String,
        #[serde(default)]
        appid: u32,
        dlc: String,
        dlc_appid: u32,
        change: DlcChange,
        store_url: String,
    },
    Service {
        title: String,
        text: String,
    },
}

/// Event ready to be queued.
#[derive(Debug, Clone)]
pub struct Event {
    pub source: Source,
    pub dedup_key: String,
    pub occurred_at: i64,
    pub body: Body,
    /// Game whose public build and patch notes are glued together (5.4).
    pub merge_app: Option<u32>,
}

impl Event {
    pub fn new(source: Source, dedup_key: impl Into<String>, occurred_at: i64, body: Body) -> Self {
        Self {
            source,
            dedup_key: dedup_key.into(),
            occurred_at,
            body,
            merge_app: None,
        }
    }

    pub fn service(
        dedup_key: impl Into<String>,
        title: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::new(
            Source::Bot,
            dedup_key,
            crate::now(),
            Body::Service {
                title: title.into(),
                text: text.into(),
            },
        )
    }

    pub fn is_service(&self) -> bool {
        matches!(self.body, Body::Service { .. })
    }
}

/// Grouping title used in digest files.
pub fn group_title(body: &Body) -> String {
    match body {
        Body::GithubComment { repo, .. }
        | Body::GithubItem { repo, .. }
        | Body::RepoWatched { repo, .. } => repo.clone(),
        Body::GuideWatched { title, .. } | Body::Service { title, .. } => title.clone(),
        Body::GuideComment { guide, .. } => guide.clone(),
        Body::GameNews { game, .. } | Body::GameBuild { game, .. } | Body::Dlc { game, .. } => {
            game.clone()
        }
    }
}

pub fn type_title(body: &Body) -> &'static str {
    match body {
        Body::GithubComment { kind, .. } => kind.title(),
        Body::GithubItem { kind, .. } => kind.title(),
        Body::RepoWatched { .. } => crate::tr!("Репозиторий", "Repository"),
        Body::GuideWatched { .. } => crate::tr!("Руководство", "Guide"),
        Body::GuideComment { .. } => crate::tr!("Комментарий к руководству", "Guide comment"),
        Body::GameNews { patch: true, .. } => crate::tr!("Патчноут", "Patch notes"),
        Body::GameNews { .. } => crate::tr!("Новость", "News"),
        Body::GameBuild { .. } => crate::tr!("Обновление билда", "Build update"),
        Body::Dlc { .. } => "DLC",
        Body::Service { .. } => crate::tr!("Служебное", "Service"),
    }
}
