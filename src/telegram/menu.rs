// SPDX-License-Identifier: GPL-3.0-or-later
//! `/settings`: screens with inline buttons and the toggles behind them.

use std::fmt::Write as _;

use super::api::Keyboard;
use crate::i18n::Lang;
use crate::settings::{Delivery, Settings};
use crate::text::{escape_html, external_html};

pub const REPOS_PER_PAGE: usize = 8;

/// What the bot waits for after a button asked for text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    AddGame,
    QuietHours,
    Timezone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Main,
    Github,
    Repos(usize),
    Guides,
    Games,
    Game(u32),
    Chats,
    Chat(i64),
    Quiet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Show(View),
    Toggle(String),
    Repo(usize, usize),
    Ask(Prompt),
    RemoveGame(u32),
    PickGame(u32),
    Leave(i64),
    Close,
}

/// Which sources have credentials on this installation.
#[derive(Debug, Clone, Copy)]
pub struct Avail {
    pub github: bool,
    pub guides: bool,
}

fn view_data(v: View) -> String {
    match v {
        View::Main => "m".into(),
        View::Github => "m:gh".into(),
        View::Repos(p) => format!("m:rp:{p}"),
        View::Guides => "m:gd".into(),
        View::Games => "m:gm".into(),
        View::Game(id) => format!("m:g:{id}"),
        View::Chats => "m:ch".into(),
        View::Chat(id) => format!("m:c:{id}"),
        View::Quiet => "m:qh".into(),
    }
}

pub fn parse(data: &str) -> Option<Action> {
    let parts: Vec<&str> = data.split(':').collect();
    let num = |i: usize| parts.get(i).and_then(|p| p.parse::<i64>().ok());
    Some(match parts.as_slice() {
        ["m"] => Action::Show(View::Main),
        ["m", "gh"] => Action::Show(View::Github),
        ["m", "rp", _] => Action::Show(View::Repos(usize::try_from(num(2)?).ok()?)),
        ["m", "gd"] => Action::Show(View::Guides),
        ["m", "gm"] => Action::Show(View::Games),
        ["m", "g", _] => Action::Show(View::Game(u32::try_from(num(2)?).ok()?)),
        ["m", "ch"] => Action::Show(View::Chats),
        ["m", "c", _] => Action::Show(View::Chat(num(2)?)),
        ["m", "qh"] => Action::Show(View::Quiet),
        ["t", ..] => Action::Toggle(data.get(2..)?.to_owned()),
        ["r", _, _] => Action::Repo(
            usize::try_from(num(1)?).ok()?,
            usize::try_from(num(2)?).ok()?,
        ),
        ["ask", "game"] => Action::Ask(Prompt::AddGame),
        ["ask", "qh"] => Action::Ask(Prompt::QuietHours),
        ["ask", "tz"] => Action::Ask(Prompt::Timezone),
        ["rm", _] => Action::RemoveGame(u32::try_from(num(1)?).ok()?),
        ["pick", _] => Action::PickGame(u32::try_from(num(1)?).ok()?),
        ["leave", _] => Action::Leave(num(1)?),
        ["x"] => Action::Close,
        _ => return None,
    })
}

fn flag(d: &mut Delivery, what: &str) -> Option<()> {
    match what {
        "gh" => d.github = !d.github,
        "gd" => d.guides = !d.guides,
        "gm" => d.games = !d.games,
        _ => return None,
    }
    Some(())
}

/// Flips one setting; returns the screen to show next, or `None` for an unknown key.
pub fn toggle(s: &mut Settings, key: &str) -> Option<View> {
    let g = &mut s.github;
    let parts: Vec<&str> = key.split(':').collect();
    let view = match key {
        "gh.on" => {
            g.enabled = !g.enabled;
            View::Github
        }
        "gh.issues" => {
            g.issues = !g.issues;
            View::Github
        }
        "gh.prs" => {
            g.pull_requests = !g.pull_requests;
            View::Github
        }
        "gh.disc" => {
            g.discussions = !g.discussions;
            View::Github
        }
        "gh.com" => {
            g.comments = !g.comments;
            View::Github
        }
        "gh.rev" => {
            g.reviews = !g.reviews;
            View::Github
        }
        "gh.revc" => {
            g.review_comments = !g.review_comments;
            View::Github
        }
        "gh.commit" => {
            g.commit_comments = !g.commit_comments;
            View::Github
        }
        "gh.repos" => {
            g.repositories = !g.repositories;
            View::Github
        }
        "gh.edits" => {
            g.edits = !g.edits;
            View::Github
        }
        "gh.dels" => {
            g.deletes = !g.deletes;
            View::Github
        }
        "gd.on" => {
            s.guides.enabled = !s.guides.enabled;
            View::Guides
        }
        "gd.com" => {
            s.guides.comments = !s.guides.comments;
            View::Guides
        }
        "gd.del" => {
            s.guides.deleted_comments = !s.guides.deleted_comments;
            View::Guides
        }
        "gd.list" => {
            s.guides.guide_changes = !s.guides.guide_changes;
            View::Guides
        }
        "gm.on" => {
            s.games.enabled = !s.games.enabled;
            View::Games
        }
        "qh.on" => {
            s.quiet_hours.enabled = !s.quiet_hours.enabled;
            View::Quiet
        }
        "lang" => {
            s.language = match s.language {
                Lang::Ru => Lang::En,
                Lang::En => Lang::Ru,
            };
            View::Main
        }
        _ => match parts.as_slice() {
            ["a", what] => {
                flag(&mut s.admin, what)?;
                View::Chats
            }
            ["c", id, what] => {
                let id: i64 = id.parse().ok()?;
                let chat = s.chats.iter_mut().find(|c| c.id == id)?;
                flag(&mut chat.delivery, what)?;
                View::Chat(id)
            }
            ["g", id, what] => {
                let id: u32 = id.parse().ok()?;
                let game = s.games.list.iter_mut().find(|x| x.appid == id)?;
                match *what {
                    "b" => game.builds = !game.builds,
                    "br" => game.branches = !game.branches,
                    "d" => game.dlc = !game.dlc,
                    "n" => game.news = !game.news,
                    "p" => game.patches = !game.patches,
                    _ => return None,
                }
                View::Game(id)
            }
            _ => return None,
        },
    };
    Some(view)
}

fn mark(on: bool) -> &'static str {
    if on {
        "✅"
    } else {
        "⬜"
    }
}

fn b(label: impl Into<String>, data: impl Into<String>) -> (String, String) {
    (label.into(), data.into())
}

fn t(on: bool, label: &str, key: &str) -> (String, String) {
    b(format!("{} {label}", mark(on)), format!("t:{key}"))
}

fn back(to: View) -> Vec<(String, String)> {
    vec![b(crate::tr!("‹ Назад", "‹ Back"), view_data(to))]
}

fn state_word(on: bool, configured: bool) -> &'static str {
    match (on, configured) {
        (_, false) => crate::tr!("не настроено", "not configured"),
        (true, true) => crate::tr!("включено", "on"),
        (false, true) => crate::tr!("выключено", "off"),
    }
}

fn not_configured(what: &str) -> String {
    crate::trf!(
        "\n\n{what} не настроено на этой установке: нет ключей. Добавьте их в программе установки HQ Watchman Setup.",
        "\n\n{what} is not configured on this installation: credentials are missing. Add them in the HQ Watchman Setup app."
    )
}

/// Text and buttons of one screen.
pub fn render(v: View, s: &Settings, avail: Avail, repos: &[String]) -> (String, Keyboard) {
    match v {
        View::Main => main(s, avail),
        View::Github => github(s, avail),
        View::Repos(page) => repos_view(s, repos, page),
        View::Guides => guides(s, avail),
        View::Games => games(s),
        View::Game(id) => game(s, id),
        View::Chats => chats(s),
        View::Chat(id) => chat(s, id),
        View::Quiet => quiet(s),
    }
}

fn main(s: &Settings, avail: Avail) -> (String, Keyboard) {
    let gh = state_word(s.github.enabled, avail.github);
    let gd = state_word(s.guides.enabled, avail.guides);
    let gm = state_word(s.games.enabled, true);
    let games = s.games.list.len();
    let chats = s.chats.len();
    let qh = if s.quiet_hours.enabled {
        format!("{}-{}", s.quiet_hours.start, s.quiet_hours.end)
    } else {
        crate::tr!("выключены", "off").to_owned()
    };
    let tz = escape_html(&s.timezone);
    let text = crate::trf!(
        "<b>Настройки</b>\nGitHub: {gh}\nРуководства Steam: {gd}\nИгры Steam: {gm}, игр: {games}\nГруппы и каналы: {chats}\nТихие часы: {qh}\nЧасовой пояс: {tz}\nЯзык: русский",
        "<b>Settings</b>\nGitHub: {gh}\nSteam guides: {gd}\nSteam games: {gm}, games: {games}\nGroups and channels: {chats}\nQuiet hours: {qh}\nTime zone: {tz}\nLanguage: English"
    );
    let kb = vec![
        vec![
            b("GitHub ›", "m:gh"),
            b(crate::tr!("Руководства ›", "Guides ›"), "m:gd"),
        ],
        vec![
            b(crate::tr!("Игры ›", "Games ›"), "m:gm"),
            b(crate::tr!("Куда слать ›", "Delivery ›"), "m:ch"),
        ],
        vec![
            b(crate::tr!("Тихие часы ›", "Quiet hours ›"), "m:qh"),
            b(crate::tr!("Language: English", "Язык: русский"), "t:lang"),
        ],
        vec![b(crate::tr!("Закрыть", "Close"), "x")],
    ];
    (text, kb)
}

fn github(s: &Settings, avail: Avail) -> (String, Keyboard) {
    let g = &s.github;
    let mut text = crate::tr!(
        "<b>GitHub</b>\nКакие события присылать. Свои действия и боты не присылаются никогда.",
        "<b>GitHub</b>\nWhich events to send. Your own actions and bots are never sent."
    )
    .to_owned();
    if !g.excluded_repos.is_empty() {
        let n = g.excluded_repos.len();
        let _ = write!(
            text,
            "{}",
            crate::trf!(
                "\nИсключено репозиториев: {n}.",
                "\nExcluded repositories: {n}."
            )
        );
    }
    if !avail.github {
        text.push_str(&not_configured("GitHub"));
        return (text, vec![back(View::Main)]);
    }
    let kb = vec![
        vec![t(
            g.enabled,
            crate::tr!("Уведомления GitHub", "GitHub notifications"),
            "gh.on",
        )],
        vec![
            t(g.issues, "Issues", "gh.issues"),
            t(g.pull_requests, "Pull requests", "gh.prs"),
        ],
        vec![
            t(
                g.discussions,
                crate::tr!("Обсуждения", "Discussions"),
                "gh.disc",
            ),
            t(g.comments, crate::tr!("Комментарии", "Comments"), "gh.com"),
        ],
        vec![
            t(g.reviews, crate::tr!("Ревью", "Reviews"), "gh.rev"),
            t(
                g.review_comments,
                crate::tr!("Комментарии к коду", "Code comments"),
                "gh.revc",
            ),
        ],
        vec![
            t(
                g.commit_comments,
                crate::tr!("К коммитам", "Commit comments"),
                "gh.commit",
            ),
            t(
                g.repositories,
                crate::tr!("Новые репозитории", "New repositories"),
                "gh.repos",
            ),
        ],
        vec![
            t(g.edits, crate::tr!("Изменения", "Edits"), "gh.edits"),
            t(g.deletes, crate::tr!("Удаления", "Deletions"), "gh.dels"),
        ],
        vec![b(crate::tr!("Репозитории ›", "Repositories ›"), "m:rp:0")],
        back(View::Main),
    ];
    (text, kb)
}

fn repos_view(s: &Settings, repos: &[String], page: usize) -> (String, Keyboard) {
    let pages = repos.len().div_ceil(REPOS_PER_PAGE).max(1);
    let page = page.min(pages - 1);
    let text = if repos.is_empty() {
        crate::tr!(
            "<b>Репозитории</b>\nПока ни одного. Они появятся после первой проверки GitHub.",
            "<b>Repositories</b>\nNone yet. They appear after the first GitHub check."
        )
        .to_owned()
    } else {
        crate::trf!(
            "<b>Репозитории</b>, страница {} из {pages}\n✅ отслеживается, ⬜ исключён. Нажмите, чтобы переключить.",
            "<b>Repositories</b>, page {} of {pages}\n✅ watched, ⬜ excluded. Tap to switch.",
            page + 1
        )
    };
    let mut kb: Keyboard = repos
        .iter()
        .enumerate()
        .skip(page * REPOS_PER_PAGE)
        .take(REPOS_PER_PAGE)
        .map(|(i, name)| {
            let excluded = s
                .github
                .excluded_repos
                .iter()
                .any(|r| r.eq_ignore_ascii_case(name));
            let label: String = name.chars().take(48).collect();
            vec![b(
                format!("{} {label}", mark(!excluded)),
                format!("r:{page}:{}", i - page * REPOS_PER_PAGE),
            )]
        })
        .collect();
    if pages > 1 {
        let mut nav = Vec::new();
        if page > 0 {
            nav.push(b("‹", format!("m:rp:{}", page - 1)));
        }
        if page + 1 < pages {
            nav.push(b("›", format!("m:rp:{}", page + 1)));
        }
        kb.push(nav);
    }
    kb.push(back(View::Github));
    (text, kb)
}

/// Switches the exclusion of the repository shown at `page`/`idx`.
pub fn toggle_repo(s: &mut Settings, repos: &[String], page: usize, idx: usize) -> bool {
    let Some(name) = repos.get(page * REPOS_PER_PAGE + idx) else {
        return false;
    };
    let list = &mut s.github.excluded_repos;
    if let Some(pos) = list.iter().position(|r| r.eq_ignore_ascii_case(name)) {
        list.remove(pos);
    } else {
        list.push(name.clone());
    }
    true
}

fn guides(s: &Settings, avail: Avail) -> (String, Keyboard) {
    let g = &s.guides;
    let mut text = crate::tr!(
        "<b>Руководства Steam</b>\nКомментарии под вашими руководствами. Ваши собственные комментарии не присылаются.",
        "<b>Steam guides</b>\nComments under your guides. Your own comments are not sent."
    )
    .to_owned();
    if !avail.guides {
        text.push_str(&not_configured(crate::tr!(
            "Профиль Steam",
            "Steam profile"
        )));
        return (text, vec![back(View::Main)]);
    }
    let kb = vec![
        vec![t(
            g.enabled,
            crate::tr!("Уведомления о руководствах", "Guide notifications"),
            "gd.on",
        )],
        vec![t(
            g.comments,
            crate::tr!("Новые комментарии", "New comments"),
            "gd.com",
        )],
        vec![t(
            g.deleted_comments,
            crate::tr!("Удалённые комментарии", "Deleted comments"),
            "gd.del",
        )],
        vec![t(
            g.guide_changes,
            crate::tr!("Новые и пропавшие руководства", "New and removed guides"),
            "gd.list",
        )],
        back(View::Main),
    ];
    (text, kb)
}

fn games(s: &Settings) -> (String, Keyboard) {
    let n = s.games.list.len();
    let max = crate::settings::MAX_GAMES;
    let text = crate::trf!(
        "<b>Игры Steam</b>\nСборки, бета-ветки, DLC и новости выбранных игр. Игр: {n} из {max}.",
        "<b>Steam games</b>\nBuilds, beta branches, DLC and news of chosen games. Games: {n} of {max}."
    );
    let mut kb = vec![vec![t(
        s.games.enabled,
        crate::tr!("Уведомления об играх", "Game notifications"),
        "gm.on",
    )]];
    for g in &s.games.list {
        let label: String = g.label().chars().take(48).collect();
        kb.push(vec![b(format!("{label} ›"), format!("m:g:{}", g.appid))]);
    }
    if n < max {
        kb.push(vec![b(
            crate::tr!("+ Добавить игру", "+ Add game"),
            "ask:game",
        )]);
    }
    kb.push(back(View::Main));
    (text, kb)
}

fn game(s: &Settings, appid: u32) -> (String, Keyboard) {
    let Some(g) = s.game(appid) else {
        return games(s);
    };
    let name = external_html(&g.label());
    let text = crate::trf!(
        "<b>{name}</b> (AppID {appid})\nЧто присылать по этой игре.",
        "<b>{name}</b> (AppID {appid})\nWhat to send for this game."
    );
    let id = g.appid;
    let kb = vec![
        vec![
            t(
                g.builds,
                crate::tr!("Обновления (public)", "Updates (public)"),
                &format!("g:{id}:b"),
            ),
            t(
                g.branches,
                crate::tr!("Бета-ветки", "Beta branches"),
                &format!("g:{id}:br"),
            ),
        ],
        vec![
            t(g.dlc, "DLC", &format!("g:{id}:d")),
            t(g.news, crate::tr!("Новости", "News"), &format!("g:{id}:n")),
        ],
        vec![t(
            g.patches,
            crate::tr!("Патчноуты", "Patch notes"),
            &format!("g:{id}:p"),
        )],
        vec![b(
            crate::tr!("Убрать игру", "Remove game"),
            format!("rm:{id}"),
        )],
        back(View::Games),
    ];
    (text, kb)
}

fn delivery_row(d: Delivery, prefix: &str) -> Vec<(String, String)> {
    vec![
        t(d.github, "GitHub", &format!("{prefix}:gh")),
        t(
            d.guides,
            crate::tr!("Руков.", "Guides"),
            &format!("{prefix}:gd"),
        ),
        t(
            d.games,
            crate::tr!("Игры", "Games"),
            &format!("{prefix}:gm"),
        ),
    ]
}

fn chats(s: &Settings) -> (String, Keyboard) {
    let text = crate::tr!(
        "<b>Куда слать</b>\nЛичный чат с вами и группы или каналы, куда вы сами добавили бота. Служебные сообщения приходят только вам.\nЧтобы добавить группу, добавьте в неё бота. Для канала сделайте бота администратором с правом публикации.",
        "<b>Delivery</b>\nYour private chat and the groups or channels you added the bot to. Service messages go only to you.\nTo add a group, add the bot to it. For a channel, make the bot an admin allowed to post."
    )
    .to_owned();
    let mut kb = vec![
        vec![b(crate::tr!("Личный чат:", "Private chat:"), "m:ch")],
        delivery_row(s.admin, "a"),
    ];
    for c in &s.chats {
        let label: String = c.title.chars().take(40).collect();
        kb.push(vec![b(format!("{label} ›"), format!("m:c:{}", c.id))]);
    }
    kb.push(back(View::Main));
    (text, kb)
}

fn chat(s: &Settings, id: i64) -> (String, Keyboard) {
    let Some(c) = s.chats.iter().find(|c| c.id == id) else {
        return chats(s);
    };
    let kind = if c.kind == "channel" {
        crate::tr!("канал", "channel")
    } else {
        crate::tr!("группа", "group")
    };
    let title = external_html(&c.title);
    let text = crate::trf!(
        "<b>{title}</b> ({kind})\nКакие уведомления сюда присылать.",
        "<b>{title}</b> ({kind})\nWhich notifications to send here."
    );
    let kb = vec![
        delivery_row(c.delivery, &format!("c:{id}")),
        vec![b(
            crate::tr!("Выйти из чата и забыть его", "Leave the chat and forget it"),
            format!("leave:{id}"),
        )],
        back(View::Chats),
    ];
    (text, kb)
}

fn quiet(s: &Settings) -> (String, Keyboard) {
    let (start, end) = (&s.quiet_hours.start, &s.quiet_hours.end);
    let tz = escape_html(&s.timezone);
    let text = crate::trf!(
        "<b>Тихие часы</b>\nВ это время сообщения приходят без звука.\nОкно: {start}-{end}\nЧасовой пояс: {tz}",
        "<b>Quiet hours</b>\nMessages arrive without sound during this time.\nWindow: {start}-{end}\nTime zone: {tz}"
    );
    let kb = vec![
        vec![t(
            s.quiet_hours.enabled,
            crate::tr!("Тихие часы", "Quiet hours"),
            "qh.on",
        )],
        vec![
            b(crate::tr!("Изменить окно", "Change window"), "ask:qh"),
            b(crate::tr!("Часовой пояс", "Time zone"), "ask:tz"),
        ],
        back(View::Main),
    ];
    (text, kb)
}

pub fn prompt_text(p: Prompt) -> &'static str {
    match p {
        Prompt::AddGame => crate::tr!(
            "Пришлите AppID, ссылку на страницу игры в Steam или часть названия. Отмена: /settings.",
            "Send the AppID, a link to the Steam store page or part of the name. Cancel: /settings."
        ),
        Prompt::QuietHours => crate::tr!(
            "Пришлите окно тихих часов в виде 22:00-08:00. Отмена: /settings.",
            "Send the quiet hours window like 22:00-08:00. Cancel: /settings."
        ),
        Prompt::Timezone => crate::tr!(
            "Пришлите часовой пояс IANA, например Europe/Moscow или Asia/Almaty. Отмена: /settings.",
            "Send an IANA time zone, for example Europe/Berlin or America/New_York. Cancel: /settings."
        ),
    }
}

/// Parses `22:00-08:00`.
pub fn parse_window(text: &str) -> Option<(String, String)> {
    let (a, z) = text.trim().split_once(['-', '–'])?;
    let a = crate::settings::parse_hhmm(a)?;
    let z = crate::settings::parse_hhmm(z)?;
    Some((
        a.strftime("%H:%M").to_string(),
        z.strftime("%H:%M").to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_data_round_trip() {
        for v in [
            View::Main,
            View::Github,
            View::Repos(3),
            View::Guides,
            View::Games,
            View::Game(572_890),
            View::Chats,
            View::Chat(-1_001_234),
            View::Quiet,
        ] {
            assert_eq!(parse(&view_data(v)), Some(Action::Show(v)));
            assert!(view_data(v).len() <= 64);
        }
        assert_eq!(parse("t:gh.on"), Some(Action::Toggle("gh.on".into())));
        assert_eq!(parse("r:1:2"), Some(Action::Repo(1, 2)));
        assert_eq!(parse("leave:-100"), Some(Action::Leave(-100)));
        assert_eq!(parse("bogus"), None);
        assert_eq!(parse("m:g:x"), None);
    }

    #[test]
    fn toggles_flip_and_route() {
        let mut s = Settings::default();
        s.games.list.push(crate::settings::Game::new(10, "G"));
        assert_eq!(toggle(&mut s, "gh.issues"), Some(View::Github));
        assert!(!s.github.issues);
        assert_eq!(toggle(&mut s, "g:10:p"), Some(View::Game(10)));
        assert!(!s.games.list[0].patches);
        assert_eq!(toggle(&mut s, "a:gm"), Some(View::Chats));
        assert!(!s.admin.games);
        assert_eq!(toggle(&mut s, "g:11:p"), None);
        assert_eq!(toggle(&mut s, "zz"), None);
        assert_eq!(toggle(&mut s, "lang"), Some(View::Main));
        assert_eq!(s.language, Lang::En);
    }

    #[test]
    fn every_screen_has_valid_buttons() {
        let mut s = Settings::default();
        s.games.list.push(crate::settings::Game::new(10, "G"));
        s.chats.push(crate::settings::Chat {
            id: -5,
            title: "T".into(),
            kind: "group".into(),
            delivery: Delivery::default(),
        });
        let repos: Vec<String> = (0..20).map(|i| format!("o/r{i}")).collect();
        let avail = Avail {
            github: true,
            guides: true,
        };
        for v in [
            View::Main,
            View::Github,
            View::Repos(1),
            View::Guides,
            View::Games,
            View::Game(10),
            View::Chats,
            View::Chat(-5),
            View::Quiet,
        ] {
            let (_, kb) = render(v, &s, avail, &repos);
            for (_, data) in kb.iter().flatten() {
                assert!(data.len() <= 64, "{data}");
                assert!(parse(data).is_some(), "{data}");
            }
        }
        assert!(toggle_repo(&mut s, &repos, 1, 0));
        assert_eq!(s.github.excluded_repos, vec!["o/r8".to_owned()]);
        assert!(toggle_repo(&mut s, &repos, 1, 0));
        assert_eq!(s.github.excluded_repos, [] as [std::string::String; 0]);
    }

    #[test]
    fn quiet_window() {
        assert_eq!(
            parse_window("22:00-8:00"),
            Some(("22:00".into(), "08:00".into()))
        );
        assert_eq!(parse_window("22:00"), None);
        assert_eq!(parse_window("25:00-08:00"), None);
    }
}
