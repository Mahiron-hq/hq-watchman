# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
