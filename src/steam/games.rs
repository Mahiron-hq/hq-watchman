// SPDX-License-Identifier: GPL-3.0-or-later
//! Game updates: PICS builds, branches and DLC, plus news and patch notes.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use super::vdf;
use super::{bbcode, store_url, SteamError, SteamHttp};
use crate::config::Config;
use crate::event::{Body, BuildChange, DlcChange, Event, Source};
use crate::health::Health;
use crate::storage::Db;
use crate::text;

const MAX_DLC: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    pub buildid: String,
    pub time_updated: Option<i64>,
    pub password: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DlcState {
    pub name: String,
    pub music: bool,
    pub content: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameState {
    pub name: String,
    pub branches: BTreeMap<String, Branch>,
    pub dlcs: BTreeMap<u32, DlcState>,
    #[serde(default)]
    pub change_number: u32,
    #[serde(default)]
    pub fallback_version: Option<u64>,
}

/// Fields read from one app info document.
#[derive(Debug, Default)]
pub struct AppInfo {
    pub name: String,
    pub kind: String,
    pub branches: BTreeMap<String, Branch>,
    pub dlc_ids: Vec<u32>,
    pub dlc_depots: BTreeMap<u32, Vec<(String, String)>>,
    pub own_depots: Vec<(String, String)>,
}

pub fn parse_app_info(text: &str) -> Result<AppInfo> {
    let doc = vdf::parse(text)?;
    let root = doc
        .get("appinfo")
        .ok_or_else(|| anyhow!("app info without root"))?;
    let common = root.get("common");
    let name = common
        .and_then(|c| c.str_at(&["name"]))
        .map(|n| text::clean_line(n, 200))
        .unwrap_or_default();
    let kind = common
        .and_then(|c| c.str_at(&["type"]))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let dlc_ids = root
        .str_at(&["extended", "listofdlc"])
        .map(|s| {
            s.split(',')
                .filter_map(|x| x.trim().parse::<u32>().ok())
                .filter(|&x| x > 0)
                .take(MAX_DLC)
                .collect()
        })
        .unwrap_or_default();
    let mut info = AppInfo {
        name,
        kind,
        dlc_ids,
        ..AppInfo::default()
    };
    if let Some(depots) = root.get("depots") {
        if let Some(branches) = depots.get("branches") {
            for (bname, b) in branches.entries() {
                let Some(buildid) = b.str_at(&["buildid"]) else {
                    continue;
                };
                info.branches.insert(
                    text::clean_line(bname, 100),
                    Branch {
                        buildid: text::clean_line(buildid, 20),
                        time_updated: b.str_at(&["timeupdated"]).and_then(|t| t.parse().ok()),
                        password: b.str_at(&["pwdrequired"]) == Some("1"),
                    },
                );
            }
        }
        for (depot, d) in depots.entries() {
            if !depot.bytes().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let gid = d
                .str_at(&["manifests", "public", "gid"])
                .or_else(|| d.str_at(&["manifests", "public"]))
                .map(|g| text::clean_line(g, 32));
            let Some(gid) = gid else { continue };
            let entry = (depot.clone(), gid);
            match d.str_at(&["dlcappid"]).and_then(|x| x.parse::<u32>().ok()) {
                Some(dlc) => info.dlc_depots.entry(dlc).or_default().push(entry),
                None => info.own_depots.push(entry),
            }
        }
    }
    Ok(info)
}

fn branch_label(name: &str, b: &Branch) -> String {
    if b.password {
        crate::trf!("{name} (с паролем)", "{name} (password)")
    } else {
        name.to_owned()
    }
}

/// Compares two snapshots of one game and builds events.
pub fn diff(appid: u32, old: &GameState, new: &GameState) -> Vec<Event> {
    let mut out = Vec::new();
    let game = new.name.clone();
    let url = store_url(appid);
    let cn = new.change_number;
    for (name, b) in &new.branches {
        let (change, old_build) = match old.branches.get(name) {
            None => (BuildChange::BranchAdded, None),
            Some(o) if o.buildid != b.buildid => (BuildChange::Updated, Some(o.buildid.clone())),
            Some(_) => continue,
        };
        let key = match change {
            BuildChange::BranchAdded => {
                format!("steam:branch_add:{appid}:{name}:{}:{cn}", b.buildid)
            }
            _ => format!("steam:build:{appid}:{name}:{}", b.buildid),
        };
        let mut ev = Event::new(
            Source::SteamUpdate,
            key,
            b.time_updated.unwrap_or_else(crate::now),
            Body::GameBuild {
                game: game.clone(),
                appid,
                is_dlc: false,
                change,
                branch: branch_label(name, b),
                old_build,
                new_build: Some(b.buildid.clone()),
                updated_at: b.time_updated,
                store_url: url.clone(),
            },
        );
        if name == "public" && change == BuildChange::Updated {
            ev.merge_app = Some(appid);
        }
        out.push(ev);
    }
    for (name, b) in &old.branches {
        if !new.branches.contains_key(name) {
            out.push(Event::new(
                Source::SteamUpdate,
                format!("steam:branch_rm:{appid}:{name}:{}:{cn}", b.buildid),
                crate::now(),
                Body::GameBuild {
                    game: game.clone(),
                    appid,
                    is_dlc: false,
                    change: BuildChange::BranchRemoved,
                    branch: branch_label(name, b),
                    old_build: Some(b.buildid.clone()),
                    new_build: None,
                    updated_at: None,
                    store_url: url.clone(),
                },
            ));
        }
    }
    let mut dlc_event = |dlc_id: u32, d: &DlcState, change: DlcChange, tag: &str| {
        out.push(Event::new(
            Source::SteamUpdate,
            format!("steam:dlc_{tag}:{appid}:{dlc_id}:{cn}"),
            crate::now(),
            Body::Dlc {
                game: game.clone(),
                appid,
                dlc: d.name.clone(),
                dlc_appid: dlc_id,
                change,
                store_url: store_url(dlc_id),
            },
        ));
    };
    for (id, d) in &new.dlcs {
        if d.music {
            continue;
        }
        match old.dlcs.get(id) {
            None => dlc_event(*id, d, DlcChange::Added, "add"),
            Some(o) if o.content != d.content && !o.content.is_empty() => {
                dlc_event(*id, d, DlcChange::ContentChanged, "content");
            }
            Some(_) => {}
        }
    }
    for (id, d) in &old.dlcs {
        if !d.music && !new.dlcs.contains_key(id) {
            dlc_event(*id, d, DlcChange::Removed, "rm");
        }
    }
    out
}

/// Builds new states for the given apps from PICS. A DLC that PICS did not describe
/// keeps its stored state instead of turning into an empty one.
async fn fetch_states(
    steam: &SteamHttp,
    appids: &[u32],
    old: &BTreeMap<u32, GameState>,
) -> Result<BTreeMap<u32, GameState>> {
    let info = super::cm::fetch_product_info(steam.http(), appids).await?;
    let mut parsed = BTreeMap::new();
    for raw in &info.apps {
        parsed.insert(raw.appid, (raw.change_number, parse_app_info(&raw.text)?));
    }
    let dlc_ids: BTreeSet<u32> = parsed
        .values()
        .flat_map(|(_, a)| a.dlc_ids.iter().copied())
        .collect();
    let dlc_ids: Vec<u32> = dlc_ids.into_iter().take(MAX_DLC).collect();
    let mut dlc_info = BTreeMap::new();
    if !dlc_ids.is_empty() {
        let info = super::cm::fetch_product_info(steam.http(), &dlc_ids).await?;
        for raw in &info.apps {
            dlc_info.insert(raw.appid, parse_app_info(&raw.text)?);
        }
    }
    let mut states = BTreeMap::new();
    for (appid, (cn, app)) in parsed {
        let mut dlcs = BTreeMap::new();
        for id in &app.dlc_ids {
            let own = dlc_info.get(id);
            if own.is_none() {
                if let Some(prev) = old.get(&appid).and_then(|o| o.dlcs.get(id)) {
                    dlcs.insert(*id, prev.clone());
                    continue;
                }
            }
            let mut content: Vec<(String, String)> =
                app.dlc_depots.get(id).cloned().unwrap_or_default();
            if let Some(d) = own {
                content.extend(d.own_depots.iter().cloned());
            }
            content.sort();
            dlcs.insert(
                *id,
                DlcState {
                    name: own
                        .map(|d| d.name.clone())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| format!("DLC {id}")),
                    music: own.is_some_and(|d| d.kind == "music"),
                    content,
                },
            );
        }
        states.insert(
            appid,
            GameState {
                name: if app.name.is_empty() {
                    format!("App {appid}")
                } else {
                    app.name
                },
                branches: app.branches,
                dlcs,
                change_number: cn,
                fallback_version: None,
            },
        );
    }
    Ok(states)
}

#[derive(Deserialize)]
struct NewsResponse {
    appnews: NewsList,
}

#[derive(Deserialize)]
struct NewsList {
    #[serde(default)]
    newsitems: Vec<NewsItem>,
}

#[derive(Deserialize)]
struct NewsItem {
    gid: String,
    title: String,
    #[serde(default)]
    contents: String,
    #[serde(default)]
    feedname: String,
    date: i64,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct UpToDate {
    response: UpToDateBody,
}

#[derive(Deserialize)]
struct UpToDateBody {
    success: bool,
    #[serde(default)]
    required_version: Option<u64>,
}

pub struct GamesPoller {
    pics_failing_since: Option<i64>,
}

impl Default for GamesPoller {
    fn default() -> Self {
        Self::new()
    }
}

impl GamesPoller {
    pub fn new() -> Self {
        Self {
            pics_failing_since: None,
        }
    }

    pub async fn poll(
        &mut self,
        cfg: &Config,
        appids: &[u32],
        db: &Db,
        health: &Health,
        steam: &SteamHttp,
    ) -> Result<()> {
        db.forget_games_except(appids).await?;
        if appids.is_empty() {
            return Ok(());
        }
        let merge = i64::try_from(cfg.games.merge_window_minutes)? * 60;
        let mut baselined = Vec::new();
        let mut stored = BTreeMap::new();
        for &appid in appids {
            if let Some(s) = db
                .game_state(i64::from(appid))
                .await?
                .and_then(|s| serde_json::from_str::<GameState>(&s).ok())
            {
                stored.insert(appid, s);
            }
        }
        match fetch_states(steam, appids, &stored).await {
            Ok(states) => {
                self.pics_failing_since = None;
                health.ok("pics").await;
                for (appid, mut state) in states {
                    let key = i64::from(appid);
                    let old: Option<GameState> = db
                        .game_state(key)
                        .await?
                        .and_then(|s| serde_json::from_str(&s).ok());
                    let events = if let Some(o) = &old {
                        state.fallback_version = o.fallback_version;
                        diff(appid, o, &state)
                    } else {
                        let (name, branches, dlcs) = (
                            &state.name,
                            state.branches.len(),
                            state.dlcs.values().filter(|d| !d.music).count(),
                        );
                        baselined.push(crate::trf!(
                            "{name} (веток: {branches}, DLC: {dlcs})",
                            "{name} (branches: {branches}, DLC: {dlcs})"
                        ));
                        Vec::new()
                    };
                    db.save_game(
                        key,
                        &state.name,
                        &serde_json::to_string(&state)?,
                        &events,
                        merge,
                    )
                    .await?;
                }
            }
            Err(e) => {
                let now = crate::now();
                let since = *self.pics_failing_since.get_or_insert(now);
                let interval = i64::try_from(cfg.intervals.games_minutes)? * 60;
                let threshold = u32::try_from(3600 / interval.max(1)).unwrap_or(1) + 1;
                health
                    .fail(
                        "pics",
                        &format!("{e:#}"),
                        threshold,
                        crate::tr!("ничего, бот перешёл на запасной сигнал (проверка версии через Steam Web API) и вернётся к PICS сам", "nothing, the bot switched to the fallback (version check via Steam Web API) and returns to PICS by itself"),
                    )
                    .await;
                if now - since >= 3600 {
                    self.fallback(appids, db, steam, merge).await?;
                }
            }
        }
        let news_result = Self::poll_news(cfg, appids, db, steam, merge).await;
        match news_result {
            Ok(()) => health.ok("news").await,
            Err(SteamError::BadKey) => {
                health
                    .fail(
                        "news",
                        crate::tr!("ключ Steam Web API отклонён", "Steam Web API key rejected"),
                        1,
                        crate::tr!("проверьте HQW_STEAM_API_KEY", "check HQW_STEAM_API_KEY"),
                    )
                    .await;
            }
            Err(SteamError::Other(e)) => {
                health
                    .fail(
                        "news",
                        &format!("{e:#}"),
                        3,
                        crate::tr!(
                            "проверьте доступность api.steampowered.com",
                            "check that api.steampowered.com is reachable"
                        ),
                    )
                    .await;
            }
        }
        if !baselined.is_empty() {
            let ev = Event::service(
                format!("bot:games_baseline:{}", crate::now()),
                crate::tr!("Игры взяты под наблюдение", "Games are now watched"),
                crate::trf!(
                    "Старые новости и билды не присылаются, отслеживаются только новые изменения.\n{}",
                    "Old news and builds are not sent, only new changes are tracked.\n{}",
                    baselined.join("\n")
                ),
            );
            db.enqueue(&ev).await?;
        }
        Ok(())
    }

    async fn poll_news(
        cfg: &Config,
        appids: &[u32],
        db: &Db,
        steam: &SteamHttp,
        merge: i64,
    ) -> Result<(), SteamError> {
        let feeds = cfg.games.news_feeds.join(",");
        for &appid in appids {
            let key = i64::from(appid);
            let Some(state) = db
                .game_state(key)
                .await?
                .and_then(|s| serde_json::from_str::<GameState>(&s).ok())
            else {
                continue;
            };
            let resp: NewsResponse = steam
                .api(
                    "ISteamNews/GetNewsForApp/v2/",
                    &[
                        ("appid", appid.to_string()),
                        ("count", "20".into()),
                        ("maxlength", "0".into()),
                        ("feeds", feeds.clone()),
                    ],
                    false,
                )
                .await?;
            let seen: BTreeSet<String> = db.news_seen(key).await?.into_iter().collect();
            let base_key = format!("news_baseline:{appid}");
            let first_time = db.meta_get(&base_key).await?.is_none();
            let mut gids = Vec::new();
            let mut events = Vec::new();
            for item in resp.appnews.newsitems {
                if !item.gid.bytes().all(|b| b.is_ascii_digit())
                    || item.gid.is_empty()
                    || item.gid.len() > 24
                {
                    continue;
                }
                if !cfg.games.news_feeds.contains(&item.feedname) || seen.contains(&item.gid) {
                    continue;
                }
                gids.push(item.gid.clone());
                if first_time {
                    continue;
                }
                let patch = item.tags.iter().any(|t| t == "patchnotes");
                let mut ev = Event::new(
                    Source::SteamUpdate,
                    format!("steam:news:{appid}:{}", item.gid),
                    item.date,
                    Body::GameNews {
                        game: state.name.clone(),
                        appid,
                        patch,
                        title: text::clean_line(&item.title, 300),
                        date: item.date,
                        text: text::clean(&bbcode::to_text(&item.contents)),
                        url: format!(
                            "https://store.steampowered.com/news/app/{appid}/view/{}",
                            item.gid
                        ),
                        store_url: store_url(appid),
                    },
                );
                if patch {
                    ev.merge_app = Some(appid);
                }
                events.push(ev);
            }
            events.sort_by_key(|e| e.occurred_at);
            if !gids.is_empty() || first_time {
                db.save_news(
                    key,
                    &gids,
                    &events,
                    merge,
                    first_time.then_some(base_key.as_str()),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn fallback(&self, appids: &[u32], db: &Db, steam: &SteamHttp, merge: i64) -> Result<()> {
        for &appid in appids {
            let key = i64::from(appid);
            let Some(mut state) = db
                .game_state(key)
                .await?
                .and_then(|s| serde_json::from_str::<GameState>(&s).ok())
            else {
                continue;
            };
            let Ok(resp) = steam
                .api::<UpToDate>(
                    "ISteamApps/UpToDateCheck/v1/",
                    &[("appid", appid.to_string()), ("version", "0".into())],
                    false,
                )
                .await
            else {
                continue;
            };
            let Some(version) = resp
                .response
                .required_version
                .filter(|_| resp.response.success)
            else {
                continue;
            };
            let mut events = Vec::new();
            if let Some(old) = state.fallback_version.filter(|&o| o != version) {
                events.push(Event::new(
                    Source::SteamUpdate,
                    format!("steam:version:{appid}:{version}"),
                    crate::now(),
                    Body::GameBuild {
                        game: state.name.clone(),
                        appid,
                        is_dlc: false,
                        change: BuildChange::Updated,
                        branch: crate::tr!(
                            "public (по версии Web API)",
                            "public (by Web API version)"
                        )
                        .into(),
                        old_build: Some(crate::trf!("версия {old}", "version {old}")),
                        new_build: Some(crate::trf!("версия {version}", "version {version}")),
                        updated_at: None,
                        store_url: store_url(appid),
                    },
                ));
            }
            state.fallback_version = Some(version);
            db.save_game(
                key,
                &state.name.clone(),
                &serde_json::to_string(&state)?,
                &events,
                merge,
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(branches: &[(&str, &str)], dlcs: &[(u32, bool, &str)]) -> GameState {
        GameState {
            name: "Game".into(),
            branches: branches
                .iter()
                .map(|(n, b)| {
                    (
                        (*n).to_owned(),
                        Branch {
                            buildid: (*b).to_owned(),
                            time_updated: Some(100),
                            password: false,
                        },
                    )
                })
                .collect(),
            dlcs: dlcs
                .iter()
                .map(|(id, music, gid)| {
                    (
                        *id,
                        DlcState {
                            name: format!("D{id}"),
                            music: *music,
                            content: if gid.is_empty() {
                                vec![]
                            } else {
                                vec![("1".into(), (*gid).to_owned())]
                            },
                        },
                    )
                })
                .collect(),
            change_number: 5,
            fallback_version: None,
        }
    }

    #[test]
    fn parses_real_app_info() {
        let a = parse_app_info(include_str!("../../tests/data/appinfo_572890.vdf")).unwrap();
        assert_eq!(a.name, "Pikuniku");
        assert_eq!(a.kind, "game");
        assert_eq!(a.dlc_ids, vec![1_016_520]);
        assert!(a.branches.contains_key("public"));
        assert_eq!(a.dlc_depots.get(&1_016_520).map(Vec::len), Some(1));
        let tf2 = parse_app_info(include_str!("../../tests/data/appinfo_440.vdf")).unwrap();
        assert!(tf2.branches.len() > 3);
        let dlc = parse_app_info(include_str!("../../tests/data/appinfo_615510.vdf")).unwrap();
        assert_eq!(dlc.kind, "dlc");
    }

    #[test]
    fn public_build_change() {
        let ev = diff(
            1,
            &state(&[("public", "1")], &[]),
            &state(&[("public", "2")], &[]),
        );
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].merge_app, Some(1));
        assert!(matches!(
            ev[0].body,
            Body::GameBuild {
                change: BuildChange::Updated,
                ..
            }
        ));
    }

    #[test]
    fn branches_added_and_removed() {
        let ev = diff(
            1,
            &state(&[("public", "1"), ("old", "3")], &[]),
            &state(&[("public", "1"), ("beta", "4")], &[]),
        );
        assert_eq!(ev.len(), 2);
        assert!(ev.iter().all(|e| e.merge_app.is_none()));
    }

    #[test]
    fn dlc_changes_skip_soundtracks() {
        let old = state(&[], &[(10, false, "a"), (11, false, ""), (12, true, "")]);
        let new = state(&[], &[(10, false, "b"), (13, false, ""), (14, true, "")]);
        let ev = diff(1, &old, &new);
        let kinds: Vec<_> = ev
            .iter()
            .map(|e| match &e.body {
                Body::Dlc {
                    dlc_appid, change, ..
                } => (*dlc_appid, *change),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                (10, DlcChange::ContentChanged),
                (13, DlcChange::Added),
                (11, DlcChange::Removed)
            ]
        );
    }

    #[test]
    fn no_change_no_events() {
        let s = state(&[("public", "1")], &[(10, false, "a")]);
        assert!(diff(1, &s, &s).is_empty());
    }
}
