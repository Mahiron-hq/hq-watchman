// SPDX-License-Identifier: GPL-3.0-or-later
//! Release files: from GitHub Releases or a local folder, always checked by signature and hash.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

pub const REPO: &str = "Mahiron-hq/hq-watchman";
/// minisign public key of the release signing key.
pub const PUBLIC_KEY: &str = "RWRh/ArZd0sNbyZPIKim+NSoQfeD97vdcfgJB4GQGpeZ8fIl4yWIrryu";
const SUMS: &str = "SHA256SUMS.txt";
const MAX_ASSET: usize = 200 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum Source {
    Release {
        version: String,
        assets: Vec<(String, String)>,
    },
    Folder(PathBuf),
}

/// Verified checksum list of one release.
pub struct Bundle {
    pub version: String,
    source: Source,
    sums: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("hq-watchman-setup/{}", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_mins(15))
        .build()?)
}

async fn download(url: &str) -> Result<Vec<u8>> {
    let host_ok = url.starts_with("https://github.com/")
        || url.starts_with("https://api.github.com/")
        || url.starts_with("https://objects.githubusercontent.com/")
        || url.starts_with("https://release-assets.githubusercontent.com/");
    if !host_ok {
        bail!("unexpected download address");
    }
    let mut resp = client()?.get(url).send().await?.error_for_status()?;
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        out.extend_from_slice(&chunk);
        if out.len() > MAX_ASSET {
            bail!("file too large");
        }
    }
    Ok(out)
}

/// Newest published release on GitHub.
pub async fn latest() -> Result<Source> {
    let r: ApiRelease = client()?
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()
        .context("no published release found")?
        .json()
        .await?;
    if r.draft || r.prerelease {
        bail!("no stable release");
    }
    Ok(Source::Release {
        version: r.tag_name.trim_start_matches('v').to_owned(),
        assets: r
            .assets
            .into_iter()
            .map(|a| (a.name, a.browser_download_url))
            .collect(),
    })
}

/// A folder with release files, version taken from the checksum list.
pub fn folder(dir: PathBuf) -> Source {
    Source::Folder(dir)
}

async fn raw(source: &Source, name: &str) -> Result<Vec<u8>> {
    match source {
        Source::Release { assets, .. } => {
            let url = assets
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, u)| u.clone())
                .ok_or_else(|| anyhow!("the release has no file {name}"))?;
            download(&url).await
        }
        Source::Folder(dir) => {
            let p = dir.join(name);
            tokio::fs::read(&p)
                .await
                .with_context(|| format!("cannot read {}", p.display()))
        }
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, data);
    d.as_ref().iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn parse_sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let hash = it.next()?;
            let name = it.next()?.trim_start_matches('*');
            (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| (name.to_owned(), hash.to_ascii_lowercase()))
        })
        .collect()
}

/// Version from asset names like `hq-watchman-0.1.0-linux-amd64.tar.gz`.
fn version_of(sums: &[(String, String)]) -> Option<String> {
    sums.iter().find_map(|(n, _)| {
        let rest = n.strip_prefix("hq-watchman-")?;
        let v = rest.split('-').next()?;
        (v.split('.').count() == 3 && v.split('.').all(|p| p.parse::<u32>().is_ok()))
            .then(|| v.to_owned())
    })
}

impl Bundle {
    /// Loads the checksum list and checks its minisign signature.
    pub async fn open(source: Source) -> Result<Self> {
        let sums = raw(&source, SUMS).await?;
        let sig = raw(&source, &format!("{SUMS}.minisig")).await?;
        let pk = minisign_verify::PublicKey::from_base64(PUBLIC_KEY)
            .map_err(|e| anyhow!("public key: {e}"))?;
        let sig = minisign_verify::Signature::decode(&String::from_utf8_lossy(&sig))
            .map_err(|e| anyhow!("signature file: {e}"))?;
        pk.verify(&sums, &sig, false)
            .map_err(|_| anyhow!("the release signature is not valid"))?;
        let sums = parse_sums(&String::from_utf8_lossy(&sums));
        let version = match &source {
            Source::Release { version, .. } => version.clone(),
            Source::Folder(_) => {
                version_of(&sums).ok_or_else(|| anyhow!("no version in {SUMS}"))?
            }
        };
        Ok(Self {
            version,
            source,
            sums,
        })
    }

    /// File content after its hash matched the signed list.
    pub async fn fetch(&self, name: &str) -> Result<(Vec<u8>, String)> {
        let expected = self
            .sums
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, h)| h.clone())
            .ok_or_else(|| anyhow!("{name} is not in the signed list"))?;
        let data = raw(&self.source, name).await?;
        let got = sha256_hex(&data);
        if got != expected {
            bail!("{name}: checksum mismatch");
        }
        Ok((data, expected))
    }
}

pub fn docker_asset(version: &str, arch: &str) -> String {
    format!("hq-watchman-{version}-docker-{arch}.tar.gz")
}

pub fn linux_asset(version: &str, arch: &str) -> String {
    format!("hq-watchman-{version}-linux-{arch}.tar.gz")
}

pub fn windows_asset(version: &str) -> String {
    format!("hq-watchman-{version}-windows-x64.exe")
}

/// Compares `a.b.c` versions.
pub fn newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> {
        v.trim_start_matches('v')
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parse(candidate) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_and_versions() {
        let s = parse_sums(&format!(
            "{}  hq-watchman-1.2.3-linux-amd64.tar.gz\nbad line\n{} *SBOM.json\n",
            "a".repeat(64),
            "B".repeat(64)
        ));
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].1, "b".repeat(64));
        assert_eq!(version_of(&s).as_deref(), Some("1.2.3"));
        assert!(newer("0.2.0", "0.1.9"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(minisign_verify::PublicKey::from_base64(PUBLIC_KEY).is_ok());
    }
}
