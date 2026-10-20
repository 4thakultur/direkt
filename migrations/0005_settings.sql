-- Runtime settings edited through the web UI or API (key/value).
-- May hold secrets (e.g. provider API keys): never logged or returned.
CREATE TABLE settings (
    key         TEXT    PRIMARY KEY NOT NULL,
    value       TEXT    NOT NULL,
    updated_at  INTEGER NOT NULL
);
