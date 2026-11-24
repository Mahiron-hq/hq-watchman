// SPDX-License-Identifier: GPL-3.0-or-later
//! Installation, update and maintenance on a Linux server over SSH (Docker or systemd).

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use hq_watchman::{tr, trf};

use crate::plan::{Plan, Proxy, Target, DOCKER_DIR, SYSTEMD_ETC};
use crate::release::{self, Bundle};
use crate::ssh::{sh_quote, Session};

pub type Log = Arc<dyn Fn(String) + Send + Sync>;

const CADDY_IMAGE: &str =
    "caddy:2.11.8-alpine@sha256:f6228111e5fafa1a5238258d0858e103f10599b8d1659a146e7726681a002bb9";
const UNIT: &str = "/etc/systemd/system/hq-watchman.service";
const BIN: &str = "/usr/local/bin/hq-watchman";
const DATA: &str = "/var/lib/hq-watchman";

#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub pretty: String,
    pub os_id: String,
    pub arch: String,
    pub docker: bool,
    pub nginx: bool,
    pub busy_web_ports: bool,
    pub installed: Option<String>,
    pub ip: String,
}

/// Reads OS, architecture, Docker, nginx, ports and an existing installation.
pub async fn inspect(s: &Session) -> Result<ServerInfo> {
    let script = r#"
. /etc/os-release 2>/dev/null || true
echo "id=${ID:-unknown}"
echo "pretty=${PRETTY_NAME:-unknown}"
echo "arch=$(uname -m)"
command -v docker >/dev/null 2>&1 && echo docker=1 || true
systemctl is-active --quiet nginx 2>/dev/null && echo nginx=1 || true
ss -ltnH 2>/dev/null | awk '{print $4}' | grep -Eq '[:.](80|443)$' && echo busy=1 || true
test -f /opt/hq-watchman/compose.yaml && echo installed=docker || true
test -f /etc/systemd/system/hq-watchman.service && echo installed=systemd || true
echo "ip=$(hostname -I 2>/dev/null | awk '{print $1}')"
"#;
    let out = s.root_script(script, None).await?;
    if !out.ok() {
        bail!("{}", out.reason());
    }
    let mut i = ServerInfo::default();
    for line in out.stdout.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim().to_owned();
        match k {
            "id" => i.os_id = v,
            "pretty" => i.pretty = v,
            "arch" => {
                i.arch = match v.as_str() {
                    "x86_64" | "amd64" => "amd64".into(),
                    "aarch64" | "arm64" => "arm64".into(),
                    other => other.to_owned(),
                }
            }
            "docker" => i.docker = true,
            "nginx" => i.nginx = true,
            "busy" => i.busy_web_ports = true,
            "installed" => i.installed = Some(v),
            "ip" => i.ip = v,
            _ => {}
        }
    }
    Ok(i)
}

pub fn supported(i: &ServerInfo) -> Result<()> {
    if !matches!(i.os_id.as_str(), "debian" | "ubuntu") {
        bail!(
            "{}",
            trf!(
                "Поддерживаются Debian и Ubuntu, на сервере: {}",
                "Debian and Ubuntu are supported, the server runs: {}",
                i.pretty
            )
        );
    }
    if !matches!(i.arch.as_str(), "amd64" | "arm64") {
        bail!(
            "{}",
            trf!(
                "Процессор {} не поддерживается (нужен x86_64 или arm64)",
                "CPU {} is not supported (x86_64 or arm64 needed)",
                i.arch
            )
        );
    }
    Ok(())
}

async fn step(s: &Session, log: &Log, title: &str, script: &str) -> Result<String> {
    log(format!("› {title}"));
    let l = Arc::clone(log);
    let mut sink = move |line: &str| {
        if !line.trim().is_empty() {
            l(format!("  {line}"));
        }
    };
    let out = s.root_script(script, Some(&mut sink)).await?;
    if !out.ok() {
        bail!("{title}: {}", out.reason());
    }
    Ok(out.stdout)
}

const DOCKER_INSTALL: &str = r#"
. /etc/os-release
apt-get update -qq
apt-get install -y -qq ca-certificates curl >/dev/null
install -m 0755 -d /etc/apt/keyrings
curl -fsSL "https://download.docker.com/linux/${ID}/gpg" -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/${ID} ${VERSION_CODENAME} stable" > /etc/apt/sources.list.d/docker.list
apt-get update -qq
apt-get install -y -qq docker-ce docker-ce-cli containerd.io docker-compose-plugin >/dev/null
systemctl enable --now docker
docker --version
"#;

const CADDY_APT: &str = r"
apt-get update -qq
apt-get install -y -qq debian-keyring debian-archive-keyring apt-transport-https curl gnupg >/dev/null
curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor --yes -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' -o /etc/apt/sources.list.d/caddy-stable.list
apt-get update -qq
apt-get install -y -qq caddy >/dev/null
caddy version
";

fn compose(version: &str, proxy: Option<Proxy>, webhook: bool) -> String {
    let mut s = format!(
        r"name: hq-watchman

services:
  hq-watchman:
    image: hq-watchman:{version}
    container_name: hq-watchman
    restart: unless-stopped
    env_file:
      - path: ./.env
        required: true
    volumes:
      - data:/data
      - ./config.toml:/config/config.toml:ro
"
    );
    if webhook && proxy == Some(Proxy::Nginx) {
        s.push_str("    ports:\n      - \"127.0.0.1:8080:8080\"\n");
    }
    s.push_str(
        r#"    networks:
      - hqw
    user: "65532:65532"
    read_only: true
    tmpfs:
      - /tmp:size=64m,mode=1777,noexec,nosuid,nodev
    cap_drop:
      - ALL
    security_opt:
      - no-new-privileges:true
    mem_limit: 128m
    memswap_limit: 128m
    pids_limit: 64
    stop_grace_period: 15s
    logging:
      driver: json-file
      options:
        max-size: 10m
        max-file: "5"
"#,
    );
    if webhook && proxy == Some(Proxy::Caddy) {
        let _ = write!(
            s,
            r#"
  caddy:
    image: {CADDY_IMAGE}
    container_name: hq-watchman-caddy
    restart: unless-stopped
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy_data:/data
      - caddy_config:/config
    networks:
      - hqw
    cap_drop:
      - ALL
    cap_add:
      - NET_BIND_SERVICE
    security_opt:
      - no-new-privileges:true
    mem_limit: 128m
    logging:
      driver: json-file
      options:
        max-size: 10m
        max-file: "3"
"#
        );
    }
    s.push_str(
        r"
networks:
  hqw:
    ipam:
      config:
        - subnet: 172.31.250.0/24
          gateway: 172.31.250.1

volumes:
  data: {}
",
    );
    if webhook && proxy == Some(Proxy::Caddy) {
        s.push_str("  caddy_data: {}\n  caddy_config: {}\n");
    }
    s
}

fn caddyfile(domain: &str, path: &str, upstream: &str) -> String {
    format!(
        "{domain} {{\n\thandle /{path} {{\n\t\treverse_proxy {upstream}\n\t}}\n\thandle {{\n\t\trespond 404\n\t}}\n}}\n"
    )
}

fn nginx_site(domain: &str, path: &str, https: bool) -> String {
    let acme = "    location /.well-known/acme-challenge/ { root /var/www/acme; }\n";
    if !https {
        return format!(
            "server {{\n    listen 80;\n    listen [::]:80;\n    server_name {domain};\n{acme}    location / {{ return 404; }}\n}}\n"
        );
    }
    format!(
        r"server {{
    listen 80;
    listen [::]:80;
    server_name {domain};
{acme}    location / {{ return 301 https://$host$request_uri; }}
}}

server {{
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name {domain};
    ssl_certificate /etc/letsencrypt/live/{domain}/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/{domain}/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    client_max_body_size 6m;
    location = /{path} {{
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_read_timeout 30s;
    }}
    location / {{ return 404; }}
}}
"
    )
}

/// nginx site plus a Let's Encrypt certificate, with a config backup and `nginx -t` before reload.
async fn setup_nginx(s: &Session, log: &Log, domain: &str, path: &str) -> Result<()> {
    let name = format!("hq-watchman-{domain}");
    let site = format!("/etc/nginx/sites-available/{name}");
    step(
        s,
        log,
        tr!("Резервная копия настроек nginx и certbot", "nginx config backup and certbot"),
        "tar czf \"/root/nginx-backup-$(date +%Y%m%d-%H%M%S).tar.gz\" -C /etc nginx\ncommand -v certbot >/dev/null || (apt-get update -qq && apt-get install -y -qq certbot >/dev/null)\ninstall -d -m 755 /var/www/acme",
    )
    .await?;
    s.put_file(&site, "644", nginx_site(domain, path, false).as_bytes())
        .await?;
    step(
        s,
        log,
        tr!("Получение сертификата", "Getting the certificate"),
        &format!(
            "ln -sf {site} /etc/nginx/sites-enabled/{name}\nnginx -t\nsystemctl reload nginx\ncertbot certonly --webroot -w /var/www/acme -d {d} --non-interactive --agree-tos --register-unsafely-without-email --keep-until-expiring -q",
            site = sh_quote(&site),
            name = sh_quote(&name),
            d = sh_quote(domain)
        ),
    )
    .await?;
    s.put_file(&site, "644", nginx_site(domain, path, true).as_bytes())
        .await?;
    step(
        s,
        log,
        tr!("Включение сайта nginx", "Enabling the nginx site"),
        "nginx -t\nsystemctl reload nginx\nmkdir -p /etc/letsencrypt/renewal-hooks/deploy\nprintf '#!/bin/sh\\nsystemctl reload nginx\\n' > /etc/letsencrypt/renewal-hooks/deploy/reload-nginx.sh\nchmod 755 /etc/letsencrypt/renewal-hooks/deploy/reload-nginx.sh",
    )
    .await?;
    s.put_file(&format!("{DOCKER_DIR}/.nginx-site"), "600", name.as_bytes())
        .await
        .ok();
    Ok(())
}

async fn upload_asset(s: &Session, log: &Log, bundle: &Bundle, name: &str, to: &str) -> Result<()> {
    log(format!("› {} {name}", tr!("Загрузка", "Uploading")));
    let (data, _) = bundle.fetch(name).await?;
    log(format!(
        "  {} ({:.1} MB)",
        tr!(
            "подпись и контрольная сумма верны",
            "signature and checksum are valid"
        ),
        f64::from(u32::try_from(data.len()).unwrap_or(u32::MAX)) / 1_048_576.0
    ));
    s.put_file(to, "600", &data).await
}

/// Full Docker installation; data of an earlier installation is kept.
pub async fn install_docker(
    s: &Session,
    info: &ServerInfo,
    plan: &Plan,
    bundle: &Bundle,
    log: &Log,
) -> Result<()> {
    s.check_root().await?;
    if !info.docker {
        step(
            s,
            log,
            tr!("Установка Docker", "Installing Docker"),
            DOCKER_INSTALL,
        )
        .await?;
    }
    let v = &bundle.version;
    upload_asset(
        s,
        log,
        bundle,
        &release::docker_asset(v, &info.arch),
        "/tmp/hqw-image.tar.gz",
    )
    .await?;
    step(
        s,
        log,
        tr!("Загрузка образа в Docker", "Loading the image into Docker"),
        "docker load -i /tmp/hqw-image.tar.gz\nrm -f /tmp/hqw-image.tar.gz",
    )
    .await?;
    let webhook = plan.github_webhook();
    let gh = plan.github.as_ref();
    let dir = DOCKER_DIR;
    s.put_file(
        &format!("{dir}/config.toml"),
        "644",
        plan.config_toml().as_bytes(),
    )
    .await?;
    s.put_file(&format!("{dir}/.env"), "600", plan.secrets_env().as_bytes())
        .await?;
    s.put_file(
        &format!("{dir}/compose.yaml"),
        "644",
        compose(v, plan.proxy, webhook).as_bytes(),
    )
    .await?;
    if let (true, Some(Proxy::Caddy), Some(g)) = (webhook, plan.proxy, gh) {
        s.put_file(
            &format!("{dir}/Caddyfile"),
            "644",
            caddyfile(&g.domain, &g.hook_path, "hq-watchman:8080").as_bytes(),
        )
        .await?;
    }
    let settings = serde_json::to_vec(&plan.settings())?;
    s.put_file(&format!("{dir}/settings.json"), "644", &settings)
        .await?;
    step(
        s,
        log,
        tr!("Начальные настройки", "Initial settings"),
        &format!(
            "cd {dir}\nchmod 750 {dir}\ndocker compose stop hq-watchman >/dev/null 2>&1 || true\ndocker compose run --rm -T --no-deps hq-watchman settings set --config /config/config.toml < settings.json\nrm -f settings.json"
        ),
    )
    .await?;
    if let (true, Some(Proxy::Nginx), Some(g)) = (webhook, plan.proxy, gh) {
        setup_nginx(s, log, &g.domain, &g.hook_path).await?;
    }
    step(
        s,
        log,
        tr!("Запуск", "Starting"),
        &format!("cd {dir}\ndocker compose up -d --remove-orphans"),
    )
    .await?;
    wait_healthy(s, log, Target::Docker).await
}

/// Installation as a systemd service without Docker.
pub async fn install_systemd(
    s: &Session,
    info: &ServerInfo,
    plan: &Plan,
    bundle: &Bundle,
    log: &Log,
) -> Result<()> {
    s.check_root().await?;
    let v = &bundle.version;
    upload_asset(
        s,
        log,
        bundle,
        &release::linux_asset(v, &info.arch),
        "/tmp/hqw-bin.tar.gz",
    )
    .await?;
    step(
        s,
        log,
        tr!("Установка программы и пользователя", "Installing the program and its user"),
        &format!(
            r"rm -rf /tmp/hqw-bin && mkdir -p /tmp/hqw-bin
tar -xzf /tmp/hqw-bin.tar.gz -C /tmp/hqw-bin
test -f {BIN} && cp -f {BIN} {BIN}.prev || true
install -m 755 /tmp/hqw-bin/hq-watchman {BIN}
rm -rf /tmp/hqw-bin /tmp/hqw-bin.tar.gz
id hq-watchman >/dev/null 2>&1 || useradd --system --home-dir {DATA} --no-create-home --shell /usr/sbin/nologin hq-watchman
install -d -m 750 -o root -g hq-watchman {SYSTEMD_ETC}
install -d -m 700 -o hq-watchman -g hq-watchman {DATA}
{BIN} --version"
        ),
    )
    .await?;
    let l = crate::plan::layout(Target::Systemd);
    s.put_file(l.config, "640", plan.config_toml().as_bytes())
        .await?;
    s.put_file(l.secrets, "640", plan.secrets_env().as_bytes())
        .await?;
    let settings = serde_json::to_vec(&plan.settings())?;
    s.put_file("/tmp/hqw-settings.json", "644", &settings)
        .await?;
    s.put_file(UNIT, "644", unit().as_bytes()).await?;
    step(
        s,
        log,
        tr!("Начальные настройки", "Initial settings"),
        &format!(
            "chgrp hq-watchman {c} {e}\nsystemctl stop hq-watchman 2>/dev/null || true\nrunuser -u hq-watchman -- {BIN} settings set --config {c} < /tmp/hqw-settings.json\nrm -f /tmp/hqw-settings.json",
            c = l.config,
            e = l.secrets
        ),
    )
    .await?;
    if let (true, Some(g)) = (plan.github_webhook(), plan.github.as_ref()) {
        if let Some(Proxy::Nginx) = plan.proxy {
            setup_nginx(s, log, &g.domain, &g.hook_path).await?;
        } else {
            if !step(s, log, "Caddy", "command -v caddy || true")
                .await?
                .contains("caddy")
            {
                step(
                    s,
                    log,
                    tr!("Установка Caddy", "Installing Caddy"),
                    CADDY_APT,
                )
                .await?;
            }
            s.put_file(
                "/etc/caddy/hq-watchman.caddy",
                "644",
                caddyfile(&g.domain, &g.hook_path, "127.0.0.1:8080").as_bytes(),
            )
            .await?;
            step(
                s,
                log,
                tr!("Настройка Caddy", "Configuring Caddy"),
                "grep -q 'import /etc/caddy/hq-watchman.caddy' /etc/caddy/Caddyfile || echo 'import /etc/caddy/hq-watchman.caddy' >> /etc/caddy/Caddyfile\ncaddy validate --config /etc/caddy/Caddyfile --adapter caddyfile\nsystemctl enable caddy >/dev/null 2>&1\nsystemctl reload-or-restart caddy",
            )
            .await?;
        }
    }
    step(
        s,
        log,
        tr!("Запуск службы", "Starting the service"),
        "systemctl daemon-reload\nsystemctl enable hq-watchman >/dev/null 2>&1\nsystemctl restart hq-watchman",
    )
    .await?;
    wait_healthy(s, log, Target::Systemd).await
}

fn unit() -> String {
    format!(
        r"[Unit]
Description=HQ Watchman Telegram bot
After=network-online.target
Wants=network-online.target

[Service]
User=hq-watchman
Group=hq-watchman
ExecStart={BIN} run --config /etc/hq-watchman/config.toml --env-file /etc/hq-watchman/secrets.env
Restart=on-failure
RestartSec=10
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ReadWritePaths={DATA}
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
CapabilityBoundingSet=
SystemCallArchitectures=native
SystemCallFilter=@system-service
UMask=0077
MemoryMax=128M

[Install]
WantedBy=multi-user.target
"
    )
}

/// Waits up to two minutes for the bot to report healthy.
pub async fn wait_healthy(s: &Session, log: &Log, t: Target) -> Result<()> {
    log(format!(
        "› {}",
        tr!("Проверка работы бота", "Checking that the bot runs")
    ));
    let probe = match t {
        Target::Docker => "docker inspect -f '{{.State.Health.Status}}' hq-watchman",
        _ => "systemctl is-active hq-watchman >/dev/null && /usr/local/bin/hq-watchman healthcheck && echo healthy || echo starting",
    };
    for _ in 0..24 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let out = s.root_script(probe, None).await?;
        let st = out.stdout.trim();
        if st == "healthy" {
            log(format!("  {}", tr!("бот работает", "the bot is running")));
            return Ok(());
        }
        if st == "unhealthy" {
            break;
        }
    }
    let logs = logs(s, t, 30).await.unwrap_or_default();
    bail!(
        "{}\n{logs}",
        tr!(
            "бот не запустился, последние строки журнала:",
            "the bot did not start, last log lines:"
        )
    )
}

pub async fn logs(s: &Session, t: Target, lines: u32) -> Result<String> {
    let cmd = match t {
        Target::Docker => format!("docker logs --tail {lines} hq-watchman 2>&1"),
        _ => format!("journalctl -u hq-watchman -n {lines} --no-pager -o cat 2>&1"),
    };
    let out = s.root_script(&cmd, None).await?;
    Ok(out.stdout)
}

fn bot_cmd(t: Target, args: &str) -> String {
    match t {
        Target::Docker => {
            format!("docker exec -i hq-watchman hq-watchman {args} --config /config/config.toml")
        }
        _ => {
            format!("runuser -u hq-watchman -- {BIN} {args} --config /etc/hq-watchman/config.toml")
        }
    }
}

/// Settings and known repositories as JSON from the running installation.
pub async fn settings_get(s: &Session, t: Target) -> Result<serde_json::Value> {
    let out = s.root_script(&bot_cmd(t, "settings get"), None).await?;
    if !out.ok() {
        bail!("{}", out.reason());
    }
    Ok(serde_json::from_str(out.stdout.trim())?)
}

pub async fn settings_set(
    s: &Session,
    t: Target,
    settings: &hq_watchman::settings::Settings,
) -> Result<()> {
    settings.validate()?;
    let json = serde_json::to_vec(settings)?;
    s.put_file("/tmp/hqw-settings.json", "644", &json).await?;
    let out = s
        .root_script(
            &format!(
                "{} < /tmp/hqw-settings.json; rc=$?; rm -f /tmp/hqw-settings.json; exit $rc",
                bot_cmd(t, "settings set")
            ),
            None,
        )
        .await?;
    if !out.ok() {
        bail!("{}", out.reason());
    }
    Ok(())
}

#[derive(Debug, Default, Clone)]
pub struct Status {
    pub state: String,
    pub version: String,
}

pub async fn status(s: &Session, t: Target) -> Result<Status> {
    let script = match t {
        Target::Docker => "docker inspect -f '{{.State.Status}} {{.State.Health.Status}}' hq-watchman 2>/dev/null || echo missing\ndocker exec hq-watchman hq-watchman --version 2>&1 || true",
        _ => "echo \"$(systemctl is-active hq-watchman)\"\n/usr/local/bin/hq-watchman --version 2>&1 || true",
    };
    let out = s.root_script(script, None).await?;
    let mut lines = out.stdout.lines();
    Ok(Status {
        state: lines.next().unwrap_or("?").trim().to_owned(),
        version: lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("?")
            .to_owned(),
    })
}

pub async fn restart(s: &Session, t: Target) -> Result<()> {
    let cmd = match t {
        Target::Docker => format!("cd {DOCKER_DIR} && docker compose restart hq-watchman"),
        _ => "systemctl restart hq-watchman".to_owned(),
    };
    let out = s.root_script(&cmd, None).await?;
    if !out.ok() {
        bail!("{}", out.reason());
    }
    Ok(())
}

/// Fresh database snapshot as bytes.
pub async fn backup(s: &Session, t: Target) -> Result<Vec<u8>> {
    let out = s.root_script(&bot_cmd(t, "backup"), None).await?;
    if !out.ok() {
        bail!("{}", out.reason());
    }
    let v: serde_json::Value = serde_json::from_str(out.stdout.trim())?;
    let file = v["file"]
        .as_str()
        .ok_or_else(|| anyhow!("no backup file"))?
        .to_owned();
    if !file
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
    {
        bail!("unexpected backup path");
    }
    let read = match t {
        Target::Docker => format!(
            "docker cp hq-watchman:{file} /tmp/hqw-backup.db >/dev/null && cat /tmp/hqw-backup.db; rm -f /tmp/hqw-backup.db"
        ),
        _ => format!("cat {file}"),
    };
    let out = s.root_script(&read, None).await?;
    if !out.ok() || out.raw.len() < 100 || !out.raw.starts_with(b"SQLite format 3") {
        bail!("backup download failed");
    }
    Ok(out.raw)
}

/// Replaces the database with a backup; the bot is stopped meanwhile.
pub async fn restore(s: &Session, t: Target, db: &[u8], log: &Log) -> Result<()> {
    if !db.starts_with(b"SQLite format 3") {
        bail!(
            "{}",
            tr!(
                "это не файл базы SQLite",
                "this is not an SQLite database file"
            )
        );
    }
    s.put_file("/tmp/hqw-restore.db", "600", db).await?;
    let script = match t {
        Target::Docker => format!(
            "cd {DOCKER_DIR}\ndocker compose stop hq-watchman\nvol=$(docker volume inspect hq-watchman_data -f '{{{{.Mountpoint}}}}')\nrm -f \"$vol\"/hq-watchman.db-wal \"$vol\"/hq-watchman.db-shm\ninstall -m 600 -o 65532 -g 65532 /tmp/hqw-restore.db \"$vol\"/hq-watchman.db\nrm -f /tmp/hqw-restore.db\ndocker compose up -d"
        ),
        _ => format!(
            "systemctl stop hq-watchman\nrm -f {DATA}/hq-watchman.db-wal {DATA}/hq-watchman.db-shm\ninstall -m 600 -o hq-watchman -g hq-watchman /tmp/hqw-restore.db {DATA}/hq-watchman.db\nrm -f /tmp/hqw-restore.db\nsystemctl start hq-watchman"
        ),
    };
    step(
        s,
        log,
        tr!("Восстановление базы", "Restoring the database"),
        &script,
    )
    .await?;
    wait_healthy(s, log, t).await
}

/// Installs a new version and goes back to the old one if the bot does not get healthy.
pub async fn update(
    s: &Session,
    info: &ServerInfo,
    t: Target,
    bundle: &Bundle,
    log: &Log,
) -> Result<()> {
    s.check_root().await?;
    let v = &bundle.version;
    if t == Target::Docker {
        upload_asset(
            s,
            log,
            bundle,
            &release::docker_asset(v, &info.arch),
            "/tmp/hqw-image.tar.gz",
        )
        .await?;
        step(
            s,
            log,
            tr!("Новый образ", "New image"),
            &format!(
                "docker load -i /tmp/hqw-image.tar.gz\nrm -f /tmp/hqw-image.tar.gz\ncd {DOCKER_DIR}\ncp -f compose.yaml compose.yaml.prev\nsed -i -E 's#image: hq-watchman:[0-9.]+#image: hq-watchman:{v}#' compose.yaml\ndocker compose up -d"
            ),
        )
        .await?;
        if let Err(e) = wait_healthy(s, log, t).await {
            log(format!(
                "! {}",
                tr!(
                    "Откат на прежнюю версию",
                    "Rolling back to the previous version"
                )
            ));
            step(
                s,
                log,
                tr!("Откат", "Rollback"),
                &format!(
                    "cd {DOCKER_DIR}\nmv -f compose.yaml.prev compose.yaml\ndocker compose up -d"
                ),
            )
            .await?;
            return Err(e);
        }
    } else {
        upload_asset(
            s,
            log,
            bundle,
            &release::linux_asset(v, &info.arch),
            "/tmp/hqw-bin.tar.gz",
        )
        .await?;
        step(
            s,
            log,
            tr!("Новая версия программы", "New program version"),
            &format!(
                "rm -rf /tmp/hqw-bin && mkdir -p /tmp/hqw-bin\ntar -xzf /tmp/hqw-bin.tar.gz -C /tmp/hqw-bin\ncp -f {BIN} {BIN}.prev\ninstall -m 755 /tmp/hqw-bin/hq-watchman {BIN}\nrm -rf /tmp/hqw-bin /tmp/hqw-bin.tar.gz\nsystemctl restart hq-watchman"
            ),
        )
        .await?;
        if let Err(e) = wait_healthy(s, log, t).await {
            log(format!(
                "! {}",
                tr!(
                    "Откат на прежнюю версию",
                    "Rolling back to the previous version"
                )
            ));
            step(
                s,
                log,
                tr!("Откат", "Rollback"),
                &format!("install -m 755 {BIN}.prev {BIN}\nsystemctl restart hq-watchman"),
            )
            .await?;
            return Err(e);
        }
    }
    Ok(())
}

/// Removes the bot; with `keep_data` the database stays on the server.
pub async fn uninstall(s: &Session, t: Target, keep_data: bool, log: &Log) -> Result<()> {
    s.check_root().await?;
    let script = match t {
        Target::Docker => format!(
            "cd {DOCKER_DIR} 2>/dev/null && {{\n  if [ -f .nginx-site ]; then n=$(cat .nginx-site); rm -f /etc/nginx/sites-enabled/\"$n\" /etc/nginx/sites-available/\"$n\"; nginx -t && systemctl reload nginx; fi\n  docker compose down {v}--remove-orphans\n}}\ndocker image ls --format '{{{{.Repository}}}}:{{{{.Tag}}}}' | grep '^hq-watchman:' | xargs -r docker rmi\n{rm}",
            v = if keep_data { "" } else { "-v " },
            rm = if keep_data {
                format!("rm -f {DOCKER_DIR}/.env")
            } else {
                format!("rm -rf {DOCKER_DIR}")
            }
        ),
        _ => format!(
            "systemctl disable --now hq-watchman 2>/dev/null || true\nrm -f {UNIT} {BIN} {BIN}.prev /etc/caddy/hq-watchman.caddy\nsed -i '\\#import /etc/caddy/hq-watchman.caddy#d' /etc/caddy/Caddyfile 2>/dev/null && systemctl reload caddy 2>/dev/null || true\nsystemctl daemon-reload\nrm -rf {SYSTEMD_ETC}\n{rm}",
            rm = if keep_data {
                String::new()
            } else {
                format!("rm -rf {DATA}\nuserdel hq-watchman 2>/dev/null || true")
            }
        ),
    };
    step(s, log, tr!("Удаление", "Removing"), &script).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_variants() {
        let plain = compose("0.1.0", None, false);
        assert!(plain.contains("image: hq-watchman:0.1.0"));
        assert!(!plain.contains("ports:") && !plain.contains("caddy"));
        let c = compose("0.1.0", Some(Proxy::Caddy), true);
        assert!(c.contains("hq-watchman-caddy") && c.contains("caddy_data: {}"));
        let n = compose("0.1.0", Some(Proxy::Nginx), true);
        assert!(n.contains("127.0.0.1:8080:8080") && !n.contains("hq-watchman-caddy"));
    }

    #[test]
    fn proxy_configs_route_only_the_hook_path() {
        let c = caddyfile("h.example.org", "abc", "hq-watchman:8080");
        assert!(c.contains("handle /abc") && c.contains("respond 404"));
        let n = nginx_site("h.example.org", "abc", true);
        assert!(n.contains("location = /abc") && n.contains("live/h.example.org"));
        assert!(!nginx_site("h.example.org", "abc", false).contains("443"));
    }
}
