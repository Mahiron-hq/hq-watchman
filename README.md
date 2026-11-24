<div align="center">

# HQ Watchman

**Telegram bot that tells its owner about GitHub, Steam guide and Steam game events**
<br>
**Telegram-бот, который сообщает владельцу о событиях GitHub, руководств и игр Steam**

[![CI](https://img.shields.io/github/actions/workflow/status/Mahiron-hq/hq-watchman/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/Mahiron-hq/hq-watchman/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-GPL--3.0--or--later-blue?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Docker-555?style=flat-square)](#install)
[![Rust](https://img.shields.io/badge/Rust-1.99-555?style=flat-square)](https://www.rust-lang.org/)

### [Releases](https://github.com/Mahiron-hq/hq-watchman/releases)

**[English](#english)** | **[Русский](#русский)**

</div>

---

| | |
|---|---|
| Platform | Linux, Docker (Debian 13 tested) |
| Version | 0.1.0 |
| License | GPL-3.0-or-later |
| Languages | Bot messages in Russian, docs in English and Russian |

## English

HQ Watchman is a self-hosted Telegram bot for one person. It sends the owner private messages about activity in their GitHub repositories, comments under their Steam guides and updates of chosen Steam games. Nobody else can control it or learn anything about it.

### Contents

1. [Features](#features)
2. [Install](#install)
3. [Usage](#usage)
4. [Build from source](#build-from-source)
5. [Security](#security)
6. [Changelog](#changelog)

### Features

- **GitHub** through a read-only GitHub App on all repositories, including private and future ones: new issues, pull requests and discussions, comments (new, edited, deleted), reviews, line comments, commit comments, new repositories. Your own actions and bots are filtered out. Missed webhooks are redelivered from the GitHub delivery log.
- **Steam guides** of one profile: new and deleted comments with the full text, new and removed guides. A deletion is reported only after two complete reads.
- **Steam games**: public build changes, beta branches, DLC (soundtracks excluded), news and patch notes from the developer feed. A build and its patch note arrive as one thread.
- **Delivery**: queue in SQLite, nothing is lost on restart, no duplicates. Long texts come as a Markdown file, bursts as one digest. Quiet hours with silent messages, pause with automatic resume.
- **Safety**: authorization by numeric Telegram ID, HMAC check before parsing, strict limits on every input, external text escaped and stripped of control and bidi characters, TLS via rustls only, no `unsafe`, non-root read-only container without a shell.
- **Self-monitoring**: one message when a source fails and one when it recovers.

### Install

Step-by-step guide: [docs/install.en.md](docs/install.en.md). In short:

1. Create a Telegram bot, a Steam Web API key and a private read-only GitHub App.
2. Load the image from the CI artifact or a release and check `SHA256SUMS.txt`.
3. Copy [`deploy/compose.yaml`](deploy/compose.yaml) and [`deploy/config.example.toml`](deploy/config.example.toml), fill `.env` with the variables from [`deploy/.env.example`](deploy/.env.example).
4. `docker compose up -d` behind an HTTPS reverse proxy.

### Usage

| Command | What it does |
|---|---|
| `/help` | List of commands |
| `/status` | Version, uptime, state of every source, pause, queue size, quiet hours |
| `/sources` | Watched repositories, guides, games with branches and DLC |
| `/pause [2h]` | Pause notifications, forever or for a time (`30m`, `2h`, `1d`, `1w`) |
| `/resume` | Resume; collected events arrive, as a digest if there are many |
| `/check_now [github\|guides\|games]` | Check now, at most once a minute per source |
| `/reload` | Re-read the config file without a restart |
| `/strangers` | Who else tried to write to the bot in the last 7 days |

Games are added and removed in `config.toml` followed by `/reload`. Repositories and guides are picked up automatically.

### Build from source

Requires Rust 1.99 (pinned in `rust-toolchain.toml`).

```sh
cargo build --release --locked
cargo test --locked
docker build -t hq-watchman:0.1.0 .
```

Queries are checked at compile time against the `.sqlx` data in the repository, so no database is needed to build. Design notes: [docs/design.md](docs/design.md).

### Security

Report vulnerabilities privately, see [SECURITY.md](SECURITY.md).

### Changelog

See [CHANGELOG.md](CHANGELOG.md).

## Русский

HQ Watchman – Telegram-бот для одного человека, который работает на своём сервере. Он пишет владельцу в личные сообщения о событиях в его репозиториях GitHub, о комментариях под его руководствами Steam и об обновлениях выбранных игр Steam. Никто другой не может им управлять или что-то о нём узнать.

### Содержание

1. [Возможности](#возможности)
2. [Установка](#установка)
3. [Использование](#использование)
4. [Сборка из исходников](#сборка-из-исходников)
5. [Безопасность](#безопасность)
6. [Изменения](#изменения)

### Возможности

- **GitHub** через GitHub App только на чтение на всех репозиториях, включая приватные и будущие: новые Issue, Pull Request и обсуждения, комментарии (новые, изменённые, удалённые), ревью, комментарии к коду, комментарии к коммитам, новые репозитории. Ваши собственные действия и боты отфильтрованы. Пропущенные вебхуки запрашиваются повторно из журнала доставок GitHub.
- **Руководства Steam** одного профиля: новые и удалённые комментарии с полным текстом, новые и пропавшие руководства. Об удалении бот сообщает только после двух полных чтений.
- **Игры Steam**: смена билда ветки public, бета-ветки, DLC (без саундтреков), новости и патчноуты из канала разработчика. Билд и его патчноут приходят одной веткой сообщений.
- **Доставка**: очередь в SQLite, при перезапуске ничего не теряется и не дублируется. Длинные тексты приходят файлом Markdown, всплески событий одним дайджестом. Тихие часы с беззвучными сообщениями, пауза с автоматическим снятием.
- **Безопасность**: авторизация по числовому ID Telegram, проверка HMAC до разбора, строгие лимиты на всех входах, внешний текст экранируется и очищается от управляющих символов и символов направления текста, TLS только через rustls, без `unsafe`, контейнер без root, без оболочки и с файловой системой только для чтения.
- **Самоконтроль**: одно сообщение при сбое источника и одно при восстановлении.

### Установка

Пошаговая инструкция: [docs/install.ru.md](docs/install.ru.md). Коротко:

1. Создайте Telegram-бота, ключ Steam Web API и приватное GitHub App только на чтение.
2. Загрузите образ из артефакта CI или релиза и проверьте `SHA256SUMS.txt`.
3. Скопируйте [`deploy/compose.yaml`](deploy/compose.yaml) и [`deploy/config.example.toml`](deploy/config.example.toml), заполните `.env` переменными из [`deploy/.env.example`](deploy/.env.example).
4. `docker compose up -d` за обратным прокси с HTTPS.

### Использование

| Команда | Что делает |
|---|---|
| `/help` | Список команд |
| `/status` | Версия, время работы, состояние каждого источника, пауза, очередь, тихие часы |
| `/sources` | Отслеживаемые репозитории, руководства, игры с ветками и DLC |
| `/pause [2h]` | Пауза уведомлений, без срока или на время (`30m`, `2h`, `1d`, `1w`) |
| `/resume` | Снять паузу; накопленное придёт, дайджестом, если событий много |
| `/check_now [github\|guides\|games]` | Проверить сейчас, не чаще раза в минуту на источник |
| `/reload` | Перечитать конфиг без перезапуска |
| `/strangers` | Кто ещё пытался писать боту за 7 дней |

Игры добавляются и удаляются в `config.toml`, затем `/reload`. Репозитории и руководства подключаются сами.

### Сборка из исходников

Нужен Rust 1.99 (закреплён в `rust-toolchain.toml`).

```sh
cargo build --release --locked
cargo test --locked
docker build -t hq-watchman:0.1.0 .
```

Запросы к базе проверяются при компиляции по данным `.sqlx` из репозитория, поэтому база для сборки не нужна. Заметки об устройстве: [docs/design.md](docs/design.md).

### Безопасность

Об уязвимостях сообщайте закрыто, см. [SECURITY.md](SECURITY.md).

### Изменения

См. [CHANGELOG.md](CHANGELOG.md).

---

## License

GPL-3.0-or-later, see [LICENSE](LICENSE).

<div align="center">

Copyright © 2026 Lev Burmistrov (Mahiron-hq)
<br>
Mahiron.HQ@protonmail.com | t.me/mahiron_hq

</div>
