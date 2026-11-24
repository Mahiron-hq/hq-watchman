# Installing HQ Watchman

There are two ways: the **HQ Watchman Setup** app does everything through windows and buttons, or you install by hand on a Linux server with Docker.

Contents:

1. [Install without a console (HQ Watchman Setup)](#1-install-without-a-console-hq-watchman-setup)
2. [Changing settings later](#2-changing-settings-later)
3. [Manual installation](#3-manual-installation)
4. [Telegram bot](#4-telegram-bot)
5. [Steam profile and key](#5-steam-profile-and-key)
6. [GitHub App](#6-github-app)
7. [Environment variables](#7-environment-variables)
8. [Server](#8-server)
9. [Reverse proxy](#9-reverse-proxy)
10. [Updating](#10-updating)
11. [Changing secrets](#11-changing-secrets)
12. [Backups and restore](#12-backups-and-restore)

## 1. Install without a console (HQ Watchman Setup)

Download the setup app for your computer from the [release](https://github.com/Mahiron-hq/hq-watchman/releases) and check it against `SHA256SUMS.txt`:

| System | File | How to start |
|---|---|---|
| Windows 10/11 x64 | `hq-watchman-setup-<version>-windows-x64.exe` | Double click. SmartScreen warns about an unknown publisher (the app has no paid code signing certificate): **More info → Run anyway**. |
| macOS 11+ | `hq-watchman-setup-<version>-macos-universal.zip` | Unpack, then right click the app → **Open** → **Open** (the app is not notarized by Apple). |
| Linux x64 | `hq-watchman-setup-<version>-linux-x64.tar.gz` | Unpack and run `hq-watchman-setup`. |

The app asks only what the chosen sources need:

1. **Where the bot runs**: a Linux server (Debian or Ubuntu) in Docker, a Linux server as a systemd service without Docker, or this Windows computer as a background task (works only while the computer is on).
2. **Server**: address, SSH port, user, password or key. The app checks access and root rights. Passwords are never saved; the server key is shown once for confirmation and pinned.
3. **Sources**: GitHub, Steam guides, Steam games, any combination.
4. **Telegram**: create a bot in @BotFather, paste the token, open the bot and press Start: the app takes your ID from that.
5. **GitHub**: your login and the mode. *Polling* needs nothing else (events arrive within a minute). *Webhook* needs a domain or subdomain pointing to the server; the HTTPS certificate is obtained automatically. The app opens GitHub in the browser: press **Create GitHub App**, then **Install** on **All repositories**. Access is read-only.
6. **Steam guides**: a link to your profile. The Web API key is optional.
7. **Steam games**: search by name, store link or AppID.
8. **Notifications**: language of bot messages (Russian or English), time zone, quiet hours.
9. **Install**: the app downloads the bot files from the release, checks the minisign signature and the checksums, installs Docker if needed, starts the bot and checks that it works. The bot sends you a start message.

The app needs no administrator rights, except for the Windows target (one UAC prompt).

## 2. Changing settings later

- In Telegram: `/settings` opens a menu with buttons: sources and event types, repositories, games (add by name, link or AppID), delivery to groups and channels, quiet hours, time zone, language.
- In HQ Watchman Setup: **Manage an installed bot** connects to the server and shows the same settings, plus logs, restart, update with automatic rollback, database backup and restore, removal.
- To add a group or channel, add the bot there yourself (for a channel, as an admin allowed to post). The bot leaves chats it was added to by anyone else.
- New credentials (another token, adding GitHub or Steam): run the setup wizard again on the same server, the bot data is kept.

## 3. Manual installation

Target: Debian 13 (x86_64 or arm64) with Docker Engine and the Compose plugin. Only Telegram is required; GitHub and Steam guides work when their variables are set.

## 4. Telegram bot

1. Open [@BotFather](https://t.me/BotFather), send `/newbot` and follow the steps. The token looks like `123456789:AA...`. It goes to `HQW_TELEGRAM_TOKEN`.
2. Find your numeric Telegram user ID, for example with [@userinfobot](https://t.me/userinfobot). It goes to `HQW_TELEGRAM_ADMIN_ID`.
3. Open a chat with your bot and press **Start**. Telegram does not let a bot write first.

## 5. Steam profile and key

1. `HQW_STEAM_PROFILE_ID` is the SteamID64 of the profile whose guides are watched (the number in `https://steamcommunity.com/profiles/<SteamID64>`). Without it, guides are off.
2. Optional: a key from <https://steamcommunity.com/dev/apikey> in `HQW_STEAM_API_KEY` makes the guide list more reliable. Without it the bot reads the profile page.

Game updates use anonymous Steam access and need nothing.

## 6. GitHub App

The app is private, read-only and installed on all repositories of your account, so new repositories are picked up automatically.

1. Choose the mode in `config.toml`: `[github] mode = "polling"` (no domain, events read from the App delivery log once a minute) or `"webhook"` (needs HTTPS on a domain and a `[webhook]` section).
2. For webhook mode, generate a secret (`openssl rand -hex 32`) and a random path segment (`openssl rand -hex 24`, goes to `webhook.path`).
3. Open **GitHub → Settings → Developer settings → GitHub Apps → New GitHub App**:
   - **Webhook**: active. **Webhook URL**: `https://<your-domain>/<path segment>` for webhook mode; for polling mode any address that does not answer, for example `https://hq-watchman.invalid/x`. **Webhook secret**: the secret from step 2 (webhook mode only).
   - **Repository permissions**, all **Read-only**: Metadata, Issues, Pull requests, Contents, Discussions. Never grant write access.
   - **Subscribe to events**: Issues, Issue comment, Pull request, Pull request review, Pull request review comment, Commit comment, Discussion, Discussion comment.
   - **Where can this GitHub App be installed?** Only on this account.
4. Note the **App ID** (`HQW_GITHUB_APP_ID`), **Generate a private key** (`HQW_GITHUB_APP_KEY`), then **Install App** → **All repositories**.
5. Your numeric GitHub account ID goes to `HQW_GITHUB_ADMIN_ID`: `curl -s https://api.github.com/users/<login> | grep '"id"'` (the first `id`). Your own actions are never reported.

## 7. Environment variables

Secrets are read once at start and removed from the environment. Docker reads them from `.env`; a service without Docker uses `--env-file <path>` with the same lines. The config file never contains secrets.

| Variable | Needed for | Format |
|---|---|---|
| `HQW_TELEGRAM_TOKEN` | always | `<digits>:<35+ characters>` |
| `HQW_TELEGRAM_ADMIN_ID` | always | positive integer |
| `HQW_GITHUB_APP_ID` | GitHub | positive integer |
| `HQW_GITHUB_APP_KEY` | GitHub | PEM; one line with `\n` instead of line breaks |
| `HQW_GITHUB_ADMIN_ID` | GitHub | positive integer |
| `HQW_GITHUB_WEBHOOK_SECRET` | GitHub, webhook mode | at least 32 bytes of random data |
| `HQW_STEAM_PROFILE_ID` | Steam guides | 17 digits |
| `HQW_STEAM_API_KEY` | optional | 32 hex characters |

The three GitHub App variables are set together or not at all. To put the PEM key on one line: `awk 'NF {printf "%s\\n", $0}' key.pem`. The file must be readable only by its owner: `chmod 600 .env`.

## 8. Server

1. Install Docker Engine and the Compose plugin from the [official Docker repository](https://docs.docker.com/engine/install/debian/).
2. Download the image archive for your CPU from the release and check it:

   ```sh
   minisign -Vm SHA256SUMS.txt -P RWRh/ArZd0sNbyZPIKim+NSoQfeD97vdcfgJB4GQGpeZ8fIl4yWIrryu
   sha256sum -c --ignore-missing SHA256SUMS.txt
   docker load -i hq-watchman-<version>-docker-amd64.tar.gz
   ```

3. Create the folder and files:

   ```sh
   sudo install -d -m 750 /opt/hq-watchman
   cd /opt/hq-watchman
   sudo cp /path/to/deploy/compose.yaml .
   sudo cp /path/to/deploy/config.example.toml config.toml
   sudo install -m 600 /dev/null .env
   ```

4. With nginx on the host, set `webhook.trusted_proxies = ["172.31.250.1"]` (the gateway of the Compose subnet). In polling mode remove the `ports` lines from `compose.yaml`: the bot needs no incoming connections.
5. Start: `docker compose up -d`, check `docker compose ps` (status `healthy`). You get a start message in Telegram. Notification settings are then changed with `/settings`.

The container runs as user 65532 with a read-only root file system, all capabilities dropped and `no-new-privileges`.

## 9. Reverse proxy

Webhook mode only. Example nginx site for `hooks.example.com` with a Let's Encrypt certificate:

```nginx
server {
    listen 443 ssl;
    http2 on;
    server_name hooks.example.com;
    ssl_certificate     /etc/letsencrypt/live/hooks.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/hooks.example.com/privkey.pem;
    client_max_body_size 6m;
    location = /<path segment> {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_read_timeout 30s;
    }
    location / { return 404; }
}
```

Without a reverse proxy, publish the port, mount the certificate files read-only and set `[webhook.tls]` in `config.toml`.

## 10. Updating

```sh
docker load -i hq-watchman-<version>-docker-amd64.tar.gz
# set the new tag in compose.yaml
docker compose up -d
```

Database migrations run automatically at start; a backup is written before any migration. HQ Watchman Setup does the same and goes back to the old version if the new one does not start.

## 11. Changing secrets

After every change, restart the bot (`docker compose up -d`).

- **Telegram token**: in BotFather send `/revoke`, put the new token into `.env`.
- **GitHub webhook secret**: set the new one in the app settings and in `.env` at the same time. Failed deliveries are redelivered by the hourly check.
- **GitHub App private key**: generate a new key, put it into `.env`, restart, then delete the old key on the app page.
- **Steam Web API key**: revoke and register a new one, put it into `.env`.

## 12. Backups and restore

Once a day the bot writes a copy of the database next to it (`hq-watchman.backup-<date>.db`) and keeps the last 7. `hq-watchman backup` makes one at once. HQ Watchman Setup downloads and restores copies with one button. By hand:

```sh
docker compose stop
docker run --rm -v hq-watchman_data:/data alpine sh -c 'cp /data/hq-watchman.backup-<date>.db /data/hq-watchman.db && rm -f /data/hq-watchman.db-wal /data/hq-watchman.db-shm && chown 65532:65532 /data/hq-watchman.db && chmod 600 /data/hq-watchman.db'
docker compose start
```
