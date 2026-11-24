# Установка HQ Watchman

Целевая система: Debian 13 (x86_64) с Docker Engine и плагином Compose. Бот работает в одном контейнере. Вебхуки GitHub приходят к нему по HTTPS через обратный прокси (в этой инструкции nginx) или через встроенный TLS.

Содержание:

1. [Telegram-бот](#1-telegram-бот)
2. [Ключ Steam Web API и профиль](#2-ключ-steam-web-api-и-профиль)
3. [GitHub App](#3-github-app)
4. [Переменные окружения](#4-переменные-окружения)
5. [Сервер](#5-сервер)
6. [Обратный прокси](#6-обратный-прокси)
7. [Обновление](#7-обновление)
8. [Смена секретов](#8-смена-секретов)
9. [Резервные копии и восстановление](#9-резервные-копии-и-восстановление)

## 1. Telegram-бот

1. Откройте [@BotFather](https://t.me/BotFather), отправьте `/newbot` и пройдите шаги. Токен выглядит как `123456789:AA...`, он идёт в `HQW_TELEGRAM_TOKEN`.
2. Узнайте свой числовой ID в Telegram, например через [@userinfobot](https://t.me/userinfobot). Он идёт в `HQW_TELEGRAM_ADMIN_ID`.
3. Откройте чат со своим ботом и нажмите **Start**. Telegram не даёт боту написать первым.

Меню команд бот регистрирует только для вашего чата. Остальные пользователи команд не видят.

## 2. Ключ Steam Web API и профиль

1. Войдите на <https://steamcommunity.com/dev/apikey>. Для этой страницы Steam требует мобильный аутентификатор Steam Guard.
2. Укажите любой домен (например, домен сервера) и примите условия. Ключ из 32 символов идёт в `HQW_STEAM_API_KEY`.
3. `HQW_STEAM_PROFILE_ID` – SteamID64 профиля, чьи руководства отслеживаются (число в `https://steamcommunity.com/profiles/<SteamID64>`). Комментарии этого профиля сохраняются, но не присылаются.

Обновления игр бот получает анонимно, аккаунт для этого не нужен.

## 3. GitHub App

App приватное, только на чтение и установлено на все репозитории аккаунта, поэтому новые репозитории подключаются сами.

1. Сгенерируйте секрет вебхука (не меньше 32 случайных байт): `openssl rand -hex 32`.
2. Сгенерируйте случайный сегмент пути вебхука: `openssl rand -hex 24`. Запишите его в `webhook.path` в `config.toml`.
3. Откройте **GitHub → Settings → Developer settings → GitHub Apps → New GitHub App** и заполните:
   - **GitHub App name**: любое свободное имя, например `hq-watchman-<логин>`.
   - **Homepage URL**: адрес репозитория или профиля.
   - **Webhook**: включён. **Webhook URL**: `https://<ваш-домен>/<сегмент пути>`. **Webhook secret**: секрет из шага 1.
   - **Repository permissions**, все **Read-only**: Metadata, Issues, Pull requests, Contents, Discussions. Остальное оставьте **No access**. Права на запись не выдавайте никогда.
   - **Subscribe to events**: Issues, Issue comment, Pull request, Pull request review, Pull request review comment, Commit comment, Discussion, Discussion comment. События установки приходят App всегда.
   - **Where can this GitHub App be installed?** Only on this account.
4. Создайте App. Запишите **App ID** (`HQW_GITHUB_APP_ID`).
5. На странице App нажмите **Generate a private key**. GitHub скачает файл `.pem`, его полное содержимое идёт в `HQW_GITHUB_APP_KEY` (формат в [разделе 4](#4-переменные-окружения)).
6. **Install App** → ваш аккаунт → **All repositories**.
7. Числовой ID вашего аккаунта GitHub идёт в `HQW_GITHUB_ADMIN_ID`: `curl -s https://api.github.com/users/<логин> | grep '"id"'` (первый `id`). Ваши собственные действия сравниваются по этому ID и не присылаются.

Если у вас есть организации, установите App и на них.

## 4. Переменные окружения

Секреты и идентификаторы читаются из окружения один раз при запуске, после чего удаляются из него. В конфиг-файле секретов нет: ключ, похожий на секрет, останавливает бота.

| Переменная | Что это | Формат |
|---|---|---|
| `HQW_TELEGRAM_TOKEN` | Токен бота от BotFather | `<цифры>:<35+ символов>` |
| `HQW_TELEGRAM_ADMIN_ID` | Ваш ID в Telegram, единственный, с кем бот общается | положительное целое |
| `HQW_GITHUB_APP_ID` | ID GitHub App | положительное целое |
| `HQW_GITHUB_APP_KEY` | Закрытый ключ GitHub App | PEM; одной строкой с `\n` вместо переводов строк |
| `HQW_GITHUB_WEBHOOK_SECRET` | Секрет вебхука | не меньше 32 байт случайных данных |
| `HQW_GITHUB_ADMIN_ID` | Числовой ID вашего аккаунта GitHub | положительное целое |
| `HQW_STEAM_API_KEY` | Ключ Steam Web API | 32 шестнадцатеричных символа |
| `HQW_STEAM_PROFILE_ID` | SteamID64 профиля с руководствами | 17 цифр |

Чтобы записать PEM-ключ одной строкой: `awk 'NF {printf "%s\\n", $0}' key.pem`. Файл `.env` должен быть доступен только владельцу: `chmod 600 .env`. Список имён лежит в [`deploy/.env.example`](../deploy/.env.example).

## 5. Сервер

1. Установите Docker Engine и плагин Compose из [официального репозитория Docker для Debian](https://docs.docker.com/engine/install/debian/).
2. Скачайте архив образа из запуска CI (артефакт `hq-watchman-image`) или из релиза и проверьте его:

   ```sh
   sha256sum -c SHA256SUMS.txt
   docker load -i hq-watchman-0.1.0-linux-amd64.tar.gz
   ```

3. Создайте папку и файлы:

   ```sh
   sudo install -d -m 750 /opt/hq-watchman
   cd /opt/hq-watchman
   sudo cp /path/to/deploy/compose.yaml .
   sudo cp /path/to/deploy/config.example.toml config.toml
   sudo install -m 600 /dev/null .env
   ```

   Отредактируйте `config.toml` (как минимум `timezone` и `webhook.path`) и заполните `.env`.

4. Если подсеть `172.31.250.0/24` из `compose.yaml` на сервере занята, замените её. При nginx на хосте укажите `webhook.trusted_proxies = ["172.31.250.1"]` (шлюз этой подсети), чтобы бот видел настоящий адрес GitHub для IP-фильтра.
5. Запуск: `docker compose up -d`. Проверка: `docker compose ps` (статус `healthy`) и `docker compose logs -f`. В Telegram придёт сообщение о запуске.

Контейнер работает от пользователя 65532, корневая файловая система только для чтения, все capabilities сброшены, включён `no-new-privileges`. Запись разрешена только в том `data`. Порт вебхука открыт только на `127.0.0.1`.

## 6. Обратный прокси

Пример сайта nginx для `hooks.example.com` с сертификатом Let's Encrypt (`certbot --nginx -d hooks.example.com`):

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

Без обратного прокси откройте порт на всех адресах, смонтируйте файлы сертификата только для чтения и задайте `[webhook.tls]` в `config.toml`. Бот перечитывает файлы в течение минуты после их изменения.

## 7. Обновление

```sh
docker load -i hq-watchman-<версия>-linux-amd64.tar.gz
# укажите новый тег в compose.yaml
docker compose up -d
```

Миграции базы применяются при запуске автоматически, перед любой миграцией делается резервная копия.

## 8. Смена секретов

После каждой смены выполните `docker compose up -d`, чтобы контейнер перезапустился с новым окружением.

- **Токен Telegram**: в BotFather отправьте `/revoke`, выберите бота, впишите новый токен в `.env`.
- **Секрет вебхука GitHub**: сгенерируйте новый и одновременно задайте его в настройках App (**Webhook secret**) и в `.env`. Доставки, упавшие в промежутке, бот запросит повторно при ежечасной сверке.
- **Закрытый ключ GitHub App**: создайте новый ключ на странице App, впишите его в `.env`, перезапустите, затем удалите старый ключ на странице App.
- **Ключ Steam Web API**: на <https://steamcommunity.com/dev/apikey> отзовите ключ и зарегистрируйте новый, впишите его в `.env`.
- **Путь вебхука**: измените `webhook.path` в `config.toml` и адрес вебхука в настройках App, затем перезапустите.

## 9. Резервные копии и восстановление

Раз в сутки бот пишет согласованную копию базы рядом с ней (`/data/hq-watchman.backup-<дата>.db`) и хранит последние 7. Восстановление:

```sh
docker compose stop
docker run --rm -v hq-watchman_data:/data alpine sh -c 'cp /data/hq-watchman.backup-<дата>.db /data/hq-watchman.db && rm -f /data/hq-watchman.db-wal /data/hq-watchman.db-shm && chown 65532:65532 /data/hq-watchman.db && chmod 600 /data/hq-watchman.db'
docker compose start
```
