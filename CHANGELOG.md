# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Telegram notifications for the single admin: GitHub App webhooks (issues, pull requests, reviews, comments, discussions, new repositories), Steam guide comments (new and deleted), Steam game updates (builds, branches, DLC, news and patch notes).
- Admin commands: `/help`, `/status`, `/sources`, `/pause`, `/resume`, `/check_now`, `/reload`, `/strangers`.
- Digest with a Markdown file, quiet hours, pause, delivery queue with retries.
- Hardened Docker image and Compose file, CI with format, Clippy, tests, cargo-deny, image scan, SBOM and secret search.
