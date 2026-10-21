//! Persistence for accounts, their two-factor secrets and recovery codes,
//! browser sessions, and API tokens.
//!
//! Passwords are Argon2id hashes, and session tokens, API tokens and recovery
//! codes are stored as SHA-256 hashes. They are high-entropy random values,
//! so a fast hash is enough. The two-factor secret is the exception: checking
//! a code needs it, so it is stored as is.
//!
//! At least one enabled administrator must always remain: changes that would
//! leave none are refused inside the same transaction, after the write, so
//! two concurrent changes cannot both pass the check.

use super::{Store, from_millis};
use crate::error::{AppError, AppResult};
use crate::users::{Permission, Role, User, normalize_permissions};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

/// An account with its password hash, for signing in.
#[derive(Debug, Clone)]
pub struct UserRecord {
    pub user: User,
    pub password_hash: String,
}

/// A new account.
#[derive(Debug, Clone)]
pub struct NewUser {
    pub username: String,
    pub password_hash: String,
    pub role: Role,
    pub permissions: Vec<Permission>,
    pub require_two_factor: bool,
    /// The password is temporary until this time: it must be changed at the
    /// next sign-in. `None` for a password the user chose.
    pub temporary_until: Option<DateTime<Utc>>,
}

/// Changes to an account. Fields left `None` keep their value.
#[derive(Debug, Clone, Default)]
pub struct UserChanges {
    pub role: Option<Role>,
    pub permissions: Option<Vec<Permission>>,
    pub require_two_factor: Option<bool>,
    pub disabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct ApiToken {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// A static `SELECT` of the columns [`UserRow`] reads, followed by `$rest`.
macro_rules! select_users {
    ($rest:literal) => {
        concat!(
            "SELECT id, username, password_hash, role, permissions, must_change_password,
                password_expires_at, require_two_factor, disabled,
                totp_secret IS NOT NULL AS two_factor_enabled, created_at, last_login_at
             FROM users ",
            $rest
        )
    };
}

impl Store {
    /// Whether an enabled administrator exists.
    pub async fn has_admin(&self) -> AppResult<bool> {
        let found: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM users WHERE role = 'admin' AND disabled = 0 LIMIT 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(found.is_some())
    }

    /// Create the first administrator. Fails with `Conflict` if an enabled
    /// administrator already exists, so a setup race cannot add a second.
    pub async fn create_first_admin(&self, new: &NewUser, now: DateTime<Utc>) -> AppResult<User> {
        let mut tx = self.pool.begin().await?;
        let id = insert_user(&mut tx, new, now).await?;
        let admins: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0")
                .fetch_one(&mut *tx)
                .await?;
        if admins != 1 {
            tx.rollback().await?;
            return Err(AppError::conflict(
                "an administrator account already exists",
            ));
        }
        tx.commit().await?;
        self.user(id).await?.map(|r| r.user).ok_or_else(missing)
    }

    /// Create an account. Usernames are unique, ignoring case.
    pub async fn create_user(&self, new: &NewUser, now: DateTime<Utc>) -> AppResult<User> {
        let mut tx = self.pool.begin().await?;
        let id = insert_user(&mut tx, new, now).await?;
        tx.commit().await?;
        self.user(id).await?.map(|r| r.user).ok_or_else(missing)
    }

    pub async fn user(&self, id: i64) -> AppResult<Option<UserRecord>> {
        let row: Option<UserRow> = sqlx::query_as(select_users!("WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Look an account up by name, ignoring case.
    pub async fn user_by_name(&self, username: &str) -> AppResult<Option<UserRecord>> {
        let row: Option<UserRow> = sqlx::query_as(select_users!("WHERE username = ?"))
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Every account: administrators first, then by name.
    pub async fn list_users(&self) -> AppResult<Vec<User>> {
        let rows: Vec<UserRow> = sqlx::query_as(select_users!("ORDER BY role = 'user', username"))
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| UserRecord::try_from(r).map(|r| r.user))
            .collect()
    }

    /// Change an account. A disabled account's sessions end. Refused with
    /// `Conflict` if no enabled administrator would remain.
    pub async fn update_user(
        &self,
        id: i64,
        changes: &UserChanges,
        now: DateTime<Utc>,
    ) -> AppResult<User> {
        let permissions = changes
            .permissions
            .as_deref()
            .map(|p| permissions_json(&normalize_permissions(p)));
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE users SET
                role = COALESCE(?1, role),
                permissions = COALESCE(?2, permissions),
                require_two_factor = COALESCE(?3, require_two_factor),
                disabled = COALESCE(?4, disabled),
                updated_at = ?5
             WHERE id = ?6",
        )
        .bind(changes.role.map(Role::as_str))
        .bind(permissions)
        .bind(changes.require_two_factor)
        .bind(changes.disabled)
        .bind(now.timestamp_millis())
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(missing());
        }
        ensure_an_admin_remains(&mut tx).await?;
        if changes.disabled == Some(true) {
            sqlx::query("DELETE FROM sessions WHERE user_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.user(id).await?.map(|r| r.user).ok_or_else(missing)
    }

    /// Delete an account with its sessions, API tokens and recovery codes.
    /// Its downloads stay, without an owner. Refused with `Conflict` if no
    /// enabled administrator would remain.
    pub async fn delete_user(&self, id: i64) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() == 0 {
            return Err(missing());
        }
        ensure_an_admin_remains(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Set a new password. With `temporary_until`, it must be changed at the
    /// next sign-in and stops working at that time. Every session of the
    /// account ends except `keep_session` (a token hash).
    pub async fn set_password(
        &self,
        id: i64,
        password_hash: &str,
        temporary_until: Option<DateTime<Utc>>,
        keep_session: Option<&str>,
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE users SET password_hash = ?1, must_change_password = ?2,
                password_expires_at = ?3, updated_at = ?4
             WHERE id = ?5",
        )
        .bind(password_hash)
        .bind(temporary_until.is_some())
        .bind(temporary_until.map(|t| t.timestamp_millis()))
        .bind(now.timestamp_millis())
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(missing());
        }
        sqlx::query("DELETE FROM sessions WHERE user_id = ?1 AND token_hash != COALESCE(?2, '')")
            .bind(id)
            .bind(keep_session)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_login(&self, id: i64, now: DateTime<Utc>) -> AppResult<()> {
        sqlx::query("UPDATE users SET last_login_at = ? WHERE id = ?")
            .bind(now.timestamp_millis())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete every administrator account, with their sessions and API
    /// tokens. Used for credential recovery; the next start offers first-run
    /// setup again. Other accounts stay. Returns how many were deleted.
    pub async fn delete_admins(&self) -> AppResult<u64> {
        let result = sqlx::query("DELETE FROM users WHERE role = 'admin'")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// The base32 TOTP secret, while two-factor authentication is on.
    pub async fn totp_secret(&self, user_id: i64) -> AppResult<Option<String>> {
        let secret: Option<Option<String>> =
            sqlx::query_scalar("SELECT totp_secret FROM users WHERE id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(secret.flatten())
    }

    /// Turn on two-factor authentication with a new secret and recovery
    /// codes. `step` is the step of the code that confirmed the secret, so
    /// that code cannot then be used to sign in.
    pub async fn enable_two_factor(
        &self,
        user_id: i64,
        secret: &str,
        step: i64,
        recovery_hashes: &[String],
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE users SET totp_secret = ?, totp_last_step = ? WHERE id = ?")
            .bind(secret)
            .bind(step)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        replace_recovery_codes(&mut tx, user_id, recovery_hashes, now).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Turn off two-factor authentication and forget the recovery codes.
    /// Returns whether it was on.
    pub async fn disable_two_factor(&self, user_id: i64) -> AppResult<bool> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE users SET totp_secret = NULL, totp_last_step = 0
             WHERE id = ? AND totp_secret IS NOT NULL",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM recovery_codes WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Record that the code for `step` was used. Returns false if that step
    /// or a later one was already used (a replayed code).
    pub async fn use_totp_step(&self, user_id: i64, step: i64) -> AppResult<bool> {
        let result = sqlx::query(
            "UPDATE users SET totp_last_step = ?1
             WHERE id = ?2 AND totp_secret IS NOT NULL AND totp_last_step < ?1",
        )
        .bind(step)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Use up a recovery code. Returns whether it existed.
    pub async fn use_recovery_code(&self, user_id: i64, code_hash: &str) -> AppResult<bool> {
        let result = sqlx::query("DELETE FROM recovery_codes WHERE user_id = ? AND code_hash = ?")
            .bind(user_id)
            .bind(code_hash)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn set_recovery_codes(
        &self,
        user_id: i64,
        recovery_hashes: &[String],
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        replace_recovery_codes(&mut tx, user_id, recovery_hashes, now).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn recovery_codes_left(&self, user_id: i64) -> AppResult<u32> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM recovery_codes WHERE user_id = ?")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    pub async fn insert_session(
        &self,
        token_hash: &str,
        user_id: i64,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
        )
        .bind(token_hash)
        .bind(user_id)
        .bind(now.timestamp_millis())
        .bind(expires_at.timestamp_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The account a live session belongs to.
    pub async fn session_user(
        &self,
        token_hash: &str,
        now: DateTime<Utc>,
    ) -> AppResult<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT user_id FROM sessions WHERE token_hash = ? AND expires_at > ?",
        )
        .bind(token_hash)
        .bind(now.timestamp_millis())
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn delete_session(&self, token_hash: &str) -> AppResult<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// End every session of an account except one (a token hash).
    pub async fn delete_user_sessions(
        &self,
        user_id: i64,
        keep_hash: Option<&str>,
    ) -> AppResult<()> {
        sqlx::query("DELETE FROM sessions WHERE user_id = ?1 AND token_hash != COALESCE(?2, '')")
            .bind(user_id)
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
        user_id: i64,
        name: &str,
        token_hash: &str,
        now: DateTime<Utc>,
    ) -> AppResult<ApiToken> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO api_tokens (id, user_id, name, token_hash, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(user_id)
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

    /// Look a token up by hash and record its use. Returns its account.
    pub async fn use_api_token(
        &self,
        token_hash: &str,
        now: DateTime<Utc>,
    ) -> AppResult<Option<i64>> {
        Ok(sqlx::query_scalar(
            "UPDATE api_tokens SET last_used_at = ? WHERE token_hash = ? RETURNING user_id",
        )
        .bind(now.timestamp_millis())
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn list_api_tokens(&self, user_id: i64) -> AppResult<Vec<ApiToken>> {
        let rows: Vec<TokenRow> = sqlx::query_as(
            "SELECT id, name, created_at, last_used_at FROM api_tokens
             WHERE user_id = ? ORDER BY created_at",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Returns whether one of the account's tokens was deleted.
    pub async fn delete_api_token(&self, user_id: i64, id: Uuid) -> AppResult<bool> {
        let result = sqlx::query("DELETE FROM api_tokens WHERE id = ? AND user_id = ?")
            .bind(id.to_string())
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }
}

async fn insert_user(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    new: &NewUser,
    now: DateTime<Utc>,
) -> AppResult<i64> {
    let ts = now.timestamp_millis();
    let result = sqlx::query_scalar(
        "INSERT INTO users (username, password_hash, role, permissions, must_change_password,
            password_expires_at, require_two_factor, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(&new.username)
    .bind(&new.password_hash)
    .bind(new.role.as_str())
    .bind(permissions_json(&normalize_permissions(&new.permissions)))
    .bind(new.temporary_until.is_some())
    .bind(new.temporary_until.map(|t| t.timestamp_millis()))
    .bind(new.require_two_factor)
    .bind(ts)
    .bind(ts)
    .fetch_one(&mut **tx)
    .await;
    match result {
        Ok(id) => Ok(id),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            Err(AppError::conflict("that username is already taken"))
        }
        Err(e) => Err(e.into()),
    }
}

async fn ensure_an_admin_remains(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> AppResult<()> {
    let admins: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0")
            .fetch_one(&mut **tx)
            .await?;
    if admins == 0 {
        return Err(AppError::conflict(
            "at least one enabled administrator must remain",
        ));
    }
    Ok(())
}

async fn replace_recovery_codes(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: i64,
    hashes: &[String],
    now: DateTime<Utc>,
) -> AppResult<()> {
    sqlx::query("DELETE FROM recovery_codes WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    for hash in hashes {
        sqlx::query("INSERT INTO recovery_codes (code_hash, user_id, created_at) VALUES (?, ?, ?)")
            .bind(hash)
            .bind(user_id)
            .bind(now.timestamp_millis())
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

fn permissions_json(permissions: &[Permission]) -> String {
    serde_json::to_string(permissions).expect("permission list serializes")
}

fn missing() -> AppError {
    AppError::not_found("account not found")
}

#[derive(FromRow)]
struct UserRow {
    id: i64,
    username: String,
    password_hash: String,
    role: String,
    permissions: String,
    must_change_password: bool,
    password_expires_at: Option<i64>,
    require_two_factor: bool,
    disabled: bool,
    two_factor_enabled: bool,
    created_at: i64,
    last_login_at: Option<i64>,
}

impl TryFrom<UserRow> for UserRecord {
    type Error = AppError;

    fn try_from(r: UserRow) -> AppResult<Self> {
        // Names this build doesn't know (from a newer one) are dropped.
        let names: Vec<String> = serde_json::from_str(&r.permissions)
            .map_err(|_| super::corrupt("invalid permissions"))?;
        let granted: Vec<Permission> = names.iter().filter_map(|n| Permission::parse(n)).collect();
        Ok(Self {
            user: User {
                id: r.id,
                username: r.username,
                role: Role::parse(&r.role).ok_or_else(|| super::corrupt("unknown role"))?,
                granted: normalize_permissions(&granted),
                must_change_password: r.must_change_password,
                password_expires_at: r.password_expires_at.map(from_millis).transpose()?,
                require_two_factor: r.require_two_factor,
                two_factor_enabled: r.two_factor_enabled,
                disabled: r.disabled,
                created_at: from_millis(r.created_at)?,
                last_login_at: r.last_login_at.map(from_millis).transpose()?,
            },
            password_hash: r.password_hash,
        })
    }
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
    use crate::store::NewDownload;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn account(name: &str, role: Role) -> NewUser {
        NewUser {
            username: name.into(),
            password_hash: "$argon2id$fake".into(),
            role,
            permissions: Vec::new(),
            require_two_factor: false,
            temporary_until: None,
        }
    }

    #[tokio::test]
    async fn first_admin_can_only_be_created_once() {
        let s = Store::open_in_memory().await.unwrap();
        assert!(!s.has_admin().await.unwrap());
        let admin = s
            .create_first_admin(&account("admin", Role::Admin), t(0))
            .await
            .unwrap();
        assert!(admin.is_admin() && !admin.must_change_password);
        assert!(s.has_admin().await.unwrap());
        let err = s
            .create_first_admin(&account("mallory", Role::Admin), t(1))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert!(
            s.user_by_name("mallory").await.unwrap().is_none(),
            "rolled back"
        );
    }

    #[tokio::test]
    async fn usernames_are_unique_ignoring_case() {
        let s = Store::open_in_memory().await.unwrap();
        s.create_user(&account("Alice", Role::User), t(0))
            .await
            .unwrap();
        let err = s
            .create_user(&account("alice", Role::User), t(0))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        let found = s.user_by_name("ALICE").await.unwrap().unwrap();
        assert_eq!(found.user.username, "Alice");
    }

    #[tokio::test]
    async fn permissions_and_temporary_passwords_are_stored() {
        let s = Store::open_in_memory().await.unwrap();
        let mut new = account("bob", Role::User);
        new.permissions = vec![Permission::ManageAllDownloads];
        new.temporary_until = Some(t(100));
        new.require_two_factor = true;
        let bob = s.create_user(&new, t(0)).await.unwrap();
        assert_eq!(
            bob.granted,
            [Permission::ViewAllDownloads, Permission::ManageAllDownloads]
        );
        assert!(bob.must_change_password && bob.require_two_factor);
        assert_eq!(bob.password_expires_at, Some(t(100)));
        assert!(!bob.two_factor_enabled);

        s.insert_session("keep", bob.id, t(0), t(1000))
            .await
            .unwrap();
        s.insert_session("drop", bob.id, t(0), t(1000))
            .await
            .unwrap();
        s.set_password(bob.id, "new-hash", None, Some("keep"), t(1))
            .await
            .unwrap();
        let record = s.user(bob.id).await.unwrap().unwrap();
        assert_eq!(record.password_hash, "new-hash");
        assert!(!record.user.must_change_password);
        assert_eq!(record.user.password_expires_at, None);
        assert_eq!(s.session_user("keep", t(2)).await.unwrap(), Some(bob.id));
        assert_eq!(s.session_user("drop", t(2)).await.unwrap(), None);

        // Unknown names from a newer build are ignored.
        sqlx::query("UPDATE users SET permissions = '[\"submit_downloads\",\"fly\"]'")
            .execute(&s.pool)
            .await
            .unwrap();
        let record = s.user(bob.id).await.unwrap().unwrap();
        assert_eq!(record.user.granted, [Permission::SubmitDownloads]);
    }

    #[tokio::test]
    async fn the_last_enabled_admin_cannot_be_removed() {
        let s = Store::open_in_memory().await.unwrap();
        let a = s
            .create_first_admin(&account("a", Role::Admin), t(0))
            .await
            .unwrap();
        let user = s
            .create_user(&account("u", Role::User), t(0))
            .await
            .unwrap();

        let demote = UserChanges {
            role: Some(Role::User),
            ..Default::default()
        };
        let disable = UserChanges {
            disabled: Some(true),
            ..Default::default()
        };
        for changes in [&demote, &disable] {
            let err = s.update_user(a.id, changes, t(1)).await.unwrap_err();
            assert_eq!(err.kind, ErrorKind::Conflict);
        }
        assert_eq!(
            s.delete_user(a.id).await.unwrap_err().kind,
            ErrorKind::Conflict
        );
        assert!(
            s.user(a.id).await.unwrap().unwrap().user.is_admin(),
            "rolled back"
        );

        // With a second administrator, the first can go.
        let promote = UserChanges {
            role: Some(Role::Admin),
            ..Default::default()
        };
        s.update_user(user.id, &promote, t(2)).await.unwrap();
        s.insert_session("a-session", a.id, t(2), t(1000))
            .await
            .unwrap();
        let a = s.update_user(a.id, &disable, t(3)).await.unwrap();
        assert!(a.disabled);
        assert_eq!(
            s.session_user("a-session", t(4)).await.unwrap(),
            None,
            "disabling ends sessions"
        );
        s.delete_user(a.id).await.unwrap();
        assert_eq!(
            s.delete_user(a.id).await.unwrap_err().kind,
            ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn deleting_an_account_keeps_its_downloads_without_owner() {
        let s = Store::open_in_memory().await.unwrap();
        s.create_first_admin(&account("a", Role::Admin), t(0))
            .await
            .unwrap();
        let u = s
            .create_user(&account("u", Role::User), t(0))
            .await
            .unwrap();
        s.insert_session("h", u.id, t(0), t(100)).await.unwrap();
        s.insert_api_token(u.id, "cli", "th", t(0)).await.unwrap();
        let d = s
            .insert_download(
                NewDownload {
                    url: "https://example.com/f".into(),
                    display_url: "https://example.com/f".into(),
                    provider: "http".into(),
                    owner_id: Some(u.id),
                    ..Default::default()
                },
                t(0),
            )
            .await
            .unwrap();
        assert_eq!(d.owner_id, Some(u.id));
        assert_eq!(
            s.list_downloads(&[], Some(u.id), 10, 0)
                .await
                .unwrap()
                .len(),
            1
        );

        s.delete_user(u.id).await.unwrap();
        assert_eq!(s.session_user("h", t(1)).await.unwrap(), None);
        assert_eq!(s.use_api_token("th", t(1)).await.unwrap(), None);
        let d = s.get_download(d.id).await.unwrap().unwrap();
        assert_eq!(d.owner_id, None);
    }

    #[tokio::test]
    async fn two_factor_is_per_account() {
        let s = Store::open_in_memory().await.unwrap();
        let a = s
            .create_first_admin(&account("a", Role::Admin), t(0))
            .await
            .unwrap();
        let b = s
            .create_user(&account("b", Role::User), t(0))
            .await
            .unwrap();
        assert_eq!(s.totp_secret(a.id).await.unwrap(), None);
        assert!(
            !s.use_totp_step(a.id, 5).await.unwrap(),
            "off: no step is accepted"
        );

        let codes = vec!["r1".to_owned(), "r2".to_owned()];
        s.enable_two_factor(a.id, "SECRET", 10, &codes, t(0))
            .await
            .unwrap();
        s.enable_two_factor(b.id, "OTHER", 10, &codes, t(0))
            .await
            .unwrap();
        assert!(s.user(a.id).await.unwrap().unwrap().user.two_factor_enabled);
        assert_eq!(
            s.totp_secret(a.id).await.unwrap().as_deref(),
            Some("SECRET")
        );
        assert!(
            !s.use_totp_step(a.id, 10).await.unwrap(),
            "confirming step is used"
        );
        assert!(s.use_totp_step(a.id, 11).await.unwrap());
        assert!(!s.use_totp_step(a.id, 11).await.unwrap(), "replay");
        assert!(
            s.use_totp_step(b.id, 11).await.unwrap(),
            "steps are per account"
        );

        assert!(s.use_recovery_code(a.id, "r1").await.unwrap());
        assert!(
            !s.use_recovery_code(a.id, "r1").await.unwrap(),
            "single use"
        );
        assert!(
            s.use_recovery_code(b.id, "r1").await.unwrap(),
            "codes are per account"
        );
        s.set_recovery_codes(a.id, &["r3".to_owned()], t(1))
            .await
            .unwrap();
        assert!(
            !s.use_recovery_code(a.id, "r2").await.unwrap(),
            "old codes replaced"
        );
        assert_eq!(s.recovery_codes_left(a.id).await.unwrap(), 1);
        assert_eq!(s.recovery_codes_left(b.id).await.unwrap(), 1);

        assert!(s.disable_two_factor(a.id).await.unwrap());
        assert!(!s.disable_two_factor(a.id).await.unwrap());
        assert_eq!(s.recovery_codes_left(a.id).await.unwrap(), 0);
        assert_eq!(s.totp_secret(b.id).await.unwrap().as_deref(), Some("OTHER"));
    }

    #[tokio::test]
    async fn sessions_expire_and_end_per_account() {
        let s = Store::open_in_memory().await.unwrap();
        let a = s
            .create_first_admin(&account("a", Role::Admin), t(0))
            .await
            .unwrap();
        let b = s
            .create_user(&account("b", Role::User), t(0))
            .await
            .unwrap();
        s.insert_session("h1", a.id, t(0), t(100)).await.unwrap();
        assert_eq!(s.session_user("h1", t(99)).await.unwrap(), Some(a.id));
        assert_eq!(s.session_user("h1", t(100)).await.unwrap(), None);
        assert_eq!(s.prune_sessions(t(100)).await.unwrap(), 1);

        s.insert_session("a1", a.id, t(0), t(100)).await.unwrap();
        s.insert_session("a2", a.id, t(0), t(100)).await.unwrap();
        s.insert_session("b1", b.id, t(0), t(100)).await.unwrap();
        s.delete_user_sessions(a.id, Some("a1")).await.unwrap();
        assert_eq!(s.session_user("a1", t(1)).await.unwrap(), Some(a.id));
        assert_eq!(s.session_user("a2", t(1)).await.unwrap(), None);
        assert_eq!(s.session_user("b1", t(1)).await.unwrap(), Some(b.id));
        s.delete_user_sessions(a.id, None).await.unwrap();
        assert_eq!(s.session_user("a1", t(1)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn api_tokens_belong_to_an_account() {
        let s = Store::open_in_memory().await.unwrap();
        let a = s
            .create_first_admin(&account("a", Role::Admin), t(0))
            .await
            .unwrap();
        let b = s
            .create_user(&account("b", Role::User), t(0))
            .await
            .unwrap();
        let token = s
            .insert_api_token(a.id, "sonarr", "hash1", t(0))
            .await
            .unwrap();
        assert_eq!(s.use_api_token("hash1", t(5)).await.unwrap(), Some(a.id));
        assert_eq!(s.use_api_token("nope", t(5)).await.unwrap(), None);
        let listed = s.list_api_tokens(a.id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].last_used_at, Some(t(5)));
        assert!(s.list_api_tokens(b.id).await.unwrap().is_empty());
        assert!(
            !s.delete_api_token(b.id, token.id).await.unwrap(),
            "not someone else's"
        );
        assert!(s.delete_api_token(a.id, token.id).await.unwrap());
        assert_eq!(s.use_api_token("hash1", t(6)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn reset_deletes_administrators_only() {
        let s = Store::open_in_memory().await.unwrap();
        s.create_first_admin(&account("a", Role::Admin), t(0))
            .await
            .unwrap();
        let u = s
            .create_user(&account("u", Role::User), t(0))
            .await
            .unwrap();
        assert_eq!(s.delete_admins().await.unwrap(), 1);
        assert!(!s.has_admin().await.unwrap());
        assert!(s.user(u.id).await.unwrap().is_some());
    }

    /// An existing single-administrator database keeps its account,
    /// sessions, tokens, two-factor secret, recovery codes and downloads.
    #[tokio::test]
    async fn upgrade_from_a_single_administrator() {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;

        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let before: Vec<_> = super::super::MIGRATOR
            .iter()
            .filter(|m| m.version < 10)
            .collect();
        for migration in &before {
            sqlx::raw_sql(migration.sql.as_ref())
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::raw_sql(
            "INSERT INTO admin (id, username, password_hash, updated_at, totp_secret, totp_last_step)
                VALUES (1, 'root', '$argon2id$fake', 5, 'SECRET', 7);
             INSERT INTO sessions VALUES ('sess', 1, 9999999999999);
             INSERT INTO api_tokens VALUES ('00000000-0000-0000-0000-000000000001', 'cli', 'tok', 1, NULL);
             INSERT INTO recovery_codes VALUES ('rc', 1);
             INSERT INTO downloads (id, url, display_url, provider, state, created_at, updated_at)
                VALUES ('00000000-0000-0000-0000-0000000000aa', 'u', 'u', 'http', 'queued', 1, 1);",
        )
        .execute(&pool)
        .await
        .unwrap();
        let upgrade = super::super::MIGRATOR
            .iter()
            .find(|m| m.version == 10)
            .unwrap();
        sqlx::raw_sql(upgrade.sql.as_ref())
            .execute(&pool)
            .await
            .unwrap();
        let s = Store { pool };

        let root = s.user_by_name("root").await.unwrap().unwrap().user;
        assert_eq!(root.id, 1);
        assert!(root.is_admin() && root.two_factor_enabled && !root.must_change_password);
        assert_eq!(s.session_user("sess", t(0)).await.unwrap(), Some(1));
        assert_eq!(s.use_api_token("tok", t(0)).await.unwrap(), Some(1));
        assert!(s.use_recovery_code(1, "rc").await.unwrap());
        assert!(!s.use_totp_step(1, 7).await.unwrap(), "last step kept");
        let d = s
            .get_download(Uuid::parse_str("00000000-0000-0000-0000-0000000000aa").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(d.owner_id, Some(1));
    }
}
