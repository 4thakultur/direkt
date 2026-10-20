-- All timestamps are Unix epoch milliseconds (UTC).

CREATE TABLE downloads (
    id                  TEXT    PRIMARY KEY NOT NULL,  -- UUID
    url                 TEXT    NOT NULL,              -- as submitted; may contain tokens
    display_url         TEXT    NOT NULL,              -- redacted, safe to show/log
    provider            TEXT    NOT NULL,
    state               TEXT    NOT NULL CHECK (state IN
                            ('queued','downloading','retrying','paused','blocked',
                             'completed','failed','cancelled')),
    post_state          TEXT    NOT NULL DEFAULT 'none' CHECK (post_state IN
                            ('none','pending','processing','completed','failed','skipped')),
    filename            TEXT,
    total_bytes         INTEGER,
    downloaded_bytes    INTEGER NOT NULL DEFAULT 0,
    retry_count         INTEGER NOT NULL DEFAULT 0,
    next_attempt_at     INTEGER,
    error_kind          TEXT,
    error_message       TEXT,
    post_error_kind     TEXT,
    post_error_message  TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    completed_at        INTEGER
);

CREATE INDEX downloads_state_idx ON downloads (state, created_at);
CREATE INDEX downloads_post_state_idx ON downloads (post_state);

CREATE TABLE download_events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    download_id  TEXT    NOT NULL REFERENCES downloads (id) ON DELETE CASCADE,
    at           INTEGER NOT NULL,
    kind         TEXT    NOT NULL,
    message      TEXT    NOT NULL
);

CREATE INDEX download_events_download_idx ON download_events (download_id, id);
