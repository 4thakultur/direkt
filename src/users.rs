//! Accounts and what they may do.
//!
//! Two roles. Administrators can do everything, including managing accounts.
//! Users can do what an administrator granted them, as a set of
//! [`Permission`]s. Downloads belong to the account that added it: a user
//! always sees and controls their own, and needs `view_all_downloads` or
//! `manage_all_downloads` for everyone else's.

use crate::domain::Download;
use crate::error::{AppError, AppResult, ErrorKind};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Every permission, and manages accounts.
    Admin,
    /// The permissions an administrator granted.
    User,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Self::Admin),
            "user" => Some(Self::User),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Admin => "Administrator",
            Self::User => "User",
        }
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Add downloads.
    SubmitDownloads,
    /// Download from hosts that use the server's premium account or credits
    /// (1fichier). Without it, only direct links.
    PremiumProviders,
    /// See everyone's downloads, not only their own.
    ViewAllDownloads,
    /// Pause, resume, retry, cancel, organize and remove everyone's
    /// downloads. Includes seeing them.
    ManageAllDownloads,
    /// Delete downloaded files from disk (their own, or any they manage).
    DeleteFiles,
    /// Change server settings: provider accounts and keys, artwork, speed
    /// limit and simultaneous downloads.
    ManageSettings,
    /// Create personal API tokens, which act with the same permissions.
    ApiTokens,
}

impl Permission {
    pub const ALL: [Permission; 7] = [
        Self::SubmitDownloads,
        Self::PremiumProviders,
        Self::ViewAllDownloads,
        Self::ManageAllDownloads,
        Self::DeleteFiles,
        Self::ManageSettings,
        Self::ApiTokens,
    ];

    /// Granted to a new user when the administrator does not choose.
    pub const DEFAULT: [Permission; 2] = [Self::SubmitDownloads, Self::PremiumProviders];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SubmitDownloads => "submit_downloads",
            Self::PremiumProviders => "premium_providers",
            Self::ViewAllDownloads => "view_all_downloads",
            Self::ManageAllDownloads => "manage_all_downloads",
            Self::DeleteFiles => "delete_files",
            Self::ManageSettings => "manage_settings",
            Self::ApiTokens => "api_tokens",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SubmitDownloads => "Add downloads",
            Self::PremiumProviders => "Use premium hosts",
            Self::ViewAllDownloads => "See everyone's downloads",
            Self::ManageAllDownloads => "Manage everyone's downloads",
            Self::DeleteFiles => "Delete files from disk",
            Self::ManageSettings => "Change server settings",
            Self::ApiTokens => "Create API tokens",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::SubmitDownloads => "Paste links and start downloads.",
            Self::PremiumProviders => {
                "Download from 1fichier with the server's premium account or CDN credits. \
                 Without it, only direct links."
            }
            Self::ViewAllDownloads => "Otherwise only the downloads they added are shown.",
            Self::ManageAllDownloads => {
                "Pause, resume, retry, cancel and remove anyone's downloads. Includes seeing them."
            }
            Self::DeleteFiles => {
                "Delete completed files from the download folder, for downloads they can manage."
            }
            Self::ManageSettings => {
                "1fichier account and keys, TMDB key, speed limit and simultaneous downloads."
            }
            Self::ApiTokens => "Tokens for scripts, with the same permissions as the account.",
        }
    }

    /// What is refused without it, for error messages.
    fn action(self) -> &'static str {
        match self {
            Self::SubmitDownloads => "add downloads",
            Self::PremiumProviders => "download from premium hosts such as 1fichier",
            Self::ViewAllDownloads => "see other people's downloads",
            Self::ManageAllDownloads => "manage other people's downloads",
            Self::DeleteFiles => "delete files from disk",
            Self::ManageSettings => "change server settings",
            Self::ApiTokens => "create API tokens",
        }
    }
}

/// Sort, deduplicate, and add what a permission implies.
pub fn normalize_permissions(permissions: &[Permission]) -> Vec<Permission> {
    let mut list = permissions.to_vec();
    if list.contains(&Permission::ManageAllDownloads) {
        list.push(Permission::ViewAllDownloads);
    }
    list.sort();
    list.dedup();
    list
}

/// An account, without its secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub role: Role,
    /// What an administrator granted. Administrators have every permission
    /// whatever this holds; use [`User::can`].
    pub granted: Vec<Permission>,
    /// Signed in with a temporary password: must choose a new one first.
    pub must_change_password: bool,
    /// When the temporary password stops working.
    pub password_expires_at: Option<DateTime<Utc>>,
    pub require_two_factor: bool,
    pub two_factor_enabled: bool,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    pub fn can(&self, permission: Permission) -> bool {
        self.is_admin()
            || self.granted.contains(&permission)
            || (permission == Permission::ViewAllDownloads
                && self.granted.contains(&Permission::ManageAllDownloads))
    }

    /// Every permission the account has.
    pub fn permissions(&self) -> Vec<Permission> {
        Permission::ALL
            .into_iter()
            .filter(|&p| self.can(p))
            .collect()
    }

    pub fn require(&self, permission: Permission) -> AppResult<()> {
        if self.can(permission) {
            Ok(())
        } else {
            Err(forbidden(format!(
                "your account is not allowed to {}",
                permission.action()
            )))
        }
    }

    pub fn require_admin(&self) -> AppResult<()> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(forbidden("only administrators can do this"))
        }
    }

    /// Must choose a password or set up two-factor authentication before
    /// using Direkt.
    pub fn needs_onboarding(&self) -> bool {
        self.must_change_password || self.needs_two_factor_setup()
    }

    pub fn needs_two_factor_setup(&self) -> bool {
        self.require_two_factor && !self.two_factor_enabled
    }

    pub fn owns(&self, download: &Download) -> bool {
        download.owner_id == Some(self.id)
    }

    pub fn can_see(&self, download: &Download) -> bool {
        self.owns(download) || self.can(Permission::ViewAllDownloads)
    }

    /// Pause, resume, retry, cancel, organize, or remove it from the list.
    pub fn can_manage(&self, download: &Download) -> bool {
        self.owns(download) || self.can(Permission::ManageAllDownloads)
    }

    /// Whose downloads lists show: `None` for everyone's.
    pub fn download_scope(&self) -> Option<i64> {
        (!self.can(Permission::ViewAllDownloads)).then_some(self.id)
    }

    /// Whose downloads bulk actions apply to: `None` for everyone's.
    pub fn manage_scope(&self) -> Option<i64> {
        (!self.can(Permission::ManageAllDownloads)).then_some(self.id)
    }

    /// An administrator, for tests and internal callers.
    #[cfg(test)]
    pub(crate) fn test_admin(id: i64) -> Self {
        Self {
            id,
            username: format!("admin{id}"),
            role: Role::Admin,
            granted: Vec::new(),
            must_change_password: false,
            password_expires_at: None,
            require_two_factor: false,
            two_factor_enabled: false,
            disabled: false,
            created_at: Utc::now(),
            last_login_at: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_user(id: i64, granted: &[Permission]) -> Self {
        Self {
            role: Role::User,
            username: format!("user{id}"),
            granted: normalize_permissions(granted),
            ..Self::test_admin(id)
        }
    }
}

fn forbidden(message: impl Into<String>) -> AppError {
    AppError::new(ErrorKind::Forbidden, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for p in Permission::ALL {
            assert_eq!(Permission::parse(p.as_str()), Some(p));
            assert_eq!(serde_json::to_value(p).unwrap(), p.as_str());
        }
        for r in [Role::Admin, Role::User] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        assert_eq!(Permission::parse("everything"), None);
        assert_eq!(Role::parse("root"), None);
    }

    #[test]
    fn administrators_can_do_everything() {
        let admin = User::test_admin(1);
        assert!(Permission::ALL.into_iter().all(|p| admin.can(p)));
        assert!(admin.require_admin().is_ok());
        assert_eq!(admin.download_scope(), None);
        assert_eq!(admin.manage_scope(), None);
    }

    #[test]
    fn users_have_what_was_granted() {
        let user = User::test_user(2, &[Permission::SubmitDownloads]);
        assert!(user.can(Permission::SubmitDownloads));
        assert!(!user.can(Permission::ManageSettings));
        let err = user.require(Permission::ManageSettings).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Forbidden);
        assert_eq!(user.require_admin().unwrap_err().kind, ErrorKind::Forbidden);
        assert_eq!(user.download_scope(), Some(2));
        assert_eq!(user.manage_scope(), Some(2));
    }

    #[test]
    fn managing_everyone_includes_seeing_everyone() {
        let user = User::test_user(2, &[Permission::ManageAllDownloads]);
        assert!(user.can(Permission::ViewAllDownloads));
        assert_eq!(
            normalize_permissions(&[
                Permission::ManageAllDownloads,
                Permission::SubmitDownloads,
                Permission::SubmitDownloads
            ]),
            vec![
                Permission::SubmitDownloads,
                Permission::ViewAllDownloads,
                Permission::ManageAllDownloads
            ]
        );
    }

    #[test]
    fn onboarding() {
        let mut user = User::test_user(2, &[]);
        assert!(!user.needs_onboarding());
        user.must_change_password = true;
        assert!(user.needs_onboarding());
        user.must_change_password = false;
        user.require_two_factor = true;
        assert!(user.needs_onboarding() && user.needs_two_factor_setup());
        user.two_factor_enabled = true;
        assert!(!user.needs_onboarding());
    }
}
