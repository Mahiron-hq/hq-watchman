// SPDX-License-Identifier: GPL-3.0-or-later
//! SSH session to the server: host key pinning, password or key login, commands with sudo.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use russh::client::{self, AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg};
use russh::ChannelMsg;
use zeroize::Zeroizing;

#[derive(Clone)]
pub enum Auth {
    Password(Zeroizing<String>),
    Key {
        path: String,
        passphrase: Option<Zeroizing<String>>,
    },
}

#[derive(Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: Auth,
    /// Password for sudo; the login password is used when empty.
    pub sudo_password: Option<Zeroizing<String>>,
}

/// Raised when the server's host key is not pinned yet or differs from the pin.
#[derive(Debug)]
pub struct HostKeyError {
    pub fingerprint: String,
    pub changed: bool,
}

impl std::fmt::Display for HostKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.changed {
            write!(f, "host key changed: {}", self.fingerprint)
        } else {
            write!(f, "unknown host key: {}", self.fingerprint)
        }
    }
}

impl std::error::Error for HostKeyError {}

struct Checker {
    pinned: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Checker {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        let ok = match key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => {
                let fp = key.fingerprint(HashAlg::Sha256).to_string();
                let ok = self.pinned.as_deref() == Some(fp.as_str());
                *self
                    .seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fp);
                ok
            }
            russh::keys::PublicKeyOrCertificate::Certificate(_) => false,
        };
        async move { Ok(ok) }
    }
}

pub struct Session {
    handle: Handle<Checker>,
    sudo: Option<Zeroizing<String>>,
    pub is_root: bool,
}

/// Output of one remote command.
pub struct Output {
    pub status: u32,
    pub raw: Vec<u8>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    /// Short reason for an error message: last stderr lines, else stdout.
    pub fn reason(&self) -> String {
        let src = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let lines: Vec<&str> = src.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = lines.len().saturating_sub(6);
        lines.get(tail..).unwrap_or_default().join("\n")
    }
}

/// Quotes a value for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl Session {
    /// Connects; `pinned` is the expected host key fingerprint (SHA256:...).
    pub async fn connect(t: &Target, pinned: Option<String>) -> Result<Self> {
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(600)),
            keepalive_interval: Some(Duration::from_secs(30)),
            ..client::Config::default()
        });
        let seen = Arc::new(Mutex::new(None));
        let checker = Checker {
            pinned: pinned.clone(),
            seen: Arc::clone(&seen),
        };
        let addr = (t.host.as_str(), t.port);
        let connected = tokio::time::timeout(
            Duration::from_secs(20),
            client::connect(config, addr, checker),
        )
        .await
        .map_err(|_| anyhow!("connection timed out"))?;
        let mut handle = match connected {
            Ok(h) => h,
            Err(e) => {
                let fp = seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(fp) = fp.filter(|f| pinned.as_deref() != Some(f.as_str())) {
                    return Err(HostKeyError {
                        fingerprint: fp,
                        changed: pinned.is_some(),
                    }
                    .into());
                }
                return Err(anyhow!("{e}"));
            }
        };
        let ok = match &t.auth {
            Auth::Password(p) => password_login(&mut handle, &t.user, p).await?,
            Auth::Key { path, passphrase } => {
                let key = russh::keys::load_secret_key(
                    path,
                    passphrase.as_deref().map(std::string::String::as_str),
                )
                .map_err(|e| anyhow!("cannot read the key file: {e}"))?;
                let hash = handle.best_supported_rsa_hash().await?.flatten();
                handle
                    .authenticate_publickey(
                        t.user.clone(),
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await?
                    .success()
            }
        };
        if !ok {
            bail!("login rejected: wrong user name, password or key");
        }
        let sudo = t.sudo_password.clone().or_else(|| match &t.auth {
            Auth::Password(p) => Some(p.clone()),
            Auth::Key { .. } => None,
        });
        let mut s = Self {
            handle,
            sudo,
            is_root: false,
        };
        s.is_root = s.run("id -u").await?.stdout.trim() == "0";
        Ok(s)
    }

    async fn exec_raw(
        &self,
        command: &str,
        stdin: Option<&[u8]>,
        mut progress: Option<&mut (dyn FnMut(&str) + Send)>,
    ) -> Result<Output> {
        let mut ch = self.handle.channel_open_session().await?;
        ch.exec(true, command).await?;
        if let Some(data) = stdin {
            ch.data_bytes(data.to_vec()).await?;
        }
        ch.eof().await?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut status = None;
        let mut pending = String::new();
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    out.extend_from_slice(&data);
                    if let Some(p) = progress.as_deref_mut() {
                        pending.push_str(&String::from_utf8_lossy(&data));
                        while let Some(i) = pending.find('\n') {
                            let line: String = pending.drain(..=i).collect();
                            p(line.trim_end());
                        }
                    }
                }
                ChannelMsg::ExtendedData { data, .. } => err.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                _ => {}
            }
        }
        if let (Some(p), false) = (progress, pending.trim().is_empty()) {
            p(pending.trim_end());
        }
        Ok(Output {
            status: status.unwrap_or(255),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            raw: out,
            stderr: String::from_utf8_lossy(&err).into_owned(),
        })
    }

    /// Runs a command as the login user.
    pub async fn run(&self, command: &str) -> Result<Output> {
        self.exec_raw(command, None, None).await
    }

    /// Runs a bash script as root (directly or through sudo), streaming stdout lines.
    pub async fn root_script(
        &self,
        script: &str,
        progress: Option<&mut (dyn FnMut(&str) + Send)>,
    ) -> Result<Output> {
        let body = format!(
            "set -euo pipefail\nexport DEBIAN_FRONTEND=noninteractive LC_ALL=C\n{script}\n"
        );
        if self.is_root {
            return self
                .exec_raw("bash -s", Some(body.as_bytes()), progress)
                .await;
        }
        let pw = self
            .sudo
            .as_ref()
            .ok_or_else(|| anyhow!("sudo password is needed"))?;
        let mut input = Zeroizing::new(Vec::with_capacity(pw.len() + body.len() + 1));
        input.extend_from_slice(pw.as_bytes());
        input.push(b'\n');
        input.extend_from_slice(body.as_bytes());
        let out = self
            .exec_raw("sudo -S -p '' bash -s", Some(&input), progress)
            .await?;
        if !out.ok() && out.stderr.contains("incorrect password") {
            bail!("sudo rejected the password");
        }
        Ok(out)
    }

    /// Writes a file as root with the given mode; the content goes through stdin.
    pub async fn put_file(&self, path: &str, mode: &str, data: &[u8]) -> Result<()> {
        let dir = std::path::Path::new(path)
            .parent()
            .map_or_else(|| "/".into(), |p| p.to_string_lossy().replace('\\', "/"));
        let cmd = format!(
            "{{ [ -d {d} ] || install -d -m 755 {d}; }} && umask 077 && cat > {p}.new && chmod {m} {p}.new && mv -f {p}.new {p}",
            d = sh_quote(&dir),
            p = sh_quote(path),
            m = mode
        );
        let out = if self.is_root {
            self.exec_raw(&format!("sh -c {}", sh_quote(&cmd)), Some(data), None)
                .await?
        } else {
            let pw = self
                .sudo
                .as_ref()
                .ok_or_else(|| anyhow!("sudo password is needed"))?;
            let mut input = Zeroizing::new(Vec::with_capacity(pw.len() + data.len() + 1));
            input.extend_from_slice(pw.as_bytes());
            input.push(b'\n');
            input.extend_from_slice(data);
            self.exec_raw(
                &format!("sudo -S -p '' sh -c {}", sh_quote(&cmd)),
                Some(&input),
                None,
            )
            .await?
        };
        if !out.ok() {
            bail!("cannot write {path}: {}", out.reason());
        }
        Ok(())
    }

    /// Checks that sudo works before any change is made.
    pub async fn check_root(&self) -> Result<()> {
        let out = self.root_script("id -u", None).await.context("sudo")?;
        if out.stdout.trim() != "0" {
            bail!("no root access: {}", out.reason());
        }
        Ok(())
    }
}

async fn password_login(handle: &mut Handle<Checker>, user: &str, pw: &str) -> Result<bool> {
    match handle.authenticate_password(user, pw).await? {
        AuthResult::Success => return Ok(true),
        AuthResult::Failure { .. } => {}
    }
    let mut reply = handle
        .authenticate_keyboard_interactive_start(user, None)
        .await?;
    for _ in 0..5 {
        match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts
                    .iter()
                    .map(|p| if p.echo { String::new() } else { pw.to_owned() })
                    .collect();
                reply = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn reason_takes_the_tail() {
        let o = Output {
            status: 1,
            raw: Vec::new(),
            stdout: "x".into(),
            stderr: "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n".into(),
        };
        assert_eq!(o.reason(), "5\n6\n7\n8\n9\n10");
    }
}
