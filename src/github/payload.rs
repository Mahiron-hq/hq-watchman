// SPDX-License-Identifier: GPL-3.0-or-later
//! Webhook payloads into normalized events.

use std::fmt::Write as _;

use serde::de::IgnoredAny;
use serde::Deserialize;

use super::valid_full_name;
use crate::config::GithubOptions;
use crate::event::{Body, Change, Event, GhCommentKind, GhItemKind, Source};
use crate::storage::RepoRow;
use crate::text;

const MAX_TITLE: usize = 300;

#[derive(Debug, Deserialize)]
struct User {
    id: u64,
    login: String,
    #[serde(rename = "type", default)]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct Repo {
    id: u64,
    full_name: String,
    #[serde(default)]
    private: bool,
    owner: User,
}

#[derive(Debug, Deserialize)]
struct Inst {
    id: u64,
    #[serde(default)]
    account: Option<User>,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    action: String,
    sender: Option<User>,
    installation: Option<Inst>,
    repository: Option<Repo>,
}

#[derive(Debug, Deserialize)]
struct Item {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<String>,
    user: Option<User>,
    #[serde(default)]
    pull_request: Option<IgnoredAny>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Comment {
    id: u64,
    #[serde(default)]
    body: Option<String>,
    user: Option<User>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    original_line: Option<u64>,
    #[serde(default)]
    commit_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Review {
    id: u64,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: String,
    user: Option<User>,
}

#[derive(Debug, Deserialize)]
struct ShortRepo {
    id: u64,
    full_name: String,
    #[serde(default)]
    private: bool,
}

#[derive(Debug, Deserialize)]
struct Full {
    issue: Option<Item>,
    pull_request: Option<Item>,
    discussion: Option<Item>,
    comment: Option<Comment>,
    review: Option<Review>,
    #[serde(default)]
    repositories_added: Vec<ShortRepo>,
    #[serde(default)]
    repositories_removed: Vec<ShortRepo>,
}

/// Facts the parser needs from the rest of the bot.
pub struct Context<'a> {
    pub admin_id: u64,
    pub options: &'a GithubOptions,
    pub known_installations: &'a [i64],
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub events: Vec<Event>,
    pub repos_added: Vec<RepoRow>,
    pub repos_removed: Vec<i64>,
    pub installation_changed: bool,
    pub unknown_installation: bool,
    pub ignored: Option<&'static str>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid payload: {0}")]
pub struct PayloadError(String);

fn login(u: Option<&User>) -> String {
    u.map_or_else(
        || crate::tr!("неизвестный", "unknown").to_owned(),
        |u| text::clean_line(&u.login, 60),
    )
}

fn body_text(b: Option<&String>) -> String {
    text::clean(b.map_or("", String::as_str))
}

fn short_hash(body: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, body.as_bytes());
    d.as_ref().iter().take(6).fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn is_bot(u: &User) -> bool {
    u.kind.eq_ignore_ascii_case("bot") || u.login.ends_with("[bot]")
}

fn comment_change(action: &str) -> Option<Change> {
    match action {
        "created" => Some(Change::New),
        "edited" => Some(Change::Edited),
        "deleted" => Some(Change::Deleted),
        _ => None,
    }
}

/// Parses one delivery. `event` is the `X-GitHub-Event` header.
pub fn parse(event: &str, body: &[u8], ctx: &Context<'_>) -> Result<Outcome, PayloadError> {
    let mut out = Outcome::default();
    if !matches!(
        event,
        "issues"
            | "issue_comment"
            | "pull_request"
            | "pull_request_review"
            | "pull_request_review_comment"
            | "commit_comment"
            | "discussion"
            | "discussion_comment"
            | "installation_repositories"
            | "installation"
    ) {
        out.ignored = Some("event type not tracked");
        return Ok(out);
    }
    let env: Envelope = serde_json::from_slice(body).map_err(|e| PayloadError(e.to_string()))?;
    let full: Full = serde_json::from_slice(body).map_err(|e| PayloadError(e.to_string()))?;
    let action = env.action.as_str();
    let inst = env
        .installation
        .as_ref()
        .ok_or_else(|| PayloadError("no installation".into()))?;
    let inst_id = i64::try_from(inst.id).map_err(|_| PayloadError("bad installation id".into()))?;

    if event == "installation" {
        out.installation_changed = true;
        let account = login(inst.account.as_ref());
        let (title, text) = match action {
            "created" => (crate::tr!("GitHub App установлено", "GitHub App installed"), crate::trf!("App установлено на аккаунт {account}. Репозитории подключатся автоматически.", "The App was installed on {account}. Repositories are added automatically.")),
            "deleted" => (crate::tr!("GitHub App удалено", "GitHub App removed"), crate::trf!("App удалено с аккаунта {account}. События GitHub больше не приходят. Установите App заново, если это ошибка.", "The App was removed from {account}. GitHub events no longer arrive. Install the App again if this was a mistake.")),
            "suspend" => (crate::tr!("GitHub App приостановлено", "GitHub App suspended"), crate::trf!("Установка на аккаунте {account} приостановлена. События GitHub не приходят, пока установку не возобновят.", "The installation on {account} is suspended. GitHub events do not arrive until it is resumed.")),
            "unsuspend" => (crate::tr!("GitHub App возобновлено", "GitHub App resumed"), crate::trf!("Установка на аккаунте {account} снова активна.", "The installation on {account} is active again.")),
            "new_permissions_accepted" => (crate::tr!("Права GitHub App изменены", "GitHub App permissions changed"), crate::trf!("На аккаунте {account} приняты новые права App. Проверьте, что права только на чтение.", "New App permissions were accepted on {account}. Check that they are read-only.")),
            _ => (crate::tr!("Изменение установки GitHub App", "GitHub App installation changed"), crate::trf!("Действие {} на аккаунте {account}.", "Action {} on {account}.", text::clean_line(action, 40))),
        };
        if inst.account.as_ref().is_some_and(|a| a.id != ctx.admin_id) {
            out.ignored = Some("installation of another account");
            return Ok(out);
        }
        out.events.push(Event::service(
            format!("gh:installation:{}:{action}:{}", inst.id, crate::now()),
            title,
            text,
        ));
        return Ok(out);
    }

    if !ctx.known_installations.contains(&inst_id) {
        out.unknown_installation = true;
        return Ok(out);
    }

    if event == "installation_repositories" {
        for r in full
            .repositories_added
            .iter()
            .filter(|r| valid_full_name(&r.full_name))
        {
            let Ok(id) = i64::try_from(r.id) else {
                continue;
            };
            out.repos_added.push(RepoRow {
                id,
                full_name: r.full_name.clone(),
                private: r.private,
                installation_id: inst_id,
            });
            out.events.push(Event::new(
                Source::Github,
                format!("gh:repo:add:{}:{}", r.id, crate::now()),
                crate::now(),
                Body::RepoWatched {
                    repo: r.full_name.clone(),
                    private: r.private,
                    removed: false,
                    url: format!("https://github.com/{}", r.full_name),
                },
            ));
        }
        for r in full
            .repositories_removed
            .iter()
            .filter(|r| valid_full_name(&r.full_name))
        {
            let Ok(id) = i64::try_from(r.id) else {
                continue;
            };
            out.repos_removed.push(id);
            out.events.push(Event::service(
                format!("gh:repo:rm:{}:{}", r.id, crate::now()),
                crate::tr!(
                    "Репозиторий снят с наблюдения",
                    "Repository no longer watched"
                ),
                crate::trf!(
                    "Репозиторий {} больше не отслеживается (удалён из установки App).",
                    "Repository {} is no longer watched (removed from the App installation).",
                    r.full_name
                ),
            ));
        }
        return Ok(out);
    }

    let repo = env
        .repository
        .as_ref()
        .ok_or_else(|| PayloadError("no repository".into()))?;
    if !valid_full_name(&repo.full_name) {
        return Err(PayloadError("bad repository name".into()));
    }
    if inst.account.as_ref().is_some_and(|a| a.id != repo.owner.id) {
        out.ignored = Some("repository owner does not match installation");
        return Ok(out);
    }
    let sender = env
        .sender
        .as_ref()
        .ok_or_else(|| PayloadError("no sender".into()))?;
    if sender.id == ctx.admin_id {
        out.ignored = Some("own action");
        return Ok(out);
    }
    if is_bot(sender)
        && !ctx
            .options
            .bot_allowlist
            .iter()
            .any(|b| b.eq_ignore_ascii_case(&sender.login))
    {
        out.ignored = Some("bot");
        return Ok(out);
    }
    let rname = repo.full_name.clone();
    let base = format!("https://github.com/{rname}");
    let now = crate::now();
    let _ = repo.private;

    let item_event = |kind: GhItemKind, it: &Item, path: &str, reopened: bool| {
        // Each reopen is its own event, so the key carries its time.
        let cycle = if reopened {
            format!(
                ":{}",
                it.updated_at
                    .as_deref()
                    .filter(|t| t.len() <= 32)
                    .map_or_else(|| now.to_string(), str::to_owned)
            )
        } else {
            String::new()
        };
        Event::new(
            Source::Github,
            format!("gh:{event}:{}:{}:{action}{cycle}", repo.id, it.number),
            now,
            Body::GithubItem {
                kind,
                reopened,
                repo: rname.clone(),
                number: it.number,
                title: text::clean_line(&it.title, MAX_TITLE),
                author: login(it.user.as_ref()),
                text: body_text(it.body.as_ref()),
                url: format!("{base}/{path}/{}", it.number),
            },
        )
    };

    match event {
        "issues" | "pull_request" | "discussion" => {
            let (it, kind, path) = match event {
                "issues" => (full.issue.as_ref(), GhItemKind::Issue, "issues"),
                "pull_request" => (full.pull_request.as_ref(), GhItemKind::PullRequest, "pull"),
                _ => (
                    full.discussion.as_ref(),
                    GhItemKind::Discussion,
                    "discussions",
                ),
            };
            let it = it.ok_or_else(|| PayloadError("no item".into()))?;
            let wanted = match event {
                "discussion" => action == "created",
                _ => action == "opened" || action == "reopened",
            };
            if wanted {
                out.events
                    .push(item_event(kind, it, path, action == "reopened"));
            } else {
                out.ignored = Some("action not tracked");
            }
        }
        "issue_comment"
        | "discussion_comment"
        | "pull_request_review_comment"
        | "commit_comment" => {
            let Some(change) = comment_change(action) else {
                out.ignored = Some("action not tracked");
                return Ok(out);
            };
            if (change == Change::Edited && !ctx.options.notify_edits)
                || (change == Change::Deleted && !ctx.options.notify_deletes)
                || (event == "commit_comment" && change != Change::New)
            {
                out.ignored = Some("edits or deletions disabled");
                return Ok(out);
            }
            let c = full
                .comment
                .as_ref()
                .ok_or_else(|| PayloadError("no comment".into()))?;
            let text_body = body_text(c.body.as_ref());
            let (kind, target, url, file, line) = match event {
                "issue_comment" => {
                    let it = full
                        .issue
                        .as_ref()
                        .ok_or_else(|| PayloadError("no issue".into()))?;
                    let kind = if it.pull_request.is_some() {
                        GhCommentKind::PullRequest
                    } else {
                        GhCommentKind::Issue
                    };
                    let path = if it.pull_request.is_some() {
                        "pull"
                    } else {
                        "issues"
                    };
                    (
                        kind,
                        format!("#{} {}", it.number, text::clean_line(&it.title, MAX_TITLE)),
                        format!("{base}/{path}/{}#issuecomment-{}", it.number, c.id),
                        None,
                        None,
                    )
                }
                "discussion_comment" => {
                    let it = full
                        .discussion
                        .as_ref()
                        .ok_or_else(|| PayloadError("no discussion".into()))?;
                    (
                        GhCommentKind::Discussion,
                        format!("#{} {}", it.number, text::clean_line(&it.title, MAX_TITLE)),
                        format!(
                            "{base}/discussions/{}#discussioncomment-{}",
                            it.number, c.id
                        ),
                        None,
                        None,
                    )
                }
                "pull_request_review_comment" => {
                    let it = full
                        .pull_request
                        .as_ref()
                        .ok_or_else(|| PayloadError("no pull request".into()))?;
                    (
                        GhCommentKind::ReviewLine,
                        format!("#{} {}", it.number, text::clean_line(&it.title, MAX_TITLE)),
                        format!("{base}/pull/{}#discussion_r{}", it.number, c.id),
                        c.path.as_deref().map(|p| text::clean_line(p, 300)),
                        c.line.or(c.original_line),
                    )
                }
                _ => {
                    let sha = c
                        .commit_id
                        .as_deref()
                        .filter(|s| {
                            s.len() >= 7
                                && s.len() <= 64
                                && s.bytes().all(|b| b.is_ascii_hexdigit())
                        })
                        .ok_or_else(|| PayloadError("bad commit id".into()))?;
                    (
                        GhCommentKind::Commit,
                        sha.get(..7).unwrap_or(sha).to_owned(),
                        format!("{base}/commit/{sha}#commitcomment-{}", c.id),
                        c.path
                            .as_deref()
                            .filter(|p| !p.is_empty())
                            .map(|p| text::clean_line(p, 300)),
                        c.line,
                    )
                }
            };
            let suffix = if change == Change::Edited {
                format!(":{}", short_hash(&text_body))
            } else {
                String::new()
            };
            out.events.push(Event::new(
                Source::Github,
                format!("gh:{event}:{}:{}:{action}{suffix}", repo.id, c.id),
                now,
                Body::GithubComment {
                    kind,
                    change,
                    repo: rname,
                    target,
                    file,
                    line,
                    review_state: None,
                    author: login(c.user.as_ref()),
                    text: text_body,
                    url,
                },
            ));
        }
        "pull_request_review" => {
            let change = match action {
                "submitted" => Change::New,
                "edited" | "dismissed" => Change::Edited,
                _ => {
                    out.ignored = Some("action not tracked");
                    return Ok(out);
                }
            };
            let r = full
                .review
                .as_ref()
                .ok_or_else(|| PayloadError("no review".into()))?;
            let it = full
                .pull_request
                .as_ref()
                .ok_or_else(|| PayloadError("no pull request".into()))?;
            let text_body = body_text(r.body.as_ref());
            let state = if action == "dismissed" {
                "dismissed"
            } else {
                r.state.as_str()
            };
            if action == "submitted"
                && state.eq_ignore_ascii_case("commented")
                && text_body.trim().is_empty()
            {
                out.ignored = Some("empty review wrapper of line comments");
                return Ok(out);
            }
            if change == Change::Edited && action == "edited" && !ctx.options.notify_edits {
                out.ignored = Some("edits disabled");
                return Ok(out);
            }
            let state_ru = match state.to_ascii_lowercase().as_str() {
                "approved" => crate::tr!("одобрено", "approved").to_owned(),
                "changes_requested" => {
                    crate::tr!("запрошены изменения", "changes requested").to_owned()
                }
                "commented" => crate::tr!("комментарий", "commented").to_owned(),
                "dismissed" => crate::tr!("ревью отклонено", "review dismissed").to_owned(),
                other => text::clean_line(other, 40),
            };
            let suffix = if change == Change::Edited {
                format!(":{}", short_hash(&format!("{state}{text_body}")))
            } else {
                String::new()
            };
            out.events.push(Event::new(
                Source::Github,
                format!("gh:review:{}:{}:{action}{suffix}", repo.id, r.id),
                now,
                Body::GithubComment {
                    kind: GhCommentKind::Review,
                    change,
                    repo: rname,
                    target: format!("#{} {}", it.number, text::clean_line(&it.title, MAX_TITLE)),
                    file: None,
                    line: None,
                    review_state: Some(state_ru),
                    author: login(r.user.as_ref()),
                    text: text_body,
                    url: format!("{base}/pull/{}#pullrequestreview-{}", it.number, r.id),
                },
            ));
        }
        _ => out.ignored = Some("event type not tracked"),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const ADMIN: u64 = 100;

    fn ctx(opts: &GithubOptions) -> Context<'_> {
        Context {
            admin_id: ADMIN,
            options: opts,
            known_installations: &[42],
        }
    }

    fn payload(extra: &str, sender_id: u64, sender_login: &str, sender_type: &str) -> String {
        format!(
            r#"{{"installation":{{"id":42,"account":{{"id":{ADMIN},"login":"admin"}}}},
                "repository":{{"id":7,"full_name":"admin/repo","private":true,"owner":{{"id":{ADMIN},"login":"admin"}}}},
                "sender":{{"id":{sender_id},"login":"{sender_login}","type":"{sender_type}"}},{extra}}}"#
        )
    }

    fn issue_comment(action: &str, sender: u64, login: &str, kind: &str) -> String {
        payload(
            &format!(
                r#""action":"{action}","issue":{{"number":3,"title":"Bug <b>","user":{{"id":5,"login":"u"}}}},
                "comment":{{"id":99,"body":"hello\u202e <a href=x>","user":{{"id":{sender},"login":"{login}","type":"{kind}"}}}}"#
            ),
            sender,
            login,
            kind,
        )
    }

    #[test]
    fn comment_from_other_user() {
        let o = GithubOptions::default();
        let out = parse(
            "issue_comment",
            issue_comment("created", 5, "someone", "User").as_bytes(),
            &ctx(&o),
        )
        .unwrap();
        assert_eq!(out.events.len(), 1);
        match &out.events[0].body {
            Body::GithubComment {
                kind,
                change,
                target,
                text,
                url,
                ..
            } => {
                assert_eq!(*kind, GhCommentKind::Issue);
                assert_eq!(*change, Change::New);
                assert_eq!(target, "#3 Bug <b>");
                assert_eq!(text, "hello <a href=x>");
                assert_eq!(
                    url,
                    "https://github.com/admin/repo/issues/3#issuecomment-99"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn each_reopen_has_its_own_key() {
        let o = GithubOptions::default();
        let reopen = |at: &str| {
            let body = payload(
                &format!(
                    r#""action":"reopened","issue":{{"number":3,"title":"Bug","user":{{"id":5,"login":"u"}},"updated_at":"{at}"}}"#
                ),
                5,
                "u",
                "User",
            );
            parse("issues", body.as_bytes(), &ctx(&o)).unwrap().events[0]
                .dedup_key
                .clone()
        };
        let a = reopen("2026-01-01T10:00:00Z");
        assert_eq!(a, reopen("2026-01-01T10:00:00Z"));
        assert_ne!(a, reopen("2026-01-02T10:00:00Z"));
    }

    #[test]
    fn own_and_bot_comments_ignored() {
        let o = GithubOptions::default();
        let own = parse(
            "issue_comment",
            issue_comment("created", ADMIN, "admin", "User").as_bytes(),
            &ctx(&o),
        )
        .unwrap();
        assert!(own.events.is_empty());
        let bot = parse(
            "issue_comment",
            issue_comment("created", 9, "dependabot[bot]", "Bot").as_bytes(),
            &ctx(&o),
        )
        .unwrap();
        assert!(bot.events.is_empty());
        let allowed = GithubOptions {
            bot_allowlist: vec!["dependabot[bot]".into()],
            ..GithubOptions::default()
        };
        let bot = parse(
            "issue_comment",
            issue_comment("created", 9, "dependabot[bot]", "Bot").as_bytes(),
            &ctx(&allowed),
        )
        .unwrap();
        assert_eq!(bot.events.len(), 1);
    }

    #[test]
    fn edits_have_distinct_keys_and_can_be_disabled() {
        let o = GithubOptions::default();
        let a = parse(
            "issue_comment",
            issue_comment("edited", 5, "x", "User").as_bytes(),
            &ctx(&o),
        )
        .unwrap();
        assert!(a.events[0].dedup_key.contains(":edited:"));
        let off = GithubOptions {
            notify_edits: false,
            ..GithubOptions::default()
        };
        let b = parse(
            "issue_comment",
            issue_comment("edited", 5, "x", "User").as_bytes(),
            &ctx(&off),
        )
        .unwrap();
        assert!(b.events.is_empty());
    }

    #[test]
    fn unknown_installation_flagged() {
        let o = GithubOptions::default();
        let c = Context {
            admin_id: ADMIN,
            options: &o,
            known_installations: &[],
        };
        let out = parse(
            "issue_comment",
            issue_comment("created", 5, "x", "User").as_bytes(),
            &c,
        )
        .unwrap();
        assert!(out.unknown_installation && out.events.is_empty());
    }

    #[test]
    fn review_line_comment_has_file_and_line() {
        let o = GithubOptions::default();
        let p = payload(
            r#""action":"created","pull_request":{"number":8,"title":"PR","user":{"id":5,"login":"u"}},
               "comment":{"id":11,"body":"nit","path":"src/main.rs","line":42,"user":{"id":5,"login":"u"}}"#,
            5,
            "u",
            "User",
        );
        let out = parse("pull_request_review_comment", p.as_bytes(), &ctx(&o)).unwrap();
        match &out.events[0].body {
            Body::GithubComment {
                file, line, url, ..
            } => {
                assert_eq!(file.as_deref(), Some("src/main.rs"));
                assert_eq!(*line, Some(42));
                assert!(url.ends_with("/pull/8#discussion_r11"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn repos_added_and_untracked_events() {
        let o = GithubOptions::default();
        let p = payload(
            r#""action":"added","repositories_added":[{"id":12,"full_name":"admin/new","private":false}],"repositories_removed":[]"#,
            ADMIN,
            "admin",
            "User",
        );
        let out = parse("installation_repositories", p.as_bytes(), &ctx(&o)).unwrap();
        assert_eq!(out.repos_added.len(), 1);
        assert_eq!(out.events.len(), 1);
        let out = parse("push", b"{}", &ctx(&o)).unwrap();
        assert!(out.ignored.is_some());
    }

    #[test]
    fn foreign_repository_ignored() {
        let o = GithubOptions::default();
        let p = issue_comment("created", 5, "x", "User")
            .replace(r#""owner":{"id":100"#, r#""owner":{"id":555"#);
        let out = parse("issue_comment", p.as_bytes(), &ctx(&o)).unwrap();
        assert!(out.events.is_empty());
    }

    proptest! {
        #[test]
        fn never_panics(event in "[a-z_]{1,30}", body in any::<Vec<u8>>()) {
            let o = GithubOptions::default();
            let _ = parse(&event, &body, &ctx(&o));
        }
    }
}
