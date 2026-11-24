// SPDX-License-Identifier: GPL-3.0-or-later
//! Steam store lookups for adding games: search by name, name by `AppID`, `AppID` from a link.

use anyhow::Result;
use serde::Deserialize;

use crate::net::HttpClient;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub appid: u32,
    pub name: String,
}

/// `AppID` from a plain number or a store or community link.
pub fn parse_appid(input: &str) -> Option<u32> {
    let t = input.trim();
    if let Ok(id) = t.parse::<u32>() {
        return (id > 0).then_some(id);
    }
    for marker in ["store.steampowered.com/app/", "steamcommunity.com/app/"] {
        if let Some(pos) = t.find(marker) {
            let rest = t.get(pos + marker.len()..)?;
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            return digits.parse::<u32>().ok().filter(|id| *id > 0);
        }
    }
    None
}

#[derive(Deserialize)]
struct SearchReply {
    #[serde(default)]
    items: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    id: u32,
    name: String,
}

pub async fn search(http: &HttpClient, term: &str) -> Result<Vec<Found>> {
    let term: String = term.trim().chars().take(100).collect();
    let req = http
        .request(
            reqwest::Method::GET,
            "https://store.steampowered.com/api/storesearch/",
        )?
        .query(&[("term", term.as_str()), ("l", "english"), ("cc", "US")])
        .timeout(std::time::Duration::from_secs(20));
    let body = http.send(req, 1024 * 1024).await?;
    let reply: SearchReply = serde_json::from_slice(&body)?;
    Ok(reply
        .items
        .into_iter()
        .filter(|i| i.id > 0)
        .take(8)
        .map(|i| Found {
            appid: i.id,
            name: crate::text::clean_line(&i.name, 100),
        })
        .collect())
}

#[derive(Deserialize)]
struct Details {
    success: bool,
    #[serde(default)]
    data: Option<DetailsData>,
}

#[derive(Deserialize)]
struct DetailsData {
    name: String,
}

/// Store name of an app, or `None` when Steam does not know the `AppID`.
pub async fn name(http: &HttpClient, appid: u32) -> Result<Option<String>> {
    let req = http
        .request(
            reqwest::Method::GET,
            "https://store.steampowered.com/api/appdetails",
        )?
        .query(&[("appids", appid.to_string()), ("filters", "basic".into())])
        .timeout(std::time::Duration::from_secs(20));
    let body = http.send(req, 4 * 1024 * 1024).await?;
    let map: std::collections::HashMap<String, Details> = serde_json::from_slice(&body)?;
    Ok(map
        .into_values()
        .next()
        .filter(|d| d.success)
        .and_then(|d| d.data)
        .map(|d| crate::text::clean_line(&d.name, 100)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appid_from_input() {
        assert_eq!(parse_appid("572890"), Some(572_890));
        assert_eq!(
            parse_appid("https://store.steampowered.com/app/2523120/Some_Game/"),
            Some(2_523_120)
        );
        assert_eq!(parse_appid("steamcommunity.com/app/588430"), Some(588_430));
        assert_eq!(parse_appid("0"), None);
        assert_eq!(parse_appid("Half-Life"), None);
        assert_eq!(parse_appid("https://store.steampowered.com/app/x"), None);
    }
}
