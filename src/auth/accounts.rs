//! Account management by administrators.
//!
//! An administrator creates an account with a role, permissions, and
//! whether it must use two-factor authentication. Direkt generates a
//! temporary password for it, shown once to the administrator, who hands it
//! over. At the first sign-in the new user must replace it with a strong
//! password of their own, then set up two-factor authentication if it is
//! required. A temporary password stops working after
//! [`TEMPORARY_PASSWORD_TTL`].
//!
//! Administrators cannot demote, disable, delete, or reset their own
//! account here, so nobody locks themselves out by accident; and the store
//! refuses any change that would leave no enabled administrator.

use super::{AuthService, hash_password, validate_username};
use crate::error::{AppError, AppResult};
use crate::store::{NewUser, Store, UserChanges};
use crate::users::{Permission, Role, User};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::time::Duration;

/// How long a temporary password works.
pub const TEMPORARY_PASSWORD_TTL: Duration = Duration::from_secs(72 * 3600);
/// Letters and digits that can't be mistaken for one another when read out
/// or copied by hand (no `0`/`o`, `1`/`l`/`i`).
const TEMPORARY_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
/// 16 characters of 31: about 79 bits.
const TEMPORARY_CHARS: usize = 16;

/// A new account, as the administrator describes it.
#[derive(Debug, Clone)]
pub struct NewAccount {
    pub username: String,
    pub role: Role,
    /// Ignored for administrators, who have every permission.
    pub permissions: Vec<Permission>,
    pub require_two_factor: bool,
}

/// A temporary password just issued, shown once.
#[derive(Debug, Clone)]
pub struct IssuedPassword {
    pub user: User,
    pub password: String,
    pub expires_at: DateTime<Utc>,
}

impl AuthService {
    /// Every account. Administrators only.
    pub async fn list_users(&self, actor: &User) -> AppResult<Vec<User>> {
        actor.require_admin()?;
        self.inner.store.list_users().await
    }

    /// One account. Administrators only.
    pub async fn get_user(&self, actor: &User, id: i64) -> AppResult<User> {
        actor.require_admin()?;
        self.account(id).await
    }

    /// Account names by id, to show who added a download.
    pub async fn usernames(&self) -> AppResult<HashMap<i64, String>> {
        Ok(self
            .inner
            .store
            .list_users()
            .await?
            .into_iter()
            .map(|u| (u.id, u.username))
            .collect())
    }

    /// Create an account with a temporary password.
    pub async fn create_user(&self, actor: &User, new: NewAccount) -> AppResult<IssuedPassword> {
        actor.require_admin()?;
        let username = validate_username(&new.username)?;
        let password = generate_temporary_password();
        let expires_at = temporary_until();
        let user = self
            .inner
            .store
            .create_user(
                &NewUser {
                    username: username.to_owned(),
                    password_hash: hash_password(password.clone()).await?,
                    role: new.role,
                    permissions: match new.role {
                        Role::Admin => Vec::new(),
                        Role::User => new.permissions,
                    },
                    require_two_factor: new.require_two_factor,
                    temporary_until: Some(expires_at),
                },
                Utc::now(),
            )
            .await?;
        tracing::info!(
            by = %actor.username,
            user = %user.username,
            role = user.role.as_str(),
            "account created"
        );
        Ok(IssuedPassword {
            user,
            password,
            expires_at,
        })
    }

    /// Change an account's role, permissions, two-factor requirement, or
    /// whether it is disabled. Takes effect on its next request.
    pub async fn update_user(
        &self,
        actor: &User,
        id: i64,
        changes: UserChanges,
    ) -> AppResult<User> {
        actor.require_admin()?;
        if id == actor.id && (changes.role == Some(Role::User) || changes.disabled == Some(true)) {
            return Err(AppError::conflict(
                "you can't demote or disable your own account; ask another administrator",
            ));
        }
        let user = self
            .inner
            .store
            .update_user(id, &changes, Utc::now())
            .await?;
        if user.disabled {
            self.forget_pending_logins(id);
        }
        tracing::info!(by = %actor.username, user = %user.username, "account changed");
        Ok(user)
    }

    /// Give an account a new temporary password, for a forgotten password.
    /// Its sessions end; its API tokens stay.
    pub async fn reset_user_password(&self, actor: &User, id: i64) -> AppResult<IssuedPassword> {
        actor.require_admin()?;
        if id == actor.id {
            return Err(AppError::conflict(
                "change your own password under Settings",
            ));
        }
        let user = self.account(id).await?;
        let issued = issue_temporary_password(&self.inner.store, user).await?;
        self.forget_pending_logins(id);
        tracing::info!(by = %actor.username, user = %issued.user.username, "temporary password issued");
        Ok(issued)
    }

    /// Turn off an account's two-factor authentication, for a lost phone.
    /// If it is required, the next sign-in sets it up again.
    pub async fn reset_user_two_factor(&self, actor: &User, id: i64) -> AppResult<User> {
        actor.require_admin()?;
        if id == actor.id {
            return Err(AppError::conflict(
                "turn off your own two-factor authentication under Settings",
            ));
        }
        let user = self.account(id).await?;
        if !self.inner.store.disable_two_factor(id).await? {
            return Err(AppError::conflict(
                "two-factor authentication is already off for this account",
            ));
        }
        self.forget_pending_logins(id);
        self.cancel_two_factor_setup(id);
        tracing::info!(by = %actor.username, user = %user.username, "two-factor authentication reset");
        self.account(id).await
    }

    /// Delete an account, its sessions and API tokens. Its downloads stay,
    /// without an owner.
    pub async fn delete_user(&self, actor: &User, id: i64) -> AppResult<()> {
        actor.require_admin()?;
        if id == actor.id {
            return Err(AppError::conflict(
                "you can't delete your own account; ask another administrator",
            ));
        }
        let user = self.account(id).await?;
        self.inner.store.delete_user(id).await?;
        self.forget_pending_logins(id);
        self.cancel_two_factor_setup(id);
        tracing::info!(by = %actor.username, user = %user.username, "account deleted");
        Ok(())
    }

    async fn account(&self, id: i64) -> AppResult<User> {
        self.inner
            .store
            .user(id)
            .await?
            .map(|r| r.user)
            .ok_or_else(|| AppError::not_found("account not found"))
    }
}

/// Set a new temporary password for an account and end its sessions. Also
/// used by `direkt reset-password` on the command line.
pub async fn issue_temporary_password(store: &Store, user: User) -> AppResult<IssuedPassword> {
    let password = generate_temporary_password();
    let expires_at = temporary_until();
    store
        .set_password(
            user.id,
            &hash_password(password.clone()).await?,
            Some(expires_at),
            None,
            Utc::now(),
        )
        .await?;
    let user = store
        .user(user.id)
        .await?
        .map(|r| r.user)
        .ok_or_else(|| AppError::not_found("account not found"))?;
    Ok(IssuedPassword {
        user,
        password,
        expires_at,
    })
}

fn temporary_until() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::from_std(TEMPORARY_PASSWORD_TTL).expect("constant fits")
}

/// Random characters from [`TEMPORARY_ALPHABET`], in groups of four.
fn generate_temporary_password() -> String {
    let mut chars = Vec::with_capacity(TEMPORARY_CHARS);
    // Rejection sampling: bytes past the largest multiple of the alphabet
    // size would favour its first letters.
    let limit = 256 - 256 % TEMPORARY_ALPHABET.len();
    while chars.len() < TEMPORARY_CHARS {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).expect("operating system RNG is unavailable");
        chars.extend(
            buf.iter()
                .filter(|&&b| usize::from(b) < limit)
                .map(|&b| TEMPORARY_ALPHABET[usize::from(b) % TEMPORARY_ALPHABET.len()] as char)
                .take(TEMPORARY_CHARS - chars.len()),
        );
    }
    chars
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::super::tests::{IP, app_code, service, with_admin};
    use super::*;
    use crate::auth::LoginOutcome;
    use crate::error::ErrorKind;

    fn new_account(name: &str) -> NewAccount {
        NewAccount {
            username: name.into(),
            role: Role::User,
            permissions: vec![Permission::SubmitDownloads],
            require_two_factor: false,
        }
    }

    #[test]
    fn temporary_passwords_are_readable_and_random() {
        let a = generate_temporary_password();
        assert_eq!(a.len(), TEMPORARY_CHARS + 3);
        assert_eq!(a.matches('-').count(), 3);
        assert!(
            a.bytes()
                .all(|b| b == b'-' || TEMPORARY_ALPHABET.contains(&b))
        );
        assert_ne!(a, generate_temporary_password());
    }

    #[tokio::test]
    async fn only_administrators_manage_accounts() {
        let (auth, _dir) = service().await;
        let (admin, _) = with_admin(&auth).await;
        let issued = auth
            .create_user(&admin, new_account("alice"))
            .await
            .unwrap();
        let alice = issued.user;
        let forbidden = |e: AppError| assert_eq!(e.kind, ErrorKind::Forbidden);
        forbidden(auth.list_users(&alice).await.unwrap_err());
        forbidden(auth.get_user(&alice, admin.id).await.unwrap_err());
        forbidden(
            auth.create_user(&alice, new_account("eve"))
                .await
                .unwrap_err(),
        );
        forbidden(
            auth.update_user(&alice, alice.id, UserChanges::default())
                .await
                .unwrap_err(),
        );
        forbidden(
            auth.reset_user_password(&alice, admin.id)
                .await
                .unwrap_err(),
        );
        forbidden(
            auth.reset_user_two_factor(&alice, admin.id)
                .await
                .unwrap_err(),
        );
        forbidden(auth.delete_user(&alice, admin.id).await.unwrap_err());
        assert_eq!(auth.list_users(&admin).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn new_users_choose_a_strong_password_at_first_sign_in() {
        let (auth, _dir) = service().await;
        let (admin, _) = with_admin(&auth).await;
        let issued = auth
            .create_user(&admin, new_account("alice"))
            .await
            .unwrap();
        assert!(issued.user.must_change_password);
        assert!(issued.user.needs_onboarding());
        assert_eq!(issued.user.granted, [Permission::SubmitDownloads]);
        let err = auth
            .create_user(&admin, new_account("ALICE"))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict, "names are unique");

        let session = auth
            .login(IP, "alice", &issued.password)
            .await
            .unwrap()
            .expect_session();
        let alice = auth.session_user(&session.token).await.unwrap().unwrap();
        assert!(alice.needs_onboarding());

        let err = auth
            .choose_password(&alice, &issued.password)
            .await
            .unwrap_err();
        assert!(err.message.contains("temporary"), "{}", err.message);
        let err = auth.choose_password(&alice, "password").await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        let new_session = auth
            .choose_password(&alice, "Mon-mot-de-passe-42")
            .await
            .unwrap();
        assert!(
            !auth.session_valid(&session.token).await.unwrap(),
            "rotated"
        );
        let alice = auth
            .session_user(&new_session.token)
            .await
            .unwrap()
            .unwrap();
        assert!(!alice.needs_onboarding());
        assert!(auth.login(IP, "alice", &issued.password).await.is_err());
        auth.login(IP, "alice", "Mon-mot-de-passe-42")
            .await
            .unwrap()
            .expect_session();
        let err = auth
            .choose_password(&alice, "Another-password-42")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict, "only while temporary");
    }

    #[tokio::test]
    async fn expired_temporary_passwords_are_refused() {
        let (auth, _dir) = service().await;
        let (admin, _) = with_admin(&auth).await;
        let issued = auth
            .create_user(&admin, new_account("alice"))
            .await
            .unwrap();
        sqlx::query("UPDATE users SET password_expires_at = 0 WHERE username = 'alice'")
            .execute(auth.inner.store.pool())
            .await
            .unwrap();
        let err = auth.login(IP, "alice", &issued.password).await.unwrap_err();
        assert!(err.message.contains("expired"), "{}", err.message);
        // A new one works.
        let issued = auth
            .reset_user_password(&admin, issued.user.id)
            .await
            .unwrap();
        auth.login(IP, "alice", &issued.password)
            .await
            .unwrap()
            .expect_session();
    }

    #[tokio::test]
    async fn required_two_factor_is_set_up_after_the_password() {
        let (auth, _dir) = service().await;
        let (admin, _) = with_admin(&auth).await;
        let mut new = new_account("bob");
        new.require_two_factor = true;
        let issued = auth.create_user(&admin, new).await.unwrap();
        let session = auth
            .login(IP, "bob", &issued.password)
            .await
            .unwrap()
            .expect_session();
        let bob = auth.session_user(&session.token).await.unwrap().unwrap();
        let session = auth
            .choose_password(&bob, "Quiet-river-stone-7")
            .await
            .unwrap();
        let bob = auth.session_user(&session.token).await.unwrap().unwrap();
        assert!(bob.needs_onboarding() && bob.needs_two_factor_setup());

        let enrollment = auth.required_two_factor_setup(&bob).await.unwrap();
        let again = auth.required_two_factor_setup(&bob).await.unwrap();
        assert_eq!(enrollment.secret, again.secret, "a reload keeps the secret");
        assert!(enrollment.uri.contains("Direkt:bob"));
        auth.finish_two_factor_setup(IP, &bob, &app_code(&enrollment.secret, 0), &session.token)
            .await
            .unwrap();
        let bob = auth.session_user(&session.token).await.unwrap().unwrap();
        assert!(!bob.needs_onboarding());

        // Required: it can't be turned off.
        let err = auth
            .disable_two_factor(
                IP,
                &bob,
                "Quiet-river-stone-7",
                &app_code(&enrollment.secret, 1),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Forbidden);

        // Lost phone: the administrator resets it, and the next sign-in sets
        // it up again.
        let bob = auth.reset_user_two_factor(&admin, bob.id).await.unwrap();
        assert!(bob.needs_two_factor_setup());
        assert!(matches!(
            auth.login(IP, "bob", "Quiet-river-stone-7").await.unwrap(),
            LoginOutcome::Session(_)
        ));
        let err = auth
            .reset_user_two_factor(&admin, bob.id)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
    }

    #[tokio::test]
    async fn disabling_and_deleting_end_access() {
        let (auth, _dir) = service().await;
        let (admin, _) = with_admin(&auth).await;
        let issued = auth
            .create_user(&admin, new_account("carol"))
            .await
            .unwrap();
        let session = auth
            .login(IP, "carol", &issued.password)
            .await
            .unwrap()
            .expect_session();
        let carol = auth.session_user(&session.token).await.unwrap().unwrap();
        let session = auth
            .choose_password(&carol, "Amber-lantern-key-99")
            .await
            .unwrap();
        let carol = auth.session_user(&session.token).await.unwrap().unwrap();
        let mut can_token = carol.clone();
        can_token.granted.push(Permission::ApiTokens);
        let (_, token) = auth.create_api_token(&can_token, "script").await.unwrap();

        let disable = UserChanges {
            disabled: Some(true),
            ..Default::default()
        };
        auth.update_user(&admin, carol.id, disable).await.unwrap();
        assert!(!auth.session_valid(&session.token).await.unwrap());
        assert!(auth.token_user(&token).await.unwrap().is_none());
        let err = auth
            .login(IP, "carol", "Amber-lantern-key-99")
            .await
            .unwrap_err();
        assert!(err.message.contains("disabled"), "{}", err.message);

        let enable = UserChanges {
            disabled: Some(false),
            ..Default::default()
        };
        auth.update_user(&admin, carol.id, enable).await.unwrap();
        assert!(
            auth.token_user(&token).await.unwrap().is_some(),
            "tokens come back"
        );

        auth.delete_user(&admin, carol.id).await.unwrap();
        assert!(auth.token_user(&token).await.unwrap().is_none());
        assert!(
            auth.login(IP, "carol", "Amber-lantern-key-99")
                .await
                .is_err()
        );
        assert_eq!(
            auth.delete_user(&admin, carol.id).await.unwrap_err().kind,
            ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn administrators_cannot_lock_themselves_out() {
        let (auth, _dir) = service().await;
        let (admin, _) = with_admin(&auth).await;
        let conflict = |e: AppError| assert_eq!(e.kind, ErrorKind::Conflict);
        for changes in [
            UserChanges {
                role: Some(Role::User),
                ..Default::default()
            },
            UserChanges {
                disabled: Some(true),
                ..Default::default()
            },
        ] {
            conflict(
                auth.update_user(&admin, admin.id, changes)
                    .await
                    .unwrap_err(),
            );
        }
        conflict(auth.delete_user(&admin, admin.id).await.unwrap_err());
        conflict(
            auth.reset_user_password(&admin, admin.id)
                .await
                .unwrap_err(),
        );
        conflict(
            auth.reset_user_two_factor(&admin, admin.id)
                .await
                .unwrap_err(),
        );

        // Requiring two-factor authentication of oneself is fine.
        let me = auth
            .update_user(
                &admin,
                admin.id,
                UserChanges {
                    require_two_factor: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(me.needs_two_factor_setup());
    }

    #[tokio::test]
    async fn promoted_users_get_every_permission() {
        let (auth, _dir) = service().await;
        let (admin, _) = with_admin(&auth).await;
        let mut new = new_account("dave");
        new.role = Role::Admin;
        let dave = auth.create_user(&admin, new).await.unwrap().user;
        assert!(dave.granted.is_empty());
        assert!(Permission::ALL.into_iter().all(|p| dave.can(p)));

        let dave = auth
            .update_user(
                &admin,
                dave.id,
                UserChanges {
                    role: Some(Role::User),
                    permissions: Some(vec![Permission::ManageSettings]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(dave.can(Permission::ManageSettings));
        assert!(!dave.can(Permission::SubmitDownloads));
    }
}
