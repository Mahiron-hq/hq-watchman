ALTER TABLE outbox ADD COLUMN chat_id INTEGER;

DROP INDEX outbox_merge;
CREATE INDEX outbox_merge ON outbox(merge_app, merge_kind, chat_id, occurred_at);
