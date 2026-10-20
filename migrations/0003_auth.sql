-- Single administrator account (row id is always 1).
CREATE TABLE admin (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    username       TEXT    NOT NULL,
    password_hash  TEXT    NOT NULL,   -- Argon2id PHC string
    updated_at     INTEGER NOT NULL
);

-- Browser sessions. Only a SHA-256 hash of the session token is stored.
CREATE TABLE sessions (
    token_hash  TEXT    PRIMARY KEY NOT NULL,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
);

CREATE INDEX sessions_expires_idx ON sessions (expires_at);

-- API bearer tokens. Only a SHA-256 hash of the token is stored; the token
-- itself is shown once at creation.
CREATE TABLE api_tokens (
    id            TEXT    PRIMARY KEY NOT NULL,   -- UUID
    name          TEXT    NOT NULL,
    token_hash    TEXT    NOT NULL UNIQUE,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER
);
