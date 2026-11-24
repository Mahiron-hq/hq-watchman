# Design notes

## Structure

One async process (tokio). Sources write normalized events (`src/event.rs`) into an SQLite queue; a single sender reads the queue and talks to Telegram. Sources know nothing about Telegram, the sender knows nothing about sources.

| Module | Role |
|---|---|
| `config.rs` | Config file (strict TOML) and environment or file secrets, each source optional |
| `settings.rs` | User settings in the database, event filter and routing to chats |
| `i18n.rs` | Russian and English texts (`tr!`, `trf!`) |
| `github/webhook.rs` | Webhook endpoint: limits, IP filter, HMAC before parsing, idempotent storage |
| `github/payload.rs` | Webhook payloads into events, filters (own actions, bots, foreign installations) |
| `github/mod.rs`, `reconcile.rs` | App JWT, installation tokens, repositories, redelivery of failed deliveries |
| `github/poll.rs` | Polling mode: payloads read from the App delivery log |
| `steam/guides.rs`, `comments.rs` | Guide list (Web API, profile page as fallback), full comment snapshots, HTML parsers |
| `steam/games.rs`, `cm.rs`, `vdf.rs`, `bbcode.rs` | Builds, branches and DLC from PICS, news and patch notes |
| `storage.rs` | Schema, migrations, queue, snapshots, cleanup, backups |
| `telegram/sender.rs`, `render.rs` | Queue, digest, quiet hours, pause, limits, HTML and Markdown views |
| `telegram/commands.rs` | Long polling, admin commands, groups and channels, strangers |
| `telegram/menu.rs` | `/settings` screens and toggles |
| `steam/store.rs` | Store search and names for adding games |
| `health.rs`, `app.rs` | Source health and alerts, task supervision, maintenance, startup |

A new source is a new module that produces `Event` values.

`setup/` is a separate crate, the desktop app HQ Watchman Setup (Slint UI, russh for SSH). It uses the bot crate as a library for the settings types, validation and online checks, so both always agree on formats.

## Decisions

**Steam PICS: own minimal client.** The spec prefers a native Rust client. The only maintained crate, `steam-vent`, depends on `rsa` 0.9 (RUSTSEC-2023-0071, Marvin timing attack, no fix), pins `protobuf = 3.5.1`, pulls large generated protocol crates and has low adoption. The bot needs only anonymous logon, `ClientPICSAccessTokenRequest` and `ClientPICSProductInfoRequest`, so `steam/cm.rs` implements this subset over the WebSocket transport of the Steam CM servers (TLS via rustls, messages via `prost`). One short session per check, no credentials. SteamCMD was not needed.

**Telegram: thin own client** instead of teloxide. Few endpoints are used; an own client keeps the dependency tree small and guarantees that the token, which Telegram puts into the URL, never reaches logs: errors are built without URLs.

**Crypto: `ring`** for HMAC-SHA256 (constant-time `verify`), RS256 JWT signing and hashes, instead of the RustCrypto crates suggested by the spec. `ring` is already required by rustls, so this adds nothing to the tree and avoids the `rsa` crate advisory.

**Digest window: 20 seconds** (owner decision). The spec asked for a 60 s window and for GitHub notifications within 30 s, which conflict. Every event waits up to 20 s for others; more than 5 events in the window become one digest.

**Links inside external text.** Telegram turns URLs, domains, `@mentions`, `#hashtags`, `$cashtags` and `/commands` in plain text into entities on its own. The bot inserts a zero-width space into such tokens of external text (owner decision), so they stay plain text. Links in messages are built by the bot only, to GitHub and Steam over https.

**Image: `scratch`** with CA certificates, time zone data and a passwd entry copied from the pinned build image. It has no shell and no package manager. The binary is static (musl).

**Edition 2021.** The bot must remove secrets from its environment, and `std::env::remove_var` is `unsafe` in edition 2024 while the crate forbids `unsafe`. The call happens in `main` before any other thread starts, which is the condition that makes it sound.

**Deleted Steam comments** need two complete reads in a row without the comment. A read is complete only when the number of parsed comments equals the count Steam reports; anything else is treated as a parser or network problem, never as deletions.

**Settings in the database.** Everything the owner changes often (sources, event types, games, chats, quiet hours, time zone, language) is one JSON document in the `meta` table, edited by `/settings` or by `hq-watchman settings set` (the setup app). The running bot picks up outside changes within 5 seconds. `config.toml` keeps infrastructure only; its old keys (`timezone`, `[quiet_hours]`, `[games] appids`, `notify_*`) seed the settings once. Filtering happens when an event is queued, so a disabled type is never stored; turning a Steam source off forgets its snapshot, and turning it on starts from a silent baseline.

**Routing.** An event is queued once per recipient chat (admin, groups, channels); the dedup key gets the chat ID as suffix. Service messages go only to the admin. A chat that blocks the bot loses its queue; a group that becomes a supergroup is followed.

**GitHub polling mode.** The App webhook points to an address that never answers, so every delivery fails and stays in the App delivery log with its payload for 3 days. The bot reads new entries once a minute and passes the payload through the same parser as the webhook. No domain, certificate or open port is needed; events arrive within a minute.

**Windows target.** The bot runs as a Task Scheduler task under `LOCAL SERVICE`, started at boot and restarted on failure, instead of a Windows service: a service needs `unsafe` FFI glue, and the crate forbids `unsafe`.

**Releases** are signed: `SHA256SUMS.txt` carries a minisign signature (public key in `minisign.pub` and in the setup app). The setup app refuses files whose signature or checksum does not match.

## Known limits

- A Steam comment written and deleted between two checks is not seen.
- Comment pages of Steam are scraped; a markup change breaks the parser (the bot reports it after 3 failed checks).
- GitHub keeps webhook deliveries for 3 days; events older than that during downtime are lost (the bot reports the period).
- `/proc/self/environ` of the process still shows the environment it was started with; removing variables only hides them from child processes and from later reads.
- In polling mode GitHub events arrive with up to a minute of delay; if GitHub ever stops logging failed deliveries for an App, the mode stops working (the bot reports it as a source failure).
