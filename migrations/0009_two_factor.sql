-- Two-factor sign-in with an authenticator app (TOTP, RFC 6238).
-- The shared secret must be readable to check codes, so it is stored as is
-- (base32); NULL while two-factor authentication is off.
ALTER TABLE admin ADD COLUMN totp_secret TEXT;
-- The last 30-second step whose code was accepted, so a code cannot be
-- used twice.
ALTER TABLE admin ADD COLUMN totp_last_step INTEGER NOT NULL DEFAULT 0;

-- Single-use recovery codes for a lost authenticator. Only a SHA-256 hash of
-- each code is stored; the codes are shown once.
CREATE TABLE recovery_codes (
    code_hash   TEXT    PRIMARY KEY NOT NULL,
    created_at  INTEGER NOT NULL
);
