// SPDX-License-Identifier: GPL-3.0-or-later
//! Local app data: saved servers (no passwords), pinned host keys, preferences.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub target: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Data {
    pub language: String,
    pub profiles: Vec<Profile>,
    pub known_hosts: BTreeMap<String, String>,
}

fn dir() -> Result<PathBuf> {
    let d = directories::ProjectDirs::from("online", "Mahiron", "HQ Watchman Setup")
        .ok_or_else(|| anyhow!("no home directory"))?;
    Ok(d.config_dir().to_path_buf())
}

fn file() -> Result<PathBuf> {
    Ok(dir()?.join("setup.json"))
}

pub fn load() -> Data {
    file()
        .ok()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(d: &Data) -> Result<()> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join("setup.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(d)?)?;
    std::fs::rename(&tmp, file()?)?;
    Ok(())
}

pub fn host_key(d: &Data, host: &str, port: u16) -> Option<String> {
    d.known_hosts.get(&format!("{host}:{port}")).cloned()
}

pub fn pin_host(host: &str, port: u16, fingerprint: &str) -> Result<()> {
    let mut d = load();
    d.known_hosts
        .insert(format!("{host}:{port}"), fingerprint.to_owned());
    save(&d)
}

pub fn remember(p: Profile) -> Result<()> {
    let mut d = load();
    d.profiles
        .retain(|x| !(x.host == p.host && x.port == p.port && x.user == p.user));
    d.profiles.insert(0, p);
    d.profiles.truncate(20);
    save(&d)
}

pub fn forget(host: &str, port: u16) -> Result<()> {
    let mut d = load();
    d.profiles.retain(|x| !(x.host == host && x.port == port));
    save(&d)
}

pub fn set_language(code: &str) {
    let mut d = load();
    code.clone_into(&mut d.language);
    let _ = save(&d);
}
