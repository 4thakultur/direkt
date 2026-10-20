//! Persistence for the admin account, two-factor secret and recovery codes,
//! browser sessions, and API tokens.
//!
//! The password is an Argon2id hash, and session tokens, API tokens and
//! recovery codes are stored as SHA-256 hashes. They are high-entropy random
//! values, so a fast hash is enough. The two-factor secret is the exception:
//! checking a code needs it, so it is stored as is.

use super::{Store, from_millis};
use crate::error::{AppError, AppResult};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminAccount {
    pub username: String,
    pub password_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct ApiToken {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

impl Store {
    pub async fn admin(&self) -> AppResult<Option<AdminAccount>> {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT username, password_hash FROM admin WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(username, password_hash)| AdminAccount {
            username,
            password_hash,
        }))
    }

    /// Create the admin account. Fails with `Conflict` if one already exists,
    /// so a setup race cannot replace an existing administrator.
    pub async fn create_admin(&self, account: &AdminAccount, now: DateTime<Utc>) -> AppResult<()> {
        let result = sqlx::query(
            "INSERT INTO admin (id, username, password_hash, updated_at) VALUES (1, ?, ?, ?)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&account.username)
        .bind(&account.password_hash)
        .bind(now.timestamp_millis())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::conflict(
                "an administrator account already exists",
            ));
        }
        Ok(())
    }

    pub async fn update_admin_password(
        &self,
        password_hash: &str,
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        sqlx::query("UPDATE admin SET password_hash = ?, updated_at = ? WHERE id = 1")
            .bind(password_hash)
            .bind(now.timestamp_millis())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The base32 TOTP secret, while two-factor authentication is on.
    pub async fn totp_secret(&self) -> AppResult<Option<String>> {
        let secret: Option<Option<String>> =
            sqlx::query_scalar("SELECT totp_secret FROM admin WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(secret.flatten())
    }

    /// Turn on two-factor authentication with a new secret and recovery
    /// codes. `step` is the step of the code that confirmed the secret, so
    /// that code cannot then be used to sign in.
    pub async fn enable_two_factor(
        &self,
        secret: &str,
        step: i64,
        recovery_hashes: &[String],
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE admin SET totp_secret = ?, totp_last_step = ? WHERE id = 1")
            .bind(secret)
            .bind(step)
            .execute(&mut *tx)
            .await?;
        replace_recovery_codes(&mut tx, recovery_hashes, now).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Turn off two-factor authentication and forget the recovery codes.
    /// Returns whether it was on.
    pub async fn disable_two_factor(&self) -> AppResult<bool> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE admin SET totp_secret = NULL, totp_last_step = 0
             WHERE id = 1 AND totp_secret IS NOT NULL",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM recovery_codes")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Record that the code for `step` was used. Returns false if that step
    /// or a later one was already used (a replayed code).
    pub async fn use_totp_step(&self, step: i64) -> AppResult<bool> {
        let result = sqlx::query(
            "UPDATE admin SET totp_last_step = ?
             WHERE id = 1 AND totp_secret IS NOT NULL AND totp_last_step < ?",
        )
        .bind(step)
        .bind(step)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Use up a recovery code. Returns whether it existed.
    pub async fn use_recovery_code(&self, code_hash: &str) -> AppResult<bool> {
        let result = sqlx::query("DELETE FROM recovery_codes WHERE code_hash = ?")
            .bind(code_hash)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn set_recovery_codes(
        &self,
        recovery_hashes: &[String],
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        replace_recovery_codes(&mut tx, recovery_hashes, now).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn recovery_codes_left(&self) -> AppResult<u32> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recovery_codes")
            .fetch_one(&self.pool)
            .await?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    /// Remove the admin account and every session and API token. Used for
    /// credential recovery; the next start offers first-run setup again.
    pub async fn reset_admin(&self) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM admin").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM recovery_codes")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM api_tokens")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn insert_session(
        &self,
        token_hash: &str,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> AppResult<()> {
        sqlx::query("INSERT INTO sessions (token_hash, created_at, expires_at) VALUES (?, ?, ?)")
            .bind(token_hash)
            .bind(now.timestamp_millis())
            .bind(expires_at.timestamp_millis())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn session_valid(&self, token_hash: &str, now: DateTime<Utc>) -> AppResult<bool> {
        let found: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM sessions WHERE token_hash = ? AND expires_at > ?")
                .bind(token_hash)
                .bind(now.timestamp_millis())
                .fetch_optional(&self.pool)
                .await?;
        Ok(found.is_some())
    }

    pub async fn delete_session(&self, token_hash: &str) -> AppResult<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_all_sessions(&self) -> AppResult<()> {
        sqlx::query("DELETE FROM sessions")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// End every session except one.
    pub async fn delete_other_sessions(&self, keep_hash: &str) -> AppResult<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash != ?")
            .bind(keep_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn prune_sessions(&self, now: DateTime<Utc>) -> AppResult<u64> {
        let result = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
            .bind(now.timestamp_millis())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn insert_api_token(
        &self,
        name: &str,
        token_hash: &str,
        now: DateTime<Utc>,
    ) -> AppResult<ApiToken> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO api_tokens (id, name, token_hash, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(name)
        .bind(token_hash)
        .bind(now.timestamp_millis())
        .execute(&self.pool)
        .await?;
        Ok(ApiToken {
            id,
            name: name.to_owned(),
            created_at: from_millis(now.timestamp_millis())?,
            last_used_at: None,
        })
    }

    /// Look up a token by hash and record its use. Returns the token's id.
    pub async fn use_api_token(
        &self,
        token_hash: &str,
        now: DateTime<Utc>,
    ) -> AppResult<Option<Uuid>> {
        let id: Option<String> = sqlx::query_scalar(
            "UPDATE api_tokens SET last_used_at = ? WHERE token_hash = ? RETURNING id",
        )
        .bind(now.timestamp_millis())
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        id.map(|id| Uuid::parse_str(&id).map_err(|_| super::corrupt("invalid token id")))
            .transpose()
    }

    pub async fn list_api_tokens(&self) -> AppResult<Vec<ApiToken>> {
        let rows: Vec<TokenRow> = sqlx::query_as(
            "SELECT id, name, created_at, last_used_at FROM api_tokens ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Returns whether a token was deleted.
    pub async fn delete_api_token(&self, id: Uuid) -> AppResult<bool> {
        let result = sqlx::query("DELETE FROM api_tokens WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }
}

async fn replace_recovery_codes(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    hashes: &[String],
    now: DateTime<Utc>,
) -> AppResult<()> {
    sqlx::query("DELETE FROM recovery_codes")
        .execute(&mut **tx)
        .await?;
    for hash in hashes {
        sqlx::query("INSERT INTO recovery_codes (code_hash, created_at) VALUES (?, ?)")
            .bind(hash)
            .bind(now.timestamp_millis())
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

#[derive(FromRow)]
struct TokenRow {
    id: String,
    name: String,
    created_at: i64,
    last_used_at: Option<i64>,
}

impl TryFrom<TokenRow> for ApiToken {
    type Error = AppError;

    fn try_from(r: TokenRow) -> AppResult<Self> {
        Ok(Self {
            id: Uuid::parse_str(&r.id).map_err(|_| super::corrupt("invalid token id"))?,
            name: r.name,
            created_at: from_millis(r.created_at)?,
            last_used_at: r.last_used_at.map(from_millis).transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn account() -> AdminAccount {
        AdminAccount {
            username: "admin".into(),
            password_hash: "$argon2id$fake".into(),
        }
    }

    #[tokio::test]
    async fn admin_can_only_be_created_once() {
        let s = Store::open_in_memory().await.unwrap();
        assert_eq!(s.admin().await.unwrap(), None);
        s.create_admin(&account(), t(0)).await.unwrap();
        let other = AdminAccount {
            username: "mallory".into(),
            password_hash: "x".into(),
        };
        let err = s.create_admin(&other, t(1)).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert_eq!(s.admin().await.unwrap(), Some(account()));
    }

    #[tokio::test]
    async fn sessions_expire_and_reset_clears_everything() {
        let s = Store::open_in_memory().await.unwrap();
        s.create_admin(&account(), t(0)).await.unwrap();
        s.insert_session("h1", t(0), t(100)).await.unwrap();
        assert!(s.session_valid("h1", t(99)).await.unwrap());
        assert!(!s.session_valid("h1", t(100)).await.unwrap());
        assert!(!s.session_valid("other", t(0)).await.unwrap());
        assert_eq!(s.prune_sessions(t(100)).await.unwrap(), 1);

        s.insert_session("h2", t(0), t(100)).await.unwrap();
        s.insert_api_token("cli", "th", t(0)).await.unwrap();
        s.reset_admin().await.unwrap();
        assert_eq!(s.admin().await.unwrap(), None);
        assert!(!s.session_valid("h2", t(1)).await.unwrap());
        assert!(s.list_api_tokens().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn two_factor_steps_and_recovery_codes() {
        let s = Store::open_in_memory().await.unwrap();
        s.create_admin(&account(), t(0)).await.unwrap();
        assert_eq!(s.totp_secret().await.unwrap(), None);
        assert!(
            !s.use_totp_step(5).await.unwrap(),
            "off: no step is accepted"
        );

        let codes = vec!["r1".to_owned(), "r2".to_owned()];
        s.enable_two_factor("SECRET", 10, &codes, t(0))
            .await
            .unwrap();
        assert_eq!(s.totp_secret().await.unwrap().as_deref(), Some("SECRET"));
        assert!(
            !s.use_totp_step(10).await.unwrap(),
            "confirming step is used"
        );
        assert!(s.use_totp_step(11).await.unwrap());
        assert!(!s.use_totp_step(11).await.unwrap(), "replay");
        assert!(!s.use_totp_step(10).await.unwrap(), "older step");

        assert_eq!(s.recovery_codes_left().await.unwrap(), 2);
        assert!(s.use_recovery_code("r1").await.unwrap());
        assert!(!s.use_recovery_code("r1").await.unwrap(), "single use");
        s.set_recovery_codes(&["r3".to_owned()], t(1))
            .await
            .unwrap();
        assert!(
            !s.use_recovery_code("r2").await.unwrap(),
            "old codes replaced"
        );
        assert_eq!(s.recovery_codes_left().await.unwrap(), 1);

        assert!(s.disable_two_factor().await.unwrap());
        assert!(!s.disable_two_factor().await.unwrap());
        assert_eq!(s.totp_secret().await.unwrap(), None);
        assert_eq!(s.recovery_codes_left().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn other_sessions_can_be_ended() {
        let s = Store::open_in_memory().await.unwrap();
        s.insert_session("keep", t(0), t(100)).await.unwrap();
        s.insert_session("drop", t(0), t(100)).await.unwrap();
        s.delete_other_sessions("keep").await.unwrap();
        assert!(s.session_valid("keep", t(1)).await.unwrap());
        assert!(!s.session_valid("drop", t(1)).await.unwrap());
    }

    #[tokio::test]
    async fn api_token_lifecycle() {
        let s = Store::open_in_memory().await.unwrap();
        let token = s.insert_api_token("sonarr", "hash1", t(0)).await.unwrap();
        assert_eq!(
            s.use_api_token("hash1", t(5)).await.unwrap(),
            Some(token.id)
        );
        assert_eq!(s.use_api_token("nope", t(5)).await.unwrap(), None);
        let listed = s.list_api_tokens().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].last_used_at, Some(t(5)));
        assert!(s.delete_api_token(token.id).await.unwrap());
        assert!(!s.delete_api_token(token.id).await.unwrap());
        assert_eq!(s.use_api_token("hash1", t(6)).await.unwrap(), None);
    }
}
