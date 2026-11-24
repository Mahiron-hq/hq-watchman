# Installing HQ Watchman

Target: Debian 13 (x86_64) with Docker Engine and the Compose plugin. The bot runs as one container. GitHub webhooks reach it over HTTPS through a reverse proxy (nginx in this guide) or through the built-in TLS.

Contents:

1. [Telegram bot](#1-telegram-bot)
2. [Steam Web API key and profile](#2-steam-web-api-key-and-profile)
3. [GitHub App](#3-github-app)
4. [Environment variables](#4-environment-variables)
5. [Server](#5-server)
6. [Reverse proxy](#6-reverse-proxy)
7. [Updating](#7-updating)
8. [Changing secrets](#8-changing-secrets)
9. [Backups and restore](#9-backups-and-restore)

## 1. Telegram bot

1. Open [@BotFather](https://t.me/BotFather), send `/newbot` and follow the steps. The token looks like `123456789:AA...`. It goes to `HQW_TELEGRAM_TOKEN`.
2. Find your numeric Telegram user ID, for example with [@userinfobot](https://t.me/userinfobot). It goes to `HQW_TELEGRAM_ADMIN_ID`.
3. Open a chat with your bot and press **Start**. Telegram does not let a bot write first.

The bot registers its command menu only for your chat. Other users see no commands.

## 2. Steam Web API key and profile

1. Sign in at <https://steamcommunity.com/dev/apikey>. Steam requires the Steam Guard mobile authenticator for this page.
2. Enter any domain name (for example your server domain) and accept the terms. The 32-character key goes to `HQW_STEAM_API_KEY`.
3. `HQW_STEAM_PROFILE_ID` is the SteamID64 of the profile whose guides are watched (the number in `https://steamcommunity.com/profiles/<SteamID64>`). Comments by this profile are stored but not reported.

Game updates use anonymous Steam access and need no account.

## 3. GitHub App

The app is private, read-only and installed on all repositories of your account, so new repositories are picked up automatically.

1. Generate a webhook secret (at least 32 random bytes): `openssl rand -hex 32`.
2. Generate a random path segment for the webhook URL: `openssl rand -hex 24`. Put it into `webhook.path` in `config.toml`.
3. Open **GitHub → Settings → Developer settings → GitHub Apps → New GitHub App** and fill in:
   - **GitHub App name**: any free name, for example `hq-watchman-<login>`.
   - **Homepage URL**: your repository or profile URL.
   - **Webhook**: active. **Webhook URL**: `https://<your-domain>/<path segment>`. **Webhook secret**: the secret from step 1.
   - **Repository permissions**, all **Read-only**: Metadata, Issues, Pull requests, Contents, Discussions. Leave everything else at **No access**. Never grant write access.
   - **Subscribe to events**: Issues, Issue comment, Pull request, Pull request review, Pull request review comment, Commit comment, Discussion, Discussion comment. Installation events are always sent to apps.
   - **Where can this GitHub App be installed?** Only on this account.
4. Create the app. Note the **App ID** (`HQW_GITHUB_APP_ID`).
5. On the app page, **Generate a private key**. GitHub downloads a `.pem` file. Its full content goes to `HQW_GITHUB_APP_KEY`; see [section 4](#4-environment-variables) for the format.
6. **Install App** → your account → **All repositories**.
7. Your numeric GitHub account ID goes to `HQW_GITHUB_ADMIN_ID`: `curl -s https://api.github.com/users/<login> | grep '"id"'` (the first `id`). Your own actions are compared by this ID and never reported.

If you have organizations, install the app on each of them too.

## 4. Environment variables

The secrets and identifiers are read from the environment once at start and then removed from it. The config file never contains secrets: a secret-looking key there stops the bot.

| Variable | Meaning | Format |
|---|---|---|
| `HQW_TELEGRAM_TOKEN` | Bot token from BotFather | `<digits>:<35+ characters>` |
| `HQW_TELEGRAM_ADMIN_ID` | Your Telegram user ID, the only person the bot talks to | positive integer |
| `HQW_GITHUB_APP_ID` | GitHub App ID | positive integer |
| `HQW_GITHUB_APP_KEY` | GitHub App private key | PEM; one line with `\n` instead of line breaks |
| `HQW_GITHUB_WEBHOOK_SECRET` | Webhook secret | at least 32 bytes of random data |
| `HQW_GITHUB_ADMIN_ID` | Your numeric GitHub account ID | positive integer |
| `HQW_STEAM_API_KEY` | Steam Web API key | 32 hex characters |
| `HQW_STEAM_PROFILE_ID` | SteamID64 of the guides profile | 17 digits |

To put the PEM key on one line: `awk 'NF {printf "%s\\n", $0}' key.pem`. The `.env` file must be readable only by its owner: `chmod 600 .env`. A list of names is in [`deploy/.env.example`](../deploy/.env.example).

## 5. Server

1. Install Docker Engine and the Compose plugin from the [official Docker repository for Debian](https://docs.docker.com/engine/install/debian/).
2. Download the image archive from the CI run (artifact `hq-watchman-image`) or the release, and check it:

   ```sh
   sha256sum -c SHA256SUMS.txt
   docker load -i hq-watchman-0.1.0-linux-amd64.tar.gz
   ```

3. Create the folder and files:

   ```sh
   sudo install -d -m 750 /opt/hq-watchman
   cd /opt/hq-watchman
   sudo cp /path/to/deploy/compose.yaml .
   sudo cp /path/to/deploy/config.example.toml config.toml
   sudo install -m 600 /dev/null .env
   ```

   Edit `config.toml` (at least `timezone` and `webhook.path`) and fill `.env`.

4. If the subnet `172.31.250.0/24` in `compose.yaml` is busy on your server, change it. With nginx on the host, set `webhook.trusted_proxies = ["172.31.250.1"]` (the gateway of that subnet) so the bot sees the real GitHub address for the IP filter.
5. Start: `docker compose up -d`. Check: `docker compose ps` (status `healthy`) and `docker compose logs -f`. You get a start message in Telegram.

The container runs as user 65532 with a read-only root file system, all capabilities dropped and `no-new-privileges`. Only the `data` volume is writable. The webhook port is published on `127.0.0.1` only.

## 6. Reverse proxy

Example nginx site for `hooks.example.com` with a Let's Encrypt certificate (`certbot --nginx -d hooks.example.com`):

```nginx
server {
    listen 443 ssl;
    http2 on;
    server_name hooks.example.com;

    ssl_certificate     /etc/letsencrypt/live/hooks.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/hooks.example.com/privkey.pem;

    client_max_body_size 6m;
    access_log off;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_read_timeout 15s;
    }
}
```

Without a reverse proxy, publish the port on all addresses, mount the certificate files read-only and set `[webhook.tls]` in `config.toml`. The bot re-reads the files within a minute after they change.

## 7. Updating

```sh
docker load -i hq-watchman-<version>-linux-amd64.tar.gz
# set the new tag in compose.yaml
docker compose up -d
```

Database migrations run automatically at start. A backup is written before any migration.

## 8. Changing secrets

After every change, run `docker compose up -d` so the container restarts with the new environment.

- **Telegram token**: in BotFather send `/revoke`, choose the bot, put the new token into `.env`.
- **GitHub webhook secret**: generate a new one, set it in the app settings (**Webhook secret**) and in `.env` at the same time. Deliveries that fail in between are redelivered by the hourly check.
- **GitHub App private key**: generate a new key on the app page, put it into `.env`, restart, then delete the old key on the app page.
- **Steam Web API key**: on <https://steamcommunity.com/dev/apikey> revoke the key and register a new one, put it into `.env`.
- **Webhook path**: change `webhook.path` in `config.toml` and the webhook URL in the app settings, then restart.

## 9. Backups and restore

Once a day the bot writes a consistent copy of the database next to it (`/data/hq-watchman.backup-<date>.db`) and keeps the last 7. To restore:

```sh
docker compose stop
docker run --rm -v hq-watchman_data:/data alpine sh -c 'cp /data/hq-watchman.backup-<date>.db /data/hq-watchman.db && rm -f /data/hq-watchman.db-wal /data/hq-watchman.db-shm && chown 65532:65532 /data/hq-watchman.db && chmod 600 /data/hq-watchman.db'
docker compose start
```
