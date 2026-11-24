// SPDX-License-Identifier: GPL-3.0-or-later
//! Telegram HTML and Markdown views of events. Markup comes only from these templates.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::event::{group_title, type_title, Body, BuildChange, Change, DlcChange, Source};
use crate::text::{self, escape_html, external_html, md_block, md_inline};

pub const MAX_MESSAGE: usize = 4096;
pub const MAX_CAPTION: usize = 1024;
const PREVIEW_CHARS: usize = 700;

const LINK_PREFIXES: &[&str] = &[
    "https://github.com/",
    "https://store.steampowered.com/",
    "https://steamcommunity.com/",
];

/// Only links the bot built itself, to GitHub and Steam, over https.
pub fn safe_link(url: &str) -> Option<&str> {
    let ok = LINK_PREFIXES.iter().any(|p| url.starts_with(p))
        && url.len() <= 512
        && url
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'<' && b != b'>' && b != b'\'');
    ok.then_some(url)
}

fn link(url: &str, label: &str) -> String {
    safe_link(url).map_or_else(String::new, |u| {
        format!("<a href=\"{}\">{}</a>", escape_html(u), escape_html(label))
    })
}

/// One notification split into the parts that may need shortening.
pub struct View {
    pub head: String,
    pub text: Option<String>,
    pub tail: String,
}

impl View {
    pub fn html(&self) -> String {
        let mut s = self.head.clone();
        if let Some(t) = &self.text {
            if t.trim().is_empty() {
                s.push_str(crate::tr!("\n<i>(без текста)</i>", "\n<i>(no text)</i>"));
            } else {
                let _ = write!(
                    s,
                    "\n<blockquote expandable>{}</blockquote>",
                    external_html(t)
                );
            }
        }
        s.push('\n');
        s.push_str(&self.tail);
        s
    }

    /// Short form with the first lines of the text, for a message with an attached file.
    pub fn summary_html(&self, limit: usize) -> String {
        let mut s = self.head.clone();
        if let Some(t) = &self.text {
            let room = limit
                .saturating_sub(visible_len(&self.head) + visible_len(&self.tail) + 80)
                .min(PREVIEW_CHARS);
            let preview = text::truncate_chars(t, room);
            let _ = write!(
                s,
                "\n<blockquote>{}</blockquote>\n<i>{}</i>",
                external_html(&preview),
                crate::tr!(
                    "Полный текст в приложенном файле.",
                    "Full text in the attached file."
                )
            );
        }
        s.push('\n');
        s.push_str(&self.tail);
        s
    }
}

/// Length of the visible text of our own HTML (tags removed, entities counted once).
pub fn visible_len(html: &str) -> usize {
    let mut n = 0usize;
    let mut in_tag = false;
    let mut in_entity = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            '&' => {
                in_entity = true;
                n += 1;
            }
            ';' if in_entity => in_entity = false,
            _ if in_entity => {}
            c => n += c.len_utf16(),
        }
    }
    n
}

fn field(out: &mut String, label: &str, value: &str) {
    let _ = write!(out, "\n{label}: {}", external_html(value));
}

fn code_field(out: &mut String, label: &str, value: &str) {
    let _ = write!(out, "\n{label}: <code>{}</code>", escape_html(value));
}

fn headline(source: Source, title: &str) -> String {
    let prefix = match source {
        Source::Github => "GitHub",
        Source::SteamGuide | Source::SteamUpdate => "Steam",
        Source::Bot => crate::tr!("Бот", "Bot"),
    };
    format!("<b>{prefix} · {}</b>", escape_html(title))
}

pub fn view(source: Source, body: &Body) -> View {
    let tag = source.hashtag();
    match body {
        Body::GithubComment {
            kind,
            change,
            repo,
            target,
            file,
            line,
            review_state,
            author,
            text,
            url,
        } => {
            let mut head = headline(source, &format!("{} · {}", kind.title(), change.word()));
            code_field(&mut head, crate::tr!("Репозиторий", "Repository"), repo);
            field(
                &mut head,
                if *kind == crate::event::GhCommentKind::Commit {
                    crate::tr!("Коммит", "Commit")
                } else {
                    crate::tr!("Тема", "Topic")
                },
                target,
            );
            if let Some(f) = file {
                let place = line.map_or_else(
                    || f.clone(),
                    |l| crate::trf!("{f}, строка {l}", "{f}, line {l}"),
                );
                code_field(&mut head, crate::tr!("Файл", "File"), &place);
            }
            if let Some(s) = review_state {
                field(&mut head, crate::tr!("Вердикт", "Verdict"), s);
            }
            field(&mut head, crate::tr!("Автор", "Author"), author);
            let label = if *change == Change::Deleted {
                crate::tr!("Удалённый текст:", "Deleted text:")
            } else {
                crate::tr!("Текст:", "Text:")
            };
            let _ = write!(head, "\n{label}");
            View {
                head,
                text: Some(text.clone()),
                tail: format!(
                    "{}\n{tag}",
                    link(url, crate::tr!("Открыть на GitHub", "Open on GitHub"))
                ),
            }
        }
        Body::GithubItem {
            kind,
            reopened,
            repo,
            number,
            title,
            author,
            text,
            url,
        } => {
            let what = if *reopened {
                crate::trf!("{} открыт заново", "{} reopened", kind.title())
            } else {
                crate::trf!("Новый {}", "New {}", kind.title())
            };
            let what = if *kind == crate::event::GhItemKind::Discussion {
                crate::tr!("Новое обсуждение", "New discussion").to_owned()
            } else {
                what
            };
            let mut head = headline(source, &what);
            code_field(&mut head, crate::tr!("Репозиторий", "Repository"), repo);
            field(
                &mut head,
                crate::tr!("Номер", "Number"),
                &format!("#{number}"),
            );
            field(&mut head, crate::tr!("Заголовок", "Title"), title);
            field(&mut head, crate::tr!("Автор", "Author"), author);
            head.push_str(crate::tr!("\nТекст:", "\nText:"));
            View {
                head,
                text: Some(text.clone()),
                tail: format!(
                    "{}\n{tag}",
                    link(url, crate::tr!("Открыть на GitHub", "Open on GitHub"))
                ),
            }
        }
        Body::RepoWatched {
            repo,
            private,
            removed,
            url,
        } => {
            let mut head = headline(
                source,
                if *removed {
                    crate::tr!(
                        "Репозиторий снят с наблюдения",
                        "Repository no longer watched"
                    )
                } else {
                    crate::tr!(
                        "Новый репозиторий под наблюдением",
                        "New repository watched"
                    )
                },
            );
            code_field(&mut head, crate::tr!("Репозиторий", "Repository"), repo);
            field(
                &mut head,
                crate::tr!("Доступ", "Access"),
                if *private {
                    crate::tr!("приватный", "private")
                } else {
                    crate::tr!("публичный", "public")
                },
            );
            View {
                head,
                text: None,
                tail: format!(
                    "{}\n{tag}",
                    link(url, crate::tr!("Открыть на GitHub", "Open on GitHub"))
                ),
            }
        }
        Body::GuideWatched {
            title,
            game,
            gone,
            url,
        } => {
            let mut head = headline(
                source,
                if *gone {
                    crate::tr!(
                        "Руководство пропало (удалено или скрыто)",
                        "Guide gone (deleted or hidden)"
                    )
                } else {
                    crate::tr!("Новое руководство под наблюдением", "New guide watched")
                },
            );
            field(&mut head, crate::tr!("Руководство", "Guide"), title);
            if !game.is_empty() {
                field(&mut head, crate::tr!("Игра", "Game"), game);
            }
            View {
                head,
                text: None,
                tail: format!(
                    "{}\n{tag}",
                    link(url, crate::tr!("Открыть руководство", "Open the guide"))
                ),
            }
        }
        Body::GuideComment {
            deleted,
            guide,
            guide_url,
            author,
            author_url,
            posted_at,
            text,
        } => {
            let mut head = headline(
                source,
                if *deleted {
                    crate::tr!("Комментарий удалён", "Comment deleted")
                } else {
                    crate::tr!("Новый комментарий к руководству", "New guide comment")
                },
            );
            field(&mut head, crate::tr!("Руководство", "Guide"), guide);
            let author_link = safe_link(author_url).map_or_else(
                || external_html(author),
                |u| {
                    format!(
                        "<a href=\"{}\">{}</a>",
                        escape_html(u),
                        external_html(author)
                    )
                },
            );
            let _ = write!(head, "\n{}: {author_link}", crate::tr!("Автор", "Author"));
            field(
                &mut head,
                crate::tr!("Время", "Time"),
                &crate::fmt_time(*posted_at),
            );
            let _ = write!(
                head,
                "\n{}",
                if *deleted {
                    crate::tr!("Удалённый текст:", "Deleted text:")
                } else {
                    crate::tr!("Текст:", "Text:")
                }
            );
            View {
                head,
                text: Some(text.clone()),
                tail: format!(
                    "{}\n{tag}",
                    link(
                        guide_url,
                        crate::tr!("Открыть руководство", "Open the guide")
                    )
                ),
            }
        }
        Body::GameNews {
            game,
            patch,
            title,
            date,
            text,
            url,
            store_url,
            ..
        } => {
            let mut head = headline(
                source,
                if *patch {
                    crate::tr!("Патчноут", "Patch notes")
                } else {
                    crate::tr!("Новость игры", "Game news")
                },
            );
            field(&mut head, crate::tr!("Игра", "Game"), game);
            field(&mut head, crate::tr!("Заголовок", "Title"), title);
            field(
                &mut head,
                crate::tr!("Дата", "Date"),
                &crate::fmt_time(*date),
            );
            head.push_str(crate::tr!("\nТекст:", "\nText:"));
            View {
                head,
                text: Some(text.clone()),
                tail: format!(
                    "{} · {}\n{tag}",
                    link(url, crate::tr!("Новость", "News")),
                    link(store_url, crate::tr!("Страница игры", "Game page"))
                ),
            }
        }
        Body::GameBuild {
            game,
            is_dlc,
            change,
            branch,
            old_build,
            new_build,
            updated_at,
            store_url,
            ..
        } => {
            let what = match change {
                BuildChange::Updated => crate::tr!("Обновление игры", "Game update"),
                BuildChange::BranchAdded => crate::tr!("Новая ветка", "New branch"),
                BuildChange::BranchRemoved => crate::tr!("Ветка удалена", "Branch removed"),
            };
            let mut head = headline(source, what);
            field(
                &mut head,
                if *is_dlc {
                    "DLC"
                } else {
                    crate::tr!("Игра", "Game")
                },
                game,
            );
            code_field(&mut head, crate::tr!("Ветка", "Branch"), branch);
            let build = match (old_build, new_build) {
                (Some(o), Some(n)) => format!("{o} → {n}"),
                (None, Some(n)) => n.clone(),
                (Some(o), None) => crate::trf!("{o} (ветки больше нет)", "{o} (branch is gone)"),
                (None, None) => crate::tr!("неизвестен", "unknown").into(),
            };
            field(&mut head, crate::tr!("Билд", "Build"), &build);
            if let Some(t) = updated_at {
                field(
                    &mut head,
                    crate::tr!("Время обновления", "Update time"),
                    &crate::fmt_time(*t),
                );
            }
            View {
                head,
                text: None,
                tail: format!(
                    "{}\n{tag}",
                    link(store_url, crate::tr!("Страница игры", "Game page"))
                ),
            }
        }
        Body::Dlc {
            game,
            dlc,
            dlc_appid,
            change,
            store_url,
            ..
        } => {
            let what = match change {
                DlcChange::Added => crate::tr!("Новое DLC", "New DLC"),
                DlcChange::Removed => {
                    crate::tr!("DLC пропало из списка", "DLC removed from the list")
                }
                DlcChange::ContentChanged => {
                    crate::tr!("Обновлён контент DLC", "DLC content updated")
                }
            };
            let mut head = headline(source, what);
            field(&mut head, crate::tr!("Игра", "Game"), game);
            field(&mut head, "DLC", &format!("{dlc} ({dlc_appid})"));
            View {
                head,
                text: None,
                tail: format!(
                    "{}\n{tag}",
                    link(store_url, crate::tr!("Страница DLC", "DLC page"))
                ),
            }
        }
        Body::Service { title, text } => {
            let mut head = headline(Source::Bot, title);
            let _ = write!(head, "\n{}", external_html(text));
            View {
                head,
                text: None,
                tail: Source::Bot.hashtag().to_owned(),
            }
        }
    }
}

type Fields = Vec<(&'static str, String)>;

fn md_fields(body: &Body) -> (Fields, Option<&str>, Fields) {
    let mut f: Vec<(&'static str, String)> = Vec::new();
    let mut links = Vec::new();
    let mut text = None;
    match body {
        Body::GithubComment {
            kind,
            change,
            repo,
            target,
            file,
            line,
            review_state,
            author,
            text: t,
            url,
        } => {
            f.push((
                crate::tr!("Тип", "Type"),
                format!("{} ({})", kind.title(), change.word()),
            ));
            f.push((crate::tr!("Репозиторий", "Repository"), repo.clone()));
            f.push((crate::tr!("Тема", "Topic"), target.clone()));
            if let Some(p) = file {
                f.push((
                    crate::tr!("Файл", "File"),
                    line.map_or_else(
                        || p.clone(),
                        |l| crate::trf!("{p}, строка {l}", "{p}, line {l}"),
                    ),
                ));
            }
            if let Some(s) = review_state {
                f.push((crate::tr!("Вердикт", "Verdict"), s.clone()));
            }
            f.push((crate::tr!("Автор", "Author"), author.clone()));
            text = Some(t.as_str());
            links.push((crate::tr!("Ссылка", "Link"), url.clone()));
        }
        Body::GithubItem {
            kind,
            reopened,
            repo,
            number,
            title,
            author,
            text: t,
            url,
        } => {
            f.push((
                crate::tr!("Тип", "Type"),
                format!(
                    "{}{}",
                    kind.title(),
                    if *reopened {
                        crate::tr!(" (открыт заново)", " (reopened)")
                    } else {
                        ""
                    }
                ),
            ));
            f.push((crate::tr!("Репозиторий", "Repository"), repo.clone()));
            f.push((crate::tr!("Номер", "Number"), format!("#{number}")));
            f.push((crate::tr!("Заголовок", "Title"), title.clone()));
            f.push((crate::tr!("Автор", "Author"), author.clone()));
            text = Some(t.as_str());
            links.push((crate::tr!("Ссылка", "Link"), url.clone()));
        }
        Body::RepoWatched {
            repo,
            private,
            removed,
            url,
        } => {
            f.push((crate::tr!("Репозиторий", "Repository"), repo.clone()));
            f.push((
                crate::tr!("Доступ", "Access"),
                if *private {
                    crate::tr!("приватный", "private")
                } else {
                    crate::tr!("публичный", "public")
                }
                .into(),
            ));
            if *removed {
                f.push((
                    crate::tr!("Состояние", "State"),
                    crate::tr!("снят с наблюдения", "no longer watched").into(),
                ));
            }
            links.push((crate::tr!("Ссылка", "Link"), url.clone()));
        }
        Body::GuideWatched {
            title,
            game,
            gone,
            url,
        } => {
            f.push((crate::tr!("Руководство", "Guide"), title.clone()));
            f.push((crate::tr!("Игра", "Game"), game.clone()));
            f.push((
                crate::tr!("Состояние", "State"),
                if *gone {
                    crate::tr!("пропало", "gone")
                } else {
                    crate::tr!("новое", "new")
                }
                .into(),
            ));
            links.push((crate::tr!("Ссылка", "Link"), url.clone()));
        }
        Body::GuideComment {
            deleted,
            guide,
            guide_url,
            author,
            author_url,
            posted_at,
            text: t,
        } => {
            f.push((
                crate::tr!("Действие", "Action"),
                if *deleted {
                    crate::tr!("удалён", "deleted")
                } else {
                    crate::tr!("новый", "new")
                }
                .into(),
            ));
            f.push((crate::tr!("Руководство", "Guide"), guide.clone()));
            f.push((crate::tr!("Автор", "Author"), author.clone()));
            f.push((crate::tr!("Время", "Time"), crate::fmt_time(*posted_at)));
            text = Some(t.as_str());
            links.push((
                crate::tr!("Профиль автора", "Author profile"),
                author_url.clone(),
            ));
            links.push((crate::tr!("Руководство", "Guide"), guide_url.clone()));
        }
        Body::GameNews {
            game,
            patch,
            title,
            date,
            text: t,
            url,
            store_url,
            ..
        } => {
            f.push((
                crate::tr!("Тип", "Type"),
                if *patch {
                    crate::tr!("патчноут", "patch notes")
                } else {
                    crate::tr!("новость", "news")
                }
                .into(),
            ));
            f.push((crate::tr!("Игра", "Game"), game.clone()));
            f.push((crate::tr!("Заголовок", "Title"), title.clone()));
            f.push((crate::tr!("Дата", "Date"), crate::fmt_time(*date)));
            text = Some(t.as_str());
            links.push((crate::tr!("Новость", "News"), url.clone()));
            links.push((crate::tr!("Страница игры", "Game page"), store_url.clone()));
        }
        Body::GameBuild {
            game,
            branch,
            old_build,
            new_build,
            updated_at,
            store_url,
            ..
        } => {
            f.push((crate::tr!("Игра", "Game"), game.clone()));
            f.push((crate::tr!("Ветка", "Branch"), branch.clone()));
            f.push((
                crate::tr!("Старый билд", "Old build"),
                old_build.clone().unwrap_or_else(|| "-".into()),
            ));
            f.push((
                crate::tr!("Новый билд", "New build"),
                new_build.clone().unwrap_or_else(|| "-".into()),
            ));
            if let Some(t) = updated_at {
                f.push((
                    crate::tr!("Время обновления", "Update time"),
                    crate::fmt_time(*t),
                ));
            }
            links.push((crate::tr!("Страница игры", "Game page"), store_url.clone()));
        }
        Body::Dlc {
            game,
            dlc,
            dlc_appid,
            change,
            store_url,
            ..
        } => {
            f.push((crate::tr!("Игра", "Game"), game.clone()));
            f.push(("DLC", format!("{dlc} ({dlc_appid})")));
            f.push((
                crate::tr!("Изменение", "Change"),
                match change {
                    DlcChange::Added => crate::tr!("новое", "new"),
                    DlcChange::Removed => crate::tr!("пропало", "gone"),
                    DlcChange::ContentChanged => crate::tr!("обновлён контент", "content updated"),
                }
                .into(),
            ));
            links.push((crate::tr!("Страница DLC", "DLC page"), store_url.clone()));
        }
        Body::Service { title, text: t } => {
            f.push((crate::tr!("Событие", "Event"), title.clone()));
            text = Some(t.as_str());
        }
    }
    (f, text, links)
}

/// Markdown entry with the same fields as the message.
pub fn md_entry(body: &Body, occurred_at: i64) -> String {
    let (fields, text, links) = md_fields(body);
    let mut s = format!(
        "#### {} · {}\n\n",
        md_inline(&crate::fmt_time(occurred_at)),
        md_inline(type_title(body))
    );
    for (k, v) in fields {
        let _ = writeln!(s, "- {k}: {}", md_inline(&v));
    }
    for (k, v) in links {
        if let Some(u) = safe_link(&v) {
            let _ = writeln!(s, "- {k}: <{u}>");
        }
    }
    if let Some(t) = text {
        let _ = write!(s, "\n{}\n", md_block(t));
    }
    s.push('\n');
    s
}

fn anchor(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

pub struct Item<'a> {
    pub source: Source,
    pub body: &'a Body,
    pub occurred_at: i64,
}

const ORDER: [Source; 4] = [
    Source::Github,
    Source::SteamGuide,
    Source::SteamUpdate,
    Source::Bot,
];

/// Digest file: title with period, contents, sections by source and object, chronological.
pub fn digest_markdown(items: &[Item<'_>], from: i64, to: i64) -> String {
    let mut by_source: BTreeMap<usize, BTreeMap<String, Vec<&Item<'_>>>> = BTreeMap::new();
    for it in items {
        let idx = ORDER.iter().position(|s| *s == it.source).unwrap_or(3);
        by_source
            .entry(idx)
            .or_default()
            .entry(group_title(it.body))
            .or_default()
            .push(it);
    }
    let (from, to, n) = (
        md_inline(&crate::fmt_time(from)),
        md_inline(&crate::fmt_time(to)),
        items.len(),
    );
    let mut s = crate::trf!(
        "# Дайджест уведомлений\n\nПериод: {from} - {to}  \nСобытий: {n}\n\n## Оглавление\n\n",
        "# Notification digest\n\nPeriod: {from} - {to}  \nEvents: {n}\n\n## Contents\n\n"
    );
    for (idx, groups) in &by_source {
        let title = ORDER
            .get(*idx)
            .map_or(crate::tr!("Прочее", "Other"), |s| s.title());
        let n: usize = groups.values().map(Vec::len).sum();
        let _ = writeln!(s, "- [{title}](#{}) ({n})", anchor(title));
    }
    for (idx, groups) in &by_source {
        let title = ORDER
            .get(*idx)
            .map_or(crate::tr!("Прочее", "Other"), |s| s.title());
        let _ = write!(s, "\n## {title}\n\n");
        for (group, list) in groups {
            let _ = write!(s, "### {}\n\n", md_inline(group));
            let mut list = list.clone();
            list.sort_by_key(|i| i.occurred_at);
            for it in list {
                s.push_str(&md_entry(it.body, it.occurred_at));
            }
        }
    }
    s
}

/// Splits a Markdown file into numbered parts under the Telegram document limit.
pub fn split_parts(md: &str, limit: usize) -> Vec<String> {
    if md.len() <= limit {
        return vec![md.to_owned()];
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    for chunk in md.split_inclusive("\n#### ") {
        if !cur.is_empty() && cur.len() + chunk.len() > limit {
            parts.push(std::mem::take(&mut cur));
        }
        if chunk.len() > limit {
            let mut start = 0;
            while start < chunk.len() {
                let mut end = (start + limit).min(chunk.len());
                while !chunk.is_char_boundary(end) {
                    end -= 1;
                }
                parts.push(chunk.get(start..end).unwrap_or_default().to_owned());
                start = end;
            }
        } else {
            cur.push_str(chunk);
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// Caption: counts by source and type.
pub fn digest_summary(items: &[Item<'_>]) -> String {
    let mut counts: BTreeMap<usize, BTreeMap<&'static str, usize>> = BTreeMap::new();
    for it in items {
        let idx = ORDER.iter().position(|s| *s == it.source).unwrap_or(3);
        *counts
            .entry(idx)
            .or_default()
            .entry(type_title(it.body))
            .or_default() += 1;
    }
    let total_n = items.len();
    let mut s = crate::trf!(
        "<b>Дайджест · событий: {total_n}</b>",
        "<b>Digest · events: {total_n}</b>"
    );
    let mut tags = Vec::new();
    for (idx, types) in &counts {
        let src = ORDER.get(*idx).copied().unwrap_or(Source::Bot);
        let total: usize = types.values().sum();
        let detail: Vec<String> = types
            .iter()
            .map(|(t, n)| format!("{}: {n}", t.to_lowercase()))
            .collect();
        let _ = write!(
            s,
            "\n{}: {total} ({})",
            escape_html(src.title()),
            escape_html(&detail.join(", "))
        );
        tags.push(src.hashtag());
    }
    s.push_str(crate::tr!(
        "\nВсе события в приложенном файле.\n",
        "\nAll events are in the attached file.\n"
    ));
    s.push_str(&tags.join(" "));
    if visible_len(&s) > MAX_CAPTION {
        let mut short = crate::trf!(
            "<b>Дайджест · событий: {total_n}</b>\nВсе события в приложенном файле.\n",
            "<b>Digest · events: {total_n}</b>\nAll events are in the attached file.\n"
        );
        short.push_str(&tags.join(" "));
        return short;
    }
    s
}

pub fn file_name(prefix: &str, ts: i64, part: Option<(usize, usize)>) -> String {
    let stamp = jiff::Timestamp::from_second(ts).map_or_else(
        |_| ts.to_string(),
        |t| {
            t.to_zoned_datetime(crate::timezone())
                .strftime("%Y-%m-%d_%H-%M-%S")
                .to_string()
        },
    );
    match part {
        Some((i, n)) if n > 1 => format!("{prefix}-{stamp}-part{i}of{n}.md"),
        _ => format!("{prefix}-{stamp}.md"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::GhCommentKind;

    fn comment(text: &str) -> Body {
        Body::GithubComment {
            kind: GhCommentKind::Issue,
            change: Change::New,
            repo: "a/b".into(),
            target: "#1 <script>".into(),
            file: None,
            line: None,
            review_state: None,
            author: "evil.com".into(),
            text: text.into(),
            url: "https://github.com/a/b/issues/1#issuecomment-2".into(),
        }
    }

    #[test]
    fn html_escapes_and_defuses() {
        let v = view(
            Source::Github,
            &comment("<b>x</b> https://evil.com/a /pause"),
        );
        let h = v.html();
        assert!(h.contains("&lt;script&gt;"));
        assert!(h.contains("&lt;b&gt;x&lt;/") && !h.contains("<b>x"));
        assert!(!h.contains("https://evil.com"));
        assert!(!h.contains("/pause"));
        assert!(h.contains("<a href=\"https://github.com/a/b/issues/1#issuecomment-2\">"));
        assert!(h.ends_with("#github"));
    }

    #[test]
    fn foreign_links_dropped() {
        assert!(safe_link("https://evil.com/").is_none());
        assert!(safe_link("http://github.com/x").is_none());
        assert!(safe_link("https://github.com/a\"onmouseover=").is_none());
        let mut b = comment("x");
        if let Body::GithubComment { url, .. } = &mut b {
            *url = "https://evil.com/".into();
        }
        assert!(!view(Source::Github, &b).html().contains("href"));
    }

    #[test]
    fn visible_length() {
        assert_eq!(visible_len("<b>ab</b> &amp; <a href=\"x\">c</a>"), 6);
    }

    #[test]
    fn long_text_summary_fits() {
        let v = view(Source::Github, &comment(&"word ".repeat(3000)));
        assert!(visible_len(&v.html()) > MAX_MESSAGE);
        assert!(visible_len(&v.summary_html(MAX_MESSAGE)) <= MAX_MESSAGE);
    }

    #[test]
    fn digest_structure_survives_hostile_markdown() {
        let b1 = comment("```\n# Fake heading\n```\n[link](javascript:x)");
        let b2 = Body::Service {
            title: "t".into(),
            text: "s".into(),
        };
        let items = [
            Item {
                source: Source::Github,
                body: &b1,
                occurred_at: 100,
            },
            Item {
                source: Source::Bot,
                body: &b2,
                occurred_at: 50,
            },
        ];
        let md = digest_markdown(&items, 50, 100);
        assert!(md.contains("## Оглавление"));
        assert!(md.contains("````text\n```\n# Fake heading\n```"));
        assert_eq!(md.matches("\n## ").count(), 3);
        let caption = digest_summary(&items);
        assert!(caption.contains("событий: 2"));
        assert!(caption.contains("#github") && caption.contains("#bot"));
    }

    #[test]
    fn parts_split_on_entries() {
        let md = format!(
            "head\n#### a\n{}\n#### b\n{}",
            "x".repeat(50),
            "y".repeat(50)
        );
        let parts = split_parts(&md, 80);
        assert!(parts.len() >= 2);
        assert_eq!(parts.concat(), md);
    }
}
