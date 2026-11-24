// SPDX-License-Identifier: GPL-3.0-or-later
//! What the user chose, and the files generated from it.

use std::fmt::Write as _;

use anyhow::{bail, Result};
use hq_watchman::i18n::Lang;
use hq_watchman::settings::{Game, Settings};
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Docker,
    Systemd,
    Windows,
}

impl Target {
    pub fn code(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Systemd => "systemd",
            Self::Windows => "windows",
        }
    }

    pub fn from_code(s: &str) -> Option<Self> {
        match s {
            "docker" => Some(Self::Docker),
            "systemd" => Some(Self::Systemd),
            "windows" => Some(Self::Windows),
            _ => None,
        }
    }

    pub fn is_remote(self) -> bool {
        !matches!(self, Self::Windows)
    }
}

/// How the webhook reaches the bot from the internet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proxy {
    /// Caddy container next to the bot, ports 80 and 443.
    Caddy,
    /// nginx already on the server, site and certificate added to it.
    Nginx,
}

#[derive(Clone, Default)]
pub struct GithubPlan {
    pub app_id: u64,
    pub slug: String,
    pub pem: Zeroizing<String>,
    pub webhook_secret: Zeroizing<String>,
    pub user_id: u64,
    pub webhook: bool,
    pub domain: String,
    pub hook_path: String,
}

#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Plan {
    pub target: Target,
    pub language: Lang,
    pub github_on: bool,
    pub guides_on: bool,
    pub games_on: bool,
    pub telegram_token: Zeroizing<String>,
    pub telegram_admin: i64,
    pub github: Option<GithubPlan>,
    pub steam_profile: Option<u64>,
    pub steam_key: Option<Zeroizing<String>>,
    pub games: Vec<Game>,
    pub timezone: String,
    pub quiet_enabled: bool,
    pub quiet_start: String,
    pub quiet_end: String,
    pub proxy: Option<Proxy>,
}

/// Paths of one installation kind.
pub struct Layout {
    pub config: &'static str,
    pub secrets: &'static str,
    pub database: &'static str,
}

pub const DOCKER_DIR: &str = "/opt/hq-watchman";
pub const SYSTEMD_ETC: &str = "/etc/hq-watchman";
pub const WINDOWS_DIR: &str = r"C:\ProgramData\HQ Watchman";

pub fn layout(t: Target) -> Layout {
    match t {
        Target::Docker => Layout {
            config: "/opt/hq-watchman/config.toml",
            secrets: "/opt/hq-watchman/.env",
            database: "/data/hq-watchman.db",
        },
        Target::Systemd => Layout {
            config: "/etc/hq-watchman/config.toml",
            secrets: "/etc/hq-watchman/secrets.env",
            database: "/var/lib/hq-watchman/hq-watchman.db",
        },
        Target::Windows => Layout {
            config: r"C:\ProgramData\HQ Watchman\config.toml",
            secrets: r"C:\ProgramData\HQ Watchman\secrets.env",
            database: r"C:\ProgramData\HQ Watchman\data\hq-watchman.db",
        },
    }
}

/// Random string of `[A-Za-z0-9]`.
pub fn random_token(len: usize) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..len)
        .map(|_| {
            let i = rand::random_range(0..ABC.len());
            char::from(ABC.get(i).copied().unwrap_or(b'x'))
        })
        .collect()
}

/// Lower-case DNS name check.
pub fn valid_domain(d: &str) -> bool {
    let d = d.trim();
    d.len() <= 253
        && d.contains('.')
        && d.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

impl Plan {
    pub fn github_webhook(&self) -> bool {
        self.github.as_ref().is_some_and(|g| g.webhook)
    }

    /// Infrastructure config file for the chosen target.
    pub fn config_toml(&self) -> String {
        let l = layout(self.target);
        let mut s = String::from(
            "# HQ Watchman config, written by HQ Watchman Setup.\n# Notification settings live in the database: /settings in Telegram or the setup app.\n\n",
        );
        let _ = writeln!(s, "language = {}", toml_str(self.language.code()));
        let mode = if self.github_webhook() {
            "webhook"
        } else {
            "polling"
        };
        let _ = write!(s, "\n[github]\nmode = {}\n", toml_str(mode));
        if let Some(g) = self.github.as_ref().filter(|g| g.webhook) {
            let (listen, trusted) = match (self.target, self.proxy) {
                (Target::Docker, Some(Proxy::Caddy)) => ("0.0.0.0:8080", "172.31.250.0/24"),
                (Target::Docker, _) => ("0.0.0.0:8080", "172.31.250.1"),
                _ => ("127.0.0.1:8080", "127.0.0.1"),
            };
            let _ = write!(
                s,
                "\n[webhook]\nlisten = {}\npath = {}\ntrusted_proxies = [{}]\n",
                toml_str(listen),
                toml_str(&g.hook_path),
                toml_str(trusted)
            );
        }
        let _ = write!(s, "\n[storage]\npath = {}\n", toml_str(l.database));
        s
    }

    /// Secrets file, `KEY=VALUE` lines; the key PEM is kept on one line with `\n`.
    pub fn secrets_env(&self) -> Zeroizing<String> {
        let mut s = Zeroizing::new(String::new());
        let _ = writeln!(s, "HQW_TELEGRAM_TOKEN={}", self.telegram_token.as_str());
        let _ = writeln!(s, "HQW_TELEGRAM_ADMIN_ID={}", self.telegram_admin);
        if let Some(g) = &self.github {
            let _ = writeln!(s, "HQW_GITHUB_APP_ID={}", g.app_id);
            let pem = Zeroizing::new(g.pem.trim().replace("\r\n", "\n").replace('\n', "\\n"));
            let _ = writeln!(s, "HQW_GITHUB_APP_KEY={}", pem.as_str());
            let _ = writeln!(s, "HQW_GITHUB_ADMIN_ID={}", g.user_id);
            if g.webhook {
                let _ = writeln!(s, "HQW_GITHUB_WEBHOOK_SECRET={}", g.webhook_secret.as_str());
            }
        }
        if let Some(p) = self.steam_profile {
            let _ = writeln!(s, "HQW_STEAM_PROFILE_ID={p}");
        }
        if let Some(k) = &self.steam_key {
            let _ = writeln!(s, "HQW_STEAM_API_KEY={}", k.as_str());
        }
        s
    }

    pub fn settings(&self) -> Settings {
        let mut s = Settings {
            language: self.language,
            timezone: self.timezone.clone(),
            ..Settings::default()
        };
        s.quiet_hours.enabled = self.quiet_enabled;
        s.quiet_hours.start.clone_from(&self.quiet_start);
        s.quiet_hours.end.clone_from(&self.quiet_end);
        s.github.enabled = self.github_on && self.github.is_some();
        s.guides.enabled = self.guides_on && self.steam_profile.is_some();
        s.games.enabled = self.games_on;
        s.games.list.clone_from(&self.games);
        s
    }

    pub fn validate(&self) -> Result<()> {
        if !hq_watchman::config::valid_telegram_token(&self.telegram_token) {
            bail!("Telegram token");
        }
        if self.telegram_admin <= 0 {
            bail!("Telegram admin ID");
        }
        if self.github_on && self.github.is_none() {
            bail!("GitHub App");
        }
        if self.guides_on && self.steam_profile.is_none() {
            bail!("Steam profile");
        }
        if let Some(g) = self.github.as_ref().filter(|g| g.webhook) {
            if !valid_domain(&g.domain) {
                bail!("domain");
            }
            hq_watchman::config::check_webhook_secret(&g.webhook_secret)?;
        }
        if self.github_webhook() && self.target == Target::Windows {
            bail!("webhook mode needs a server");
        }
        self.settings().validate()?;
        let cfg = hq_watchman::config::Config::parse(&self.config_toml())?;
        let env = self.secrets_env();
        let map: Vec<(String, String)> = env
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let secrets = hq_watchman::config::Secrets::from_map(|n| {
            map.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str())
        })?;
        if secrets.github.is_some()
            && cfg.github.mode == hq_watchman::config::GithubMode::Webhook
            && cfg.webhook.is_none()
        {
            bail!("webhook section missing");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan {
            target: Target::Docker,
            language: Lang::En,
            github_on: false,
            guides_on: false,
            games_on: true,
            telegram_token: Zeroizing::new("123456:ABCdefGHIjklMNOpqrSTUvwxYZ0123456789_-".into()),
            telegram_admin: 42,
            github: None,
            steam_profile: None,
            steam_key: None,
            games: vec![Game::new(570, "Dota 2")],
            timezone: "Europe/Berlin".into(),
            quiet_enabled: true,
            quiet_start: "23:00".into(),
            quiet_end: "07:00".into(),
            proxy: None,
        }
    }

    #[test]
    fn games_only_is_valid_and_parses_in_the_bot() {
        let p = plan();
        p.validate().unwrap();
        let cfg = hq_watchman::config::Config::parse(&p.config_toml()).unwrap();
        assert!(cfg.webhook.is_none());
        assert_eq!(cfg.github.mode, hq_watchman::config::GithubMode::Polling);
        assert!(!p.secrets_env().contains("GITHUB"));
        let s = p.settings();
        assert!(!s.github.enabled && !s.guides.enabled && s.games.enabled);
    }

    #[test]
    fn webhook_plan_writes_section_and_secret() {
        let mut p = plan();
        p.github_on = true;
        p.proxy = Some(Proxy::Caddy);
        p.github = Some(GithubPlan {
            app_id: 7,
            slug: "x".into(),
            pem: Zeroizing::new(String::new()),
            webhook_secret: Zeroizing::new(random_token(48)),
            user_id: 9,
            webhook: true,
            domain: "hooks.example.org".into(),
            hook_path: random_token(32),
        });
        let cfg = hq_watchman::config::Config::parse(&p.config_toml()).unwrap();
        let w = cfg.webhook.unwrap();
        assert_eq!(w.listen.port(), 8080);
        assert_eq!(w.trusted_proxies.len(), 1);
        assert!(p.secrets_env().contains("HQW_GITHUB_WEBHOOK_SECRET="));
        assert!(p.validate().is_err(), "empty PEM must be rejected");
    }

    #[test]
    fn domains() {
        assert!(valid_domain("hooks.example.org"));
        assert!(!valid_domain("localhost"));
        assert!(!valid_domain("bad_name.org"));
        assert!(!valid_domain("-a.org"));
    }
}
