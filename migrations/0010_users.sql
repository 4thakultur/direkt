-- Several accounts instead of one administrator. The existing administrator
-- becomes user 1, and keeps their sessions, API tokens, two-factor secret,
-- recovery codes and downloads.

CREATE TABLE users (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    username              TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash         TEXT    NOT NULL,   -- Argon2id PHC string
    role                  TEXT    NOT NULL CHECK (role IN ('admin', 'user')),
    -- JSON list of permission names; ignored for administrators.
    permissions           TEXT    NOT NULL DEFAULT '[]',
    -- Set while the password is a temporary one from an administrator: the
    -- next sign-in must choose a new one.
    must_change_password  INTEGER NOT NULL DEFAULT 0,
    -- When the temporary password stops working.
    password_expires_at   INTEGER,
    -- An administrator requires two-factor authentication for this account.
    require_two_factor    INTEGER NOT NULL DEFAULT 0,
    disabled              INTEGER NOT NULL DEFAULT 0,
    -- Base32 TOTP secret while two-factor authentication is on.
    totp_secret           TEXT,
    -- The last 30-second step whose code was accepted.
    totp_last_step        INTEGER NOT NULL DEFAULT 0,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL,
    last_login_at         INTEGER
);

INSERT INTO users (id, username, password_hash, role, totp_secret, totp_last_step,
                   created_at, updated_at)
SELECT 1, username, password_hash, 'admin', totp_secret, totp_last_step, updated_at, updated_at
FROM admin;

DROP TABLE admin;

-- Sessions, API tokens and recovery codes belong to a user. Recreated, since
-- SQLite cannot add a NOT NULL foreign key column.
CREATE TABLE sessions_new (
    token_hash  TEXT    PRIMARY KEY NOT NULL,
    user_id     INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
);
INSERT INTO sessions_new (token_hash, user_id, created_at, expires_at)
SELECT token_hash, 1, created_at, expires_at FROM sessions
WHERE EXISTS (SELECT 1 FROM users WHERE id = 1);
DROP TABLE sessions;
ALTER TABLE sessions_new RENAME TO sessions;
CREATE INDEX sessions_expires_idx ON sessions (expires_at);
CREATE INDEX sessions_user_idx ON sessions (user_id);

CREATE TABLE api_tokens_new (
    id            TEXT    PRIMARY KEY NOT NULL,   -- UUID
    user_id       INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name          TEXT    NOT NULL,
    token_hash    TEXT    NOT NULL UNIQUE,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER
);
INSERT INTO api_tokens_new (id, user_id, name, token_hash, created_at, last_used_at)
SELECT id, 1, name, token_hash, created_at, last_used_at FROM api_tokens
WHERE EXISTS (SELECT 1 FROM users WHERE id = 1);
DROP TABLE api_tokens;
ALTER TABLE api_tokens_new RENAME TO api_tokens;
CREATE INDEX api_tokens_user_idx ON api_tokens (user_id);

CREATE TABLE recovery_codes_new (
    code_hash   TEXT    NOT NULL,
    user_id     INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (user_id, code_hash)
);
INSERT INTO recovery_codes_new (code_hash, user_id, created_at)
SELECT code_hash, 1, created_at FROM recovery_codes
WHERE EXISTS (SELECT 1 FROM users WHERE id = 1);
DROP TABLE recovery_codes;
ALTER TABLE recovery_codes_new RENAME TO recovery_codes;

-- The account that added a download. NULL once that account is deleted;
-- the download itself stays.
ALTER TABLE downloads ADD COLUMN owner_id INTEGER REFERENCES users (id) ON DELETE SET NULL;
UPDATE downloads SET owner_id = 1 WHERE EXISTS (SELECT 1 FROM users WHERE id = 1);
CREATE INDEX downloads_owner_idx ON downloads (owner_id, created_at);
