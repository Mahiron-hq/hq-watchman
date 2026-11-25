# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow [Semantic Versioning](https://semver.org/).

## [0.1.2] - 2026-11-25

### Fixed

- Steam DLC info is read again: Steam sends DLC info without the access token, and 0.1.1 skipped it, so a new DLC came without its name and soundtracks were not recognized. Only a game itself answered without its token is skipped now.

## [0.1.1] - 2026-11-25

### Security

- Setup: installing after changing the server address, port, user or key no longer uses the connection tested earlier; the connection has to be tested again.
- Setup: on a Linux server, files on their way into place go through a folder only root can write instead of `/tmp`.
- Setup on Windows: the user account may only read the installation folder and change the data folder, so it can no longer replace the program that runs as LOCAL SERVICE. The administrator script is passed on the command line, and staged files are checked by SHA-256 after they are copied into a protected folder.
- Setup: the bot owner is set by a one-time code (sent by the Start button or typed into the bot chat), so another person who sends `/start` first is not taken as the owner. Editing the bot token resets the bot check.
- Steam app info is downloaded only from Steam hosts, and inline app info is checked against its size and SHA-1.
- Webhook IP filter: while the GitHub address ranges are not loaded, deliveries get 503 and are redelivered later instead of being accepted from any address.

### Fixed

- A long message sent as several files no longer repeats the files already sent when a later one fails; the rest is sent as a reply to the first file.
- A Telegram update whose handling failed is fetched again (up to three times) instead of being lost.
- A GitHub delivery and the repository list change it brings are saved in one transaction.
- Repository, delivery, installation and guide lists longer than the page limit no longer make the bot treat the rest as removed or skip unread deliveries.
- A repository added by a webhook during reconciliation is no longer reported as removed.
- Every reopen of an issue or pull request is reported, not only the first.
- A Steam news item no longer comes again after the retention period, and the first read of a game's news is recorded together with the news.
- A failed daily backup is retried in an hour instead of the next day.
- `/reload` keeps the running values of settings that need a restart (all webhook settings, `github.mode`, health address, database path, logging), lists them and reminds that secrets are read only on restart.
- A DLC missing from a Steam answer keeps its known state instead of turning into an empty "DLC <id>" with its content removed.
- A group or channel where Telegram no longer lets the bot post is removed from delivery, and the admin is told.
- Buttons of an old repository list in `/settings` no longer switch a different repository.
- Setup: restore checks the database file first (integrity, schema not newer than the program), keeps the previous database in the data folder and puts it back if the bot does not start.
- Setup: an update that fails before the health check is rolled back too.

### Changed

- Release builds start only for a tag that matches the program version on `main` and after the full CI passes; release files get GitHub build provenance attestations.
- Fuzzing runs weekly; the setup app is linted and tested on Windows and macOS in CI.

## [0.1.0] - 2026-11-25

### Added

- Telegram notifications for the single admin: GitHub App webhooks (issues, pull requests, reviews, comments, discussions, new repositories), Steam guide comments (new and deleted), Steam game updates (builds, branches, DLC, news and patch notes).
- Admin commands: `/settings`, `/help`, `/status`, `/sources`, `/pause`, `/resume`, `/check_now`, `/reload`, `/strangers`.
- Digest with a Markdown file, quiet hours, pause, delivery queue with retries.
- Hardened Docker image and Compose file, CI with format, Clippy, tests, cargo-deny, image scan, SBOM and secret search.
- HQ Watchman Setup, a desktop app for Windows, macOS and Linux: installs the bot on a Linux server (Docker or systemd) or on this Windows computer, creates the GitHub App, checks Telegram, GitHub and Steam access, edits settings, shows logs, updates with rollback, backs up and restores the database, removes the bot.
- Sources are optional: only the Telegram variables are required; GitHub and Steam guides start when their variables are set.
- Settings menu `/settings` with buttons: sources and event types, excluded repositories, games with per-game choice (builds, beta branches, DLC, news, patch notes), quiet hours, time zone, language. Games are added by name, store link or AppID.
- Delivery to groups and channels the admin adds the bot to, chosen per source.
- GitHub polling mode that reads the App delivery log: no domain or open port needed.
- Bot messages in Russian or English.
- Commands `check`, `settings get|set`, `backup` and options `--env-file`, `--log-file`.
- Release files for Linux x86_64 and arm64 (Docker images and static binaries) and Windows x64, signed with minisign.
