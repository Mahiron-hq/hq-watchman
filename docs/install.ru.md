# Установка HQ Watchman

Есть два пути: программа **HQ Watchman Setup** делает всё через окна и кнопки, либо установка вручную на сервер Linux с Docker.

Содержание:

1. [Установка без консоли (HQ Watchman Setup)](#1-установка-без-консоли-hq-watchman-setup)
2. [Изменение настроек потом](#2-изменение-настроек-потом)
3. [Установка вручную](#3-установка-вручную)
4. [Telegram-бот](#4-telegram-бот)
5. [Профиль и ключ Steam](#5-профиль-и-ключ-steam)
6. [GitHub App](#6-github-app)
7. [Переменные окружения](#7-переменные-окружения)
8. [Сервер](#8-сервер)
9. [Обратный прокси](#9-обратный-прокси)
10. [Обновление](#10-обновление)
11. [Смена секретов](#11-смена-секретов)
12. [Резервные копии и восстановление](#12-резервные-копии-и-восстановление)

## 1. Установка без консоли (HQ Watchman Setup)

Скачайте программу установки для своего компьютера из [релиза](https://github.com/Mahiron-hq/hq-watchman/releases) и сверьте её с `SHA256SUMS.txt`:

| Система | Файл | Как запустить |
|---|---|---|
| Windows 10/11 x64 | `hq-watchman-setup-<версия>-windows-x64.exe` | Двойной щелчок. SmartScreen предупредит о неизвестном издателе (у программы нет платного сертификата подписи): **Подробнее → Выполнить в любом случае**. |
| macOS 11+ | `hq-watchman-setup-<версия>-macos-universal.zip` | Распакуйте, затем правый щелчок по программе → **Открыть** → **Открыть** (программа не заверена Apple). |
| Linux x64 | `hq-watchman-setup-<версия>-linux-x64.tar.gz` | Распакуйте и запустите `hq-watchman-setup`. |

Программа спрашивает только то, что нужно выбранным источникам:

1. **Где работает бот**: сервер Linux (Debian или Ubuntu) в Docker, сервер Linux как служба systemd без Docker или этот компьютер с Windows как фоновая задача (работает, только пока компьютер включён).
2. **Сервер**: адрес, порт SSH, пользователь, пароль или ключ. Программа проверяет доступ и права root. Пароли не сохраняются; ключ сервера показывается один раз для подтверждения и запоминается.
3. **Источники**: GitHub, руководства Steam, игры Steam, в любом сочетании.
4. **Telegram**: создайте бота в @BotFather, вставьте токен, нажмите «Открыть бота», а затем Start в Telegram. Бот получит одноразовый код, и программа возьмёт ваш ID только из сообщения с этим кодом. Если Start ничего не делает, отправьте боту сообщением код, показанный в программе.
5. **GitHub**: ваш логин и режим. *Опрос* больше ничего не требует (события приходят в течение минуты). *Вебхук* требует домен или поддомен, указывающий на сервер; сертификат HTTPS получается автоматически. Программа откроет GitHub в браузере: нажмите **Create GitHub App**, затем **Install** на **All repositories**. Доступ только на чтение.
6. **Руководства Steam**: ссылка на ваш профиль. Ключ Web API необязателен.
7. **Игры Steam**: поиск по названию, ссылке на магазин или AppID.
8. **Уведомления**: язык сообщений бота (русский или английский), часовой пояс, тихие часы.
9. **Установка**: программа скачивает файлы бота из релиза, проверяет подпись minisign и контрольные суммы, при необходимости ставит Docker, запускает бота и проверяет его работу. Бот присылает вам сообщение о запуске.

Права администратора программе не нужны, кроме установки на Windows (одно окно UAC).

## 2. Изменение настроек потом

- В Telegram: `/settings` открывает меню с кнопками: источники и типы событий, репозитории, игры (добавление по названию, ссылке или AppID), доставка в группы и каналы, тихие часы, часовой пояс, язык.
- В HQ Watchman Setup: **Управлять установленным ботом** подключается к серверу и показывает те же настройки, а также журнал, перезапуск, обновление с автоматическим откатом, резервную копию базы и восстановление, удаление.
- Чтобы добавить группу или канал, добавьте туда бота сами (в канал администратором с правом публикации). Из чатов, куда его добавил кто-то другой, бот выходит.
- Новые ключи (другой токен, подключение GitHub или Steam): пройдите мастер установки заново на том же сервере, данные бота сохранятся.

## 3. Установка вручную

Целевая система: Debian 13 (x86_64 или arm64) с Docker Engine и плагином Compose. Обязателен только Telegram; GitHub и руководства Steam работают, когда заданы их переменные.

## 4. Telegram-бот

1. Откройте [@BotFather](https://t.me/BotFather), отправьте `/newbot` и следуйте шагам. Токен вида `123456789:AA...` идёт в `HQW_TELEGRAM_TOKEN`.
2. Узнайте свой числовой ID в Telegram, например через [@userinfobot](https://t.me/userinfobot). Он идёт в `HQW_TELEGRAM_ADMIN_ID`.
3. Откройте чат со своим ботом и нажмите **Start**. Telegram не разрешает боту писать первым.

## 5. Профиль и ключ Steam

1. `HQW_STEAM_PROFILE_ID` это SteamID64 профиля, чьи руководства отслеживаются (число в `https://steamcommunity.com/profiles/<SteamID64>`). Без него руководства выключены.
2. Необязательно: ключ с <https://steamcommunity.com/dev/apikey> в `HQW_STEAM_API_KEY` делает чтение списка руководств надёжнее. Без него бот читает страницу профиля.

Обновления игр используют анонимный доступ к Steam и ничего не требуют.

## 6. GitHub App

Приложение приватное, только на чтение и установлено на все репозитории аккаунта, поэтому новые репозитории подхватываются сами.

1. Выберите режим в `config.toml`: `[github] mode = "polling"` (без домена, события читаются из журнала доставок App раз в минуту) или `"webhook"` (нужен HTTPS на домене и раздел `[webhook]`).
2. Для режима вебхука создайте секрет (`openssl rand -hex 32`) и случайный сегмент пути (`openssl rand -hex 24`, идёт в `webhook.path`).
3. Откройте **GitHub → Settings → Developer settings → GitHub Apps → New GitHub App**:
   - **Webhook**: включён. **Webhook URL**: `https://<ваш-домен>/<сегмент пути>` для режима вебхука; для режима опроса любой адрес, который не отвечает, например `https://hq-watchman.invalid/x`. **Webhook secret**: секрет из шага 2 (только для вебхука).
   - **Repository permissions**, все **Read-only**: Metadata, Issues, Pull requests, Contents, Discussions. Права на запись не давайте никогда.
   - **Subscribe to events**: Issues, Issue comment, Pull request, Pull request review, Pull request review comment, Commit comment, Discussion, Discussion comment.
   - **Where can this GitHub App be installed?** Only on this account.
4. Запишите **App ID** (`HQW_GITHUB_APP_ID`), нажмите **Generate a private key** (`HQW_GITHUB_APP_KEY`), затем **Install App** → **All repositories**.
5. Ваш числовой ID аккаунта GitHub идёт в `HQW_GITHUB_ADMIN_ID`: `curl -s https://api.github.com/users/<login> | grep '"id"'` (первый `id`). Ваши собственные действия не присылаются.

## 7. Переменные окружения

Секреты читаются один раз при запуске и удаляются из окружения. Docker берёт их из `.env`; служба без Docker использует `--env-file <путь>` с теми же строками. В файле конфигурации секретов нет никогда.

| Переменная | Нужна для | Формат |
|---|---|---|
| `HQW_TELEGRAM_TOKEN` | всегда | `<цифры>:<35+ символов>` |
| `HQW_TELEGRAM_ADMIN_ID` | всегда | положительное целое |
| `HQW_GITHUB_APP_ID` | GitHub | положительное целое |
| `HQW_GITHUB_APP_KEY` | GitHub | PEM; одной строкой с `\n` вместо переносов |
| `HQW_GITHUB_ADMIN_ID` | GitHub | положительное целое |
| `HQW_GITHUB_WEBHOOK_SECRET` | GitHub, режим вебхука | не меньше 32 байт случайных данных |
| `HQW_STEAM_PROFILE_ID` | руководства Steam | 17 цифр |
| `HQW_STEAM_API_KEY` | необязательно | 32 шестнадцатеричных символа |

Три переменные GitHub App задаются вместе или не задаются вовсе. PEM-ключ в одну строку: `awk 'NF {printf "%s\\n", $0}' key.pem`. Файл должен читаться только владельцем: `chmod 600 .env`.

## 8. Сервер

1. Установите Docker Engine и плагин Compose из [официального репозитория Docker](https://docs.docker.com/engine/install/debian/).
2. Скачайте из релиза архив образа для своего процессора и проверьте его:

   ```sh
   minisign -Vm SHA256SUMS.txt -P RWRh/ArZd0sNbyZPIKim+NSoQfeD97vdcfgJB4GQGpeZ8fIl4yWIrryu
   sha256sum -c --ignore-missing SHA256SUMS.txt
   docker load -i hq-watchman-<версия>-docker-amd64.tar.gz
   ```

3. Создайте папку и файлы:

   ```sh
   sudo install -d -m 750 /opt/hq-watchman
   cd /opt/hq-watchman
   sudo cp /путь/к/deploy/compose.yaml .
   sudo cp /путь/к/deploy/config.example.toml config.toml
   sudo install -m 600 /dev/null .env
   ```

4. При nginx на хосте укажите `webhook.trusted_proxies = ["172.31.250.1"]` (шлюз подсети Compose). В режиме опроса удалите строки `ports` из `compose.yaml`: входящие соединения боту не нужны.
5. Запуск: `docker compose up -d`, проверка: `docker compose ps` (состояние `healthy`). В Telegram придёт сообщение о запуске. Дальше настройки уведомлений меняются через `/settings`.

Контейнер работает от пользователя 65532 с корневой файловой системой только для чтения, без capabilities и с `no-new-privileges`.

## 9. Обратный прокси

Только для режима вебхука. Пример сайта nginx для `hooks.example.com` с сертификатом Let's Encrypt:

```nginx
server {
    listen 443 ssl;
    http2 on;
    server_name hooks.example.com;
    ssl_certificate     /etc/letsencrypt/live/hooks.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/hooks.example.com/privkey.pem;
    client_max_body_size 6m;
    location = /<сегмент пути> {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_read_timeout 30s;
    }
    location / { return 404; }
}
```

Без обратного прокси опубликуйте порт, подключите файлы сертификата только для чтения и задайте `[webhook.tls]` в `config.toml`.

## 10. Обновление

```sh
docker load -i hq-watchman-<версия>-docker-amd64.tar.gz
# укажите новый тег в compose.yaml
docker compose up -d
```

Миграции базы выполняются автоматически при запуске, перед миграцией делается резервная копия. HQ Watchman Setup делает то же самое и возвращает прежнюю версию, если новая не запустилась.

## 11. Смена секретов

После каждой смены перезапустите бота (`docker compose up -d`).

- **Токен Telegram**: в BotFather отправьте `/revoke`, новый токен впишите в `.env`.
- **Секрет вебхука GitHub**: задайте новый одновременно в настройках App и в `.env`. Неудачные доставки запросятся повторно ежечасной проверкой.
- **Закрытый ключ GitHub App**: создайте новый ключ, впишите в `.env`, перезапустите, затем удалите старый ключ на странице App.
- **Ключ Steam Web API**: отзовите и зарегистрируйте новый, впишите в `.env`.

## 12. Резервные копии и восстановление

Раз в сутки бот пишет копию базы рядом с ней (`hq-watchman.backup-<дата>.db`) и хранит последние 7. `hq-watchman backup` делает копию сразу. HQ Watchman Setup скачивает и восстанавливает копии одной кнопкой. Вручную:

```sh
docker compose stop
docker run --rm -v hq-watchman_data:/data alpine sh -c 'cp /data/hq-watchman.backup-<дата>.db /data/hq-watchman.db && rm -f /data/hq-watchman.db-wal /data/hq-watchman.db-shm && chown 65532:65532 /data/hq-watchman.db && chmod 600 /data/hq-watchman.db'
docker compose start
```
