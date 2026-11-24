CREATE TABLE meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE outbox (
    id INTEGER PRIMARY KEY NOT NULL,
    dedup_key TEXT NOT NULL UNIQUE,
    kind INTEGER NOT NULL,
    source TEXT NOT NULL,
    body TEXT NOT NULL,
    occurred_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    status INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL DEFAULT 0,
    sent_at INTEGER,
    message_id INTEGER,
    merge_app INTEGER,
    merge_kind TEXT,
    reply_to INTEGER REFERENCES outbox(id) ON DELETE SET NULL
) STRICT;

CREATE INDEX outbox_status ON outbox(status, next_attempt_at);
CREATE INDEX outbox_merge ON outbox(merge_app, merge_kind, occurred_at);

CREATE TABLE deliveries (
    guid TEXT PRIMARY KEY NOT NULL,
    received_at INTEGER NOT NULL
) STRICT;

CREATE TABLE redeliveries (
    guid TEXT PRIMARY KEY NOT NULL,
    requested_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL
) STRICT;

CREATE TABLE installations (
    id INTEGER PRIMARY KEY NOT NULL,
    account_login TEXT NOT NULL,
    suspended INTEGER NOT NULL
) STRICT;

CREATE TABLE repos (
    id INTEGER PRIMARY KEY NOT NULL,
    full_name TEXT NOT NULL,
    private INTEGER NOT NULL,
    installation_id INTEGER NOT NULL
) STRICT;

CREATE TABLE guides (
    id INTEGER PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    game TEXT NOT NULL,
    comment_count INTEGER NOT NULL,
    missing_checks INTEGER NOT NULL DEFAULT 0,
    last_full_read INTEGER,
    baseline_done INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE guide_comments (
    guide_id INTEGER NOT NULL REFERENCES guides(id) ON DELETE CASCADE,
    comment_id TEXT NOT NULL,
    author TEXT NOT NULL,
    author_url TEXT NOT NULL,
    author_steamid INTEGER NOT NULL,
    posted_at INTEGER NOT NULL,
    text TEXT NOT NULL,
    missing_reads INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (guide_id, comment_id)
) STRICT;

CREATE TABLE games (
    appid INTEGER PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    state TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE news_seen (
    appid INTEGER NOT NULL,
    gid TEXT NOT NULL,
    seen_at INTEGER NOT NULL,
    PRIMARY KEY (appid, gid)
) STRICT;

CREATE TABLE strangers (
    user_id INTEGER PRIMARY KEY NOT NULL,
    username TEXT,
    attempts INTEGER NOT NULL,
    last_at INTEGER NOT NULL,
    last_reply_at INTEGER
) STRICT;
