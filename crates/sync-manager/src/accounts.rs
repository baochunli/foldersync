//! High-level account management service coordinating app-owned encrypted
//! `accounts/<id>/rclone.conf` files, per-state scoped OS secret storage
//! (`<install-id>/<account-id>`), per-account config mutation locks, Drive
//! OAuth flows, explicit single-remote import, and `lsjson --dirs-only`
//! folder-identity resolution (§7, §8, §19).
//!
//! Designed as a small, self-contained API that both `sync-cli` and the
//! `SyncManager` / UI wiring layer (`connect-t4`) can call cleanly.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use rclone_adapter::{
    AccountConfigLock, AuthUrlCallback, ConfigError, ControlledProcessRunner, DriveAuthError,
    DriveAuthRequest, DriveRemoteConfigSpec, DuplicateDirFinding, EngineIdentity, ImportRemoteSpec,
    LsJsonEntry, LsJsonError, LsJsonRequest, PasswordCommand, ProcessControl, ProcessRunner,
    RealProcessRunner, ResolvedRemotePath, check_config_recovery_state,
    cleanup_account_setup_temp_files, create_drive_remote_config_locked, find_duplicate_dir_names,
    import_single_remote_into_encrypted_config_locked, is_global_default_rclone_config,
    list_dirs_with_runner, pinned_binary_path, qualify_binary, qualify_pinned_binary,
    resolve_folder_path_with_runner, run_drive_auth_locked, verify_encrypted_config,
    verify_expected_folder_identity_with_runner,
};
use serde::{Deserialize, Serialize};
use sync_core::{AccountId, CloudCollection, DeviceId};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    clock::{Clock, SystemClock},
    error::ManagerError,
    platform::{
        StateLock, ensure_private_dir_under, reject_symlinks_under_root, resolve_state_dir,
        validate_safe_path_id,
    },
    secrets::{
        SecretStore, SecretStoreError, SecretString, SystemSecretStore, scoped_credential_key,
    },
    store::{AccountRecord, DeviceRecord, SqliteStateStore},
};

/// Reserved account identifier used exclusively by local-to-local CLI fixtures.
/// `add_drive_account` and `import_account` refuse this ID so fixture commands
/// cannot collide with or overwrite real accounts.
pub const RESERVED_LOCAL_FIXTURE_ACCOUNT_ID: &str = "local-fixture";

/// Factory producing the non-secret `--password-command` argv for a given
/// scoped credential key (`<install-id>/<account-id>`).
pub type PasswordCommandFactory =
    Arc<dyn Fn(&str) -> Result<PasswordCommand, AccountError> + Send + Sync>;

fn format_matching_ids(ids: &[Option<String>]) -> String {
    ids.iter()
        .map(|id| id.as_deref().unwrap_or("(no ID)"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Errors returned by [`AccountService`], carrying actionable `cause()` and `next_step()`.
#[derive(Debug, Error)]
pub enum AccountError {
    #[error("{cause} Next step: {next_step}")]
    AuthenticationRequired { cause: String, next_step: String },
    #[error(
        "Refused to import from the default global rclone configuration `{}` without explicit confirmation.",
        path.display()
    )]
    GlobalDefaultConfigRefused { path: PathBuf },
    #[error("Remote `[{remote}]` was not found in `{}`.", path.display())]
    RemoteNotFound { path: PathBuf, remote: String },
    #[error(
        "Source rclone config `{}` is already encrypted and cannot be split into a single remote section.",
        path.display()
    )]
    EncryptedSourceUnsupported { path: PathBuf },
    #[error(
        "Ambiguous duplicate cloud folder `{component}` at `{path}` ({} matching folders; IDs: {}).",
        matching_ids.len(),
        format_matching_ids(matching_ids)
    )]
    AmbiguousDuplicate {
        path: String,
        component: String,
        matching_ids: Vec<Option<String>>,
    },
    #[error(
        "Cloud folder `{path}` changed identity (expected ID `{expected_id}`, found `{actual_id}`)."
    )]
    FolderIdMismatch {
        path: String,
        expected_id: String,
        actual_id: String,
    },
    #[error("Cloud folder `{component}` was not found at `{path}`.")]
    FolderNotFound { path: String, component: String },
    #[error("Account `{account_id}` already exists.")]
    AlreadyExists { account_id: AccountId },
    #[error("Account `{account_id}` was not found.")]
    NotFound { account_id: AccountId },
    #[error("{cause} Next step: {next_step}")]
    InvalidInput { cause: String, next_step: String },
    #[error("{cause} Next step: {next_step}")]
    RecoveryNeeded { cause: String, next_step: String },
    #[error("{cause} Next step: {next_step}")]
    Engine { cause: String, next_step: String },
    #[error("{0}")]
    Manager(Box<ManagerError>),
}

impl From<ManagerError> for AccountError {
    fn from(err: ManagerError) -> Self {
        Self::Manager(Box::new(err))
    }
}

impl AccountError {
    pub fn cause(&self) -> String {
        match self {
            Self::AuthenticationRequired { cause, .. }
            | Self::InvalidInput { cause, .. }
            | Self::RecoveryNeeded { cause, .. }
            | Self::Engine { cause, .. } => cause.clone(),
            Self::GlobalDefaultConfigRefused { .. }
            | Self::RemoteNotFound { .. }
            | Self::EncryptedSourceUnsupported { .. }
            | Self::AmbiguousDuplicate { .. }
            | Self::FolderIdMismatch { .. }
            | Self::FolderNotFound { .. }
            | Self::AlreadyExists { .. }
            | Self::NotFound { .. } => self.to_string(),
            Self::Manager(err) => err.cause(),
        }
    }

    pub fn next_step(&self) -> String {
        match self {
            Self::AuthenticationRequired { next_step, .. }
            | Self::InvalidInput { next_step, .. }
            | Self::RecoveryNeeded { next_step, .. }
            | Self::Engine { next_step, .. } => next_step.clone(),
            Self::GlobalDefaultConfigRefused { .. } => {
                "Specify a separate exported rclone config file or pass --allow-global-config explicitly."
                    .to_string()
            }
            Self::RemoteNotFound { .. } => {
                "Check the exact remote section name in the source rclone config file.".to_string()
            }
            Self::EncryptedSourceUnsupported { .. } => {
                "Export the single remote section to a temporary unencrypted file or configure a new Drive account directly."
                    .to_string()
            }
            Self::AmbiguousDuplicate { .. } => {
                "Rename or consolidate the duplicate folder in Google Drive before syncing."
                    .to_string()
            }
            Self::FolderIdMismatch { .. } => {
                "Review whether the folder or its parent was renamed or replaced in Google Drive before resuming sync."
                    .to_string()
            }
            Self::FolderNotFound { .. } => {
                "Verify the folder exists under the account's shared parent folder.".to_string()
            }
            Self::AlreadyExists { .. } => {
                "Choose a different account ID or run `sync-cli account reconnect` for the existing account."
                    .to_string()
            }
            Self::NotFound { .. } => {
                "Run `sync-cli account list --state-dir <dir>` to inspect configured accounts."
                    .to_string()
            }
            Self::Manager(err) => err.next_step(),
        }
    }
}

impl From<SecretStoreError> for AccountError {
    fn from(err: SecretStoreError) -> Self {
        match err {
            SecretStoreError::AuthenticationRequired { reason } => Self::AuthenticationRequired {
                cause: format!("Credential store is locked or unavailable: {reason}."),
                next_step: "Unlock your macOS login Keychain and retry the account operation."
                    .to_string(),
            },
            SecretStoreError::AlreadyExists { account_id } => Self::AlreadyExists {
                account_id: AccountId::new(account_id),
            },
            SecretStoreError::NotFound { account_id } => Self::AuthenticationRequired {
                cause: format!(
                    "No configuration passphrase was found in the credential store for `{account_id}`."
                ),
                next_step: "Reconnect the account to restore its credential store entry."
                    .to_string(),
            },
            SecretStoreError::InvalidAccountId(id) => Self::InvalidInput {
                cause: format!("Invalid credential identifier `{id}`."),
                next_step:
                    "Use an ASCII alphanumeric identifier containing only letters, digits, `-`, or `_`."
                        .to_string(),
            },
            SecretStoreError::Backend { reason } => Self::Engine {
                cause: format!("Credential store error: {reason}."),
                next_step: "Verify Keychain access permissions and retry.".to_string(),
            },
        }
    }
}

impl From<ConfigError> for AccountError {
    fn from(err: ConfigError) -> Self {
        match err {
            ConfigError::AuthenticationRequired { reason } => Self::AuthenticationRequired {
                cause: format!("Failed to unlock encrypted rclone configuration: {reason}."),
                next_step: "Unlock your macOS Keychain or reconnect the account.".to_string(),
            },
            ConfigError::GlobalDefaultConfigRefused { path } => {
                Self::GlobalDefaultConfigRefused { path }
            }
            ConfigError::RemoteNotFound { path, remote } => Self::RemoteNotFound { path, remote },
            ConfigError::EncryptedSourceUnsupported { path } => {
                Self::EncryptedSourceUnsupported { path }
            }
            ConfigError::UnsupportedDriveImportOption { remote, reason } => Self::InvalidInput {
                cause: format!("Drive import for remote `[{remote}]` rejected: {reason}."),
                next_step: "Export a clean Drive section containing only unquoted allowlisted options (type, client_id, client_secret, scope, token, root_folder_id) and retry."
                    .to_string(),
            },
            ConfigError::RecoveryNeeded {
                config_path,
                backup_paths,
            } => Self::RecoveryNeeded {
                cause: format!(
                    "Recovery needed: account config `{}` is missing while rclone backup file(s) exist ({}); files were left untouched.",
                    config_path.display(),
                    backup_paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                next_step: "Inspect the preserved `rclone.conf.old*` backup file in the account directory and restore it to `rclone.conf` (mode 0600) or remove it before re-adding the account."
                    .to_string(),
            },
            other => Self::Engine {
                cause: other.to_string(),
                next_step:
                    "Verify the pinned rclone binary and account state directory permissions."
                        .to_string(),
            },
        }
    }
}

impl From<DriveAuthError> for AccountError {
    fn from(err: DriveAuthError) -> Self {
        match err {
            DriveAuthError::AuthenticationRequired { reason } => Self::AuthenticationRequired {
                cause: format!("Google Drive authorization or credential store failed: {reason}."),
                next_step:
                    "Unlock your macOS Keychain and complete Google Drive authorization in your browser."
                        .to_string(),
            },
            DriveAuthError::TokenNotSaved { remote } => Self::AuthenticationRequired {
                cause: format!(
                    "Google Drive authorization for remote `{remote}:` exited without saving an OAuth token in the encrypted configuration."
                ),
                next_step:
                    "Run `sync-cli account reconnect --account <id> --state-dir <dir>` (or re-run `account add-drive`) to complete authorization."
                        .to_string(),
            },
            DriveAuthError::Config(cfg) => Self::from(cfg),
            other => Self::Engine {
                cause: other.to_string(),
                next_step:
                    "Verify your OAuth client ID/secret and complete the browser authorization step."
                        .to_string(),
            },
        }
    }
}

impl From<LsJsonError> for AccountError {
    fn from(err: LsJsonError) -> Self {
        match err {
            LsJsonError::AuthenticationRequired { reason } => Self::AuthenticationRequired {
                cause: format!("Remote authentication or credential lookup failed: {reason}."),
                next_step: "Unlock your Keychain or reconnect the Google Drive account."
                    .to_string(),
            },
            LsJsonError::AmbiguousDuplicate {
                path,
                component,
                matching_ids,
                ..
            } => Self::AmbiguousDuplicate {
                path,
                component,
                matching_ids,
            },
            LsJsonError::FolderIdMismatch {
                path,
                expected_id,
                actual_id,
            } => Self::FolderIdMismatch {
                path,
                expected_id,
                actual_id,
            },
            LsJsonError::AncestorIdMismatch {
                path,
                expected_ancestors,
                actual_ancestors,
            } => Self::FolderIdMismatch {
                path,
                expected_id: expected_ancestors.join("/"),
                actual_id: actual_ancestors.join("/"),
            },
            LsJsonError::NotFound { path, component } => Self::FolderNotFound { path, component },
            other => Self::Engine {
                cause: other.to_string(),
                next_step:
                    "Verify network access, account credentials, and the pinned rclone binary."
                        .to_string(),
            },
        }
    }
}

/// Configuration for constructing an [`AccountService`].
pub struct AccountServiceConfig {
    /// Explicit state directory (resolved to canonical absolute path).
    pub state_dir: PathBuf,
    /// Secret store backend (`SystemSecretStore` in production, `InMemorySecretStore` in tests).
    pub secret_store: Arc<dyn SecretStore>,
    /// Process runner (`RealProcessRunner` in production, `FakeProcessRunner` in tests).
    pub process_runner: Arc<dyn ProcessRunner>,
    /// Clock for `created_at` / `updated_at` timestamps.
    pub clock: Arc<dyn Clock>,
    /// Optional explicit pinned rclone binary path override.
    pub rclone_binary_override: Option<PathBuf>,
    /// Optional explicit `sync-cli` helper binary path for `--password-command`.
    pub credential_helper_binary: Option<PathBuf>,
    /// Optional custom `--password-command` factory (for tests injecting a fake helper executable).
    pub password_command_factory: Option<PasswordCommandFactory>,
    /// When `true` (default for CLI), each operation acquires the global `StateLock`.
    /// Set to `false` when called from inside a `SyncManager` that already owns `StateLock`.
    pub acquire_state_lock: bool,
}

impl AccountServiceConfig {
    /// Creates a production config rooted at an explicit `state_dir`.
    pub fn with_state_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            secret_store: Arc::new(SystemSecretStore),
            process_runner: Arc::new(RealProcessRunner),
            clock: Arc::new(SystemClock),
            rclone_binary_override: None,
            credential_helper_binary: None,
            password_command_factory: None,
            acquire_state_lock: true,
        }
    }
}

/// Request to create and optionally authorize a new Google Drive account (§7, §8).
pub struct AddDriveAccountRequest {
    pub id: AccountId,
    pub display_name: String,
    pub remote_name: String,
    pub root_folder_id: String,
    pub client_id: String,
    pub client_secret: SecretString,
    pub scope: String,
    /// When `true`, launches pinned rclone's OAuth browser flow immediately
    /// after creating the encrypted config.
    pub run_oauth_flow: bool,
}

/// Result returned by [`AccountService::add_drive_account`] or [`AccountService::reconnect_drive_account`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveAccountResult {
    pub account: AccountRecord,
    pub auth_url: Option<String>,
}

/// Request to explicitly import ONE named remote from a user-chosen rclone config (§8).
#[derive(Debug, Clone)]
pub struct ImportAccountRequest {
    pub id: AccountId,
    pub display_name: String,
    pub source_config_path: PathBuf,
    pub source_remote_name: String,
    pub target_remote_name: String,
    /// Shared parent folder ID (required for Drive imports if not already set in
    /// the source section; overrides source `root_folder_id` when supplied).
    pub root_folder_id: Option<String>,
    /// Must be `true` to allow reading any default rclone config location.
    pub allow_global_default_source: bool,
}

/// Result returned by [`AccountService::check_account`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountCheckReport {
    pub account: AccountRecord,
    pub encrypted_config_verified: bool,
    pub parent_directories: Vec<LsJsonEntry>,
    pub duplicate_directories: Vec<DuplicateDirFinding>,
}

/// A selectable immediate child of the shared Data folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionCandidate {
    pub collection: CloudCollection,
    pub name: String,
    pub modified_at: String,
}

/// Selectable folders and the sibling names skipped during browsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionListing {
    pub candidates: Vec<CollectionCandidate>,
    pub skipped: Vec<SkippedCollection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedCollection {
    pub name: String,
    pub reason: CollectionSkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollectionSkipReason {
    #[serde(rename = "unsupported name")]
    UnsupportedName,
    #[serde(rename = "duplicate")]
    Duplicate,
}

/// Account operation started by the manager's cancellable authorization API.
pub enum DriveAuthorization {
    Connect(AddDriveAccountRequest),
    Reconnect(AccountId),
}

pub(crate) struct LockedDrive<'a> {
    _lock: AccountConfigLock,
    pub account: AccountRecord,
    pub request: LsJsonRequest,
    pub runner: ControlledProcessRunner<'a>,
}

pub(crate) const DRIVE_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

impl LockedDrive<'_> {
    pub fn verify_collection(
        &self,
        collection: &CloudCollection,
    ) -> Result<CloudCollection, AccountError> {
        validate_drive_folder_id(&collection.folder_id)?;
        if !collection.display_path.starts_with("Data/") {
            return Err(AccountError::InvalidInput {
                cause: "Drive collections must be inside the shared Data folder.".into(),
                next_step: "Choose a collection under Data.".into(),
            });
        }
        if let Some(finding) = sync_core::validate_names(&[&collection.display_path])
            .into_iter()
            .next()
        {
            return Err(AccountError::InvalidInput {
                cause: finding.cause,
                next_step: finding.next_step,
            });
        }
        let root = Some(self.account.remote_root_folder_id.as_str());
        let verified = verify_expected_folder_identity_with_runner(
            &self.request,
            &collection.display_path,
            &collection.folder_id,
            root,
            (!collection.ancestor_ids.is_empty()).then_some(collection.ancestor_ids.as_slice()),
            &self.runner,
        )?;
        Ok(CloudCollection {
            display_path: verified.display_path.clone(),
            folder_id: collection.folder_id.clone(),
            ancestor_ids: verified.ancestor_ids(root),
        })
    }

    fn data_id(&self) -> Result<String, AccountError> {
        let data = resolve_folder_path_with_runner(&self.request, "Data", true, &self.runner)?;
        Ok(data
            .leaf_folder_id()
            .expect("Data folder ID required")
            .to_owned())
    }

    fn candidates(&self, data_id: &str) -> Result<CollectionListing, AccountError> {
        let entries = list_dirs_with_runner(&self.request, "Data", &self.runner)?;
        let duplicates: BTreeSet<_> = find_duplicate_dir_names(&entries)
            .into_iter()
            .map(|duplicate| duplicate.name)
            .collect();
        let mut listing = CollectionListing {
            candidates: Vec::new(),
            skipped: duplicates
                .iter()
                .map(|name| SkippedCollection {
                    name: name.clone(),
                    reason: CollectionSkipReason::Duplicate,
                })
                .collect(),
        };
        for entry in entries.into_iter().filter(|entry| entry.is_dir) {
            if duplicates.contains(&entry.name) {
                continue;
            }
            if validate_collection_name(&entry.name).is_err() {
                listing.skipped.push(SkippedCollection {
                    name: entry.name,
                    reason: CollectionSkipReason::UnsupportedName,
                });
                continue;
            }
            listing.candidates.push(self.candidate(data_id, entry)?);
        }
        Ok(listing)
    }

    fn candidate(
        &self,
        data_id: &str,
        entry: LsJsonEntry,
    ) -> Result<CollectionCandidate, AccountError> {
        if entry.path != entry.name {
            return Err(AccountError::InvalidInput {
                cause: "The Data listing returned a folder outside its immediate children.".into(),
                next_step: "Refresh the folder list and review the shared Data folder.".into(),
            });
        }
        let folder_id = entry.id.unwrap_or_default();
        validate_drive_folder_id(&folder_id)?;
        Ok(CollectionCandidate {
            collection: CloudCollection {
                display_path: format!("Data/{}", entry.name),
                folder_id,
                ancestor_ids: vec![
                    self.account.remote_root_folder_id.clone(),
                    data_id.to_owned(),
                ],
            },
            name: entry.name,
            modified_at: entry.mod_time,
        })
    }
}

pub(crate) fn validate_drive_folder_id(id: &str) -> Result<(), AccountError> {
    if id.is_empty()
        || id.len() > 255
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(AccountError::InvalidInput {
            cause: "The Drive folder returned an invalid ID.".into(),
            next_step: "Refresh the Drive folder list and select the folder again.".into(),
        });
    }
    Ok(())
}

fn validate_collection_name(name: &str) -> Result<(), AccountError> {
    if name.trim().is_empty() || name.contains('/') {
        return Err(AccountError::InvalidInput {
            cause: "Enter one folder name inside Data.".into(),
            next_step: "Use a single nonempty folder name.".into(),
        });
    }
    if let Some(finding) = sync_core::validate_names(&[name]).into_iter().next() {
        return Err(AccountError::InvalidInput {
            cause: finding.cause,
            next_step: finding.next_step,
        });
    }
    Ok(())
}

/// Account management service for encrypted rclone configs, per-state scoped
/// credentials, Drive OAuth flows, explicit single-remote import, and
/// folder-identity resolution.
pub struct AccountService {
    state_dir: PathBuf,
    secret_store: Arc<dyn SecretStore>,
    process_runner: Arc<dyn ProcessRunner>,
    clock: Arc<dyn Clock>,
    rclone_binary_override: Option<PathBuf>,
    qualified_engine: Mutex<Option<EngineIdentity>>,
    credential_helper_binary: Option<PathBuf>,
    password_command_factory: Option<PasswordCommandFactory>,
    acquire_state_lock: bool,
    startup_warnings: Vec<String>,
}

impl AccountService {
    /// Opens an [`AccountService`] bound to `config.state_dir`, removes any
    /// leftover `.foldersync-setup-tmp-*` files inside existing account
    /// directories, and records startup warnings for any missing-config
    /// recovery-needed backups or stale installation-scoped credentials.
    pub fn new(config: AccountServiceConfig) -> Result<Self, AccountError> {
        let mut svc = Self::from_config(config)?;
        svc.startup_warnings = svc.cleanup_stale_account_artifacts();
        Ok(svc)
    }

    pub(crate) fn from_config(config: AccountServiceConfig) -> Result<Self, AccountError> {
        let state_dir = resolve_state_dir(Some(&config.state_dir))?;
        Ok(Self {
            state_dir,
            secret_store: config.secret_store,
            process_runner: config.process_runner,
            clock: config.clock,
            rclone_binary_override: config.rclone_binary_override,
            qualified_engine: Mutex::new(None),
            credential_helper_binary: config.credential_helper_binary,
            password_command_factory: config.password_command_factory,
            acquire_state_lock: config.acquire_state_lock,
            startup_warnings: Vec::new(),
        })
    }

    /// Convenience constructor for callers (such as `SyncManager`) that already
    /// hold the exclusive `StateLock` on `state_dir`. Per-account config locks
    /// are still always acquired on every operation.
    pub fn for_locked_manager(mut config: AccountServiceConfig) -> Result<Self, AccountError> {
        config.acquire_state_lock = false;
        Self::new(config)
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Non-fatal startup diagnostics (such as recovery-needed missing configs
    /// with preserved `rclone.conf.old*` backups, or stale credentials left
    /// behind by an interrupted registration).
    pub fn startup_warnings(&self) -> &[String] {
        &self.startup_warnings
    }

    /// Best-effort startup cleanup of ONLY FolderSync's `.foldersync-setup-tmp-*`
    /// temp files across `<state_dir>/accounts/*`, plus inspection for
    /// recovery-needed backups and stale credentials scoped to this installation.
    pub fn cleanup_stale_account_artifacts(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let accounts_root = self.state_dir.join("accounts");
        let Ok(meta) = fs::symlink_metadata(&accounts_root) else {
            return warnings;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return warnings;
        }
        let Ok(entries) = fs::read_dir(&accounts_root) else {
            return warnings;
        };

        // Only inspect state.sqlite if it already exists and is not a symlink
        // (never create state.sqlite during startup scan).
        let db_path = self.state_dir.join("state.sqlite");
        let existing_store = match fs::symlink_metadata(&db_path) {
            Ok(m) if !m.file_type().is_symlink() && m.is_file() => {
                SqliteStateStore::open(&db_path).ok()
            }
            _ => None,
        };

        for entry in entries.flatten() {
            let acct_dir = entry.path();
            if let Ok(emeta) = fs::symlink_metadata(&acct_dir)
                && !emeta.file_type().is_symlink()
                && emeta.is_dir()
            {
                let lock_path = acct_dir.join("config.lock");
                if let Ok(_lock) = AccountConfigLock::acquire(&lock_path) {
                    let _ = cleanup_account_setup_temp_files(&acct_dir);
                    let acct_name = acct_dir
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown");
                    if let Err(ConfigError::RecoveryNeeded { backup_paths, .. }) =
                        check_config_recovery_state(&acct_dir.join("rclone.conf"))
                    {
                        warnings.push(format!(
                            "Recovery needed for account `{acct_name}`: `rclone.conf` is missing while rclone backup file(s) exist ({}); files were left untouched.",
                            backup_paths
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    if let Some(store) = &existing_store
                        && validate_safe_path_id("account_id", acct_name).is_ok()
                    {
                        let acct_id = AccountId::new(acct_name);
                        if matches!(store.get_account(&acct_id), Ok(None))
                            && let Ok(cred_key) = self.credential_key_for(store, &acct_id)
                            && self.secret_store.get_secret(&cred_key).is_ok()
                        {
                            warnings.push(format!(
                                "Stale credential found for incomplete account `{acct_name}` (no account record in state.sqlite); retry `account add-drive` or `account import` with `--id {acct_name}` to recover."
                            ));
                        }
                    }
                }
            }
        }
        warnings
    }

    fn maybe_lock_state(&self) -> Result<Option<StateLock>, AccountError> {
        if self.acquire_state_lock {
            Ok(Some(StateLock::acquire(&self.state_dir)?))
        } else {
            Ok(None)
        }
    }

    fn open_store(&self) -> Result<SqliteStateStore, AccountError> {
        let db_path = self.state_dir.join("state.sqlite");
        Ok(SqliteStateStore::open(&db_path)?)
    }

    fn installation_id(&self, store: &SqliteStateStore) -> Result<String, AccountError> {
        let default_device = DeviceRecord {
            id: DeviceId::new(format!("device-{}", Uuid::new_v4())),
            name: "foldersync-installation".to_string(),
            created_at: self.now_rfc3339(),
        };
        let device = store.ensure_device(&default_device)?;
        Ok(device.id.as_str().to_string())
    }

    fn credential_key_for(
        &self,
        store: &SqliteStateStore,
        id: &AccountId,
    ) -> Result<String, AccountError> {
        let install_id = self.installation_id(store)?;
        Ok(scoped_credential_key(&install_id, id.as_str())?)
    }

    fn now_rfc3339(&self) -> String {
        self.clock.now_rfc3339()
    }

    fn qualify_engine(&self) -> Result<EngineIdentity, AccountError> {
        let mut qualified = self.qualified_engine.lock().unwrap();
        if let Some(engine) = qualified.as_ref() {
            return Ok(engine.clone());
        }
        let engine = match self.rclone_binary_override.as_deref() {
            Some(path) => qualify_binary(path).map(Some),
            None => qualify_pinned_binary(),
        }
        .map_err(|err| AccountError::Engine {
            cause: err.to_string(),
            next_step: "Verify the installed FolderSync rclone executable.".to_string(),
        })?
        .ok_or_else(|| AccountError::Engine {
            cause: "Pinned rclone binary is not installed.".to_string(),
            next_step: "Run scripts/fetch-rclone.sh to install the verified development engine."
                .to_string(),
        })?;
        *qualified = Some(engine.clone());
        Ok(engine)
    }

    fn reject_reserved_account_id(id: &AccountId) -> Result<(), AccountError> {
        if id.as_str() == RESERVED_LOCAL_FIXTURE_ACCOUNT_ID {
            return Err(AccountError::InvalidInput {
                cause: format!(
                    "Account ID `{RESERVED_LOCAL_FIXTURE_ACCOUNT_ID}` is reserved for local-to-local fixture mappings."
                ),
                next_step:
                    "Choose a different account ID (for example, `household-a` or `imported-1`)."
                        .to_string(),
            });
        }
        Ok(())
    }

    /// Returns the validated private directory `<state_dir>/accounts/<id>`,
    /// refusing symlinks anywhere under `state_dir`.
    pub fn account_dir(&self, id: &AccountId) -> Result<PathBuf, AccountError> {
        validate_safe_path_id("account_id", id.as_str())?;
        let candidate = self.state_dir.join("accounts").join(id.as_str());
        Ok(reject_symlinks_under_root(
            "account_dir",
            &self.state_dir,
            &candidate,
        )?)
    }

    /// Returns `<state_dir>/accounts/<id>/rclone.conf`, refusing symlinks under `state_dir`.
    pub fn account_config_path(&self, id: &AccountId) -> Result<PathBuf, AccountError> {
        let dir = self.account_dir(id)?;
        let cfg = dir.join("rclone.conf");
        Ok(reject_symlinks_under_root(
            "account_config",
            &self.state_dir,
            &cfg,
        )?)
    }

    /// Returns `<state_dir>/accounts/<id>/config.lock`, refusing symlinks under `state_dir`.
    pub fn account_lock_path(&self, id: &AccountId) -> Result<PathBuf, AccountError> {
        let dir = self.account_dir(id)?;
        let lock = dir.join("config.lock");
        Ok(reject_symlinks_under_root(
            "account_lock",
            &self.state_dir,
            &lock,
        )?)
    }

    /// Acquires the per-account config mutation lock (`accounts/<id>/config.lock`).
    pub fn acquire_account_lock(&self, id: &AccountId) -> Result<AccountConfigLock, AccountError> {
        let dir = self.account_dir(id)?;
        ensure_private_dir_under("account_dir", &self.state_dir, &dir)?;
        let lock_path = self.account_lock_path(id)?;
        Ok(AccountConfigLock::acquire(&lock_path)?)
    }

    pub(crate) fn try_account_lock(
        &self,
        id: &AccountId,
    ) -> Result<AccountConfigLock, AccountError> {
        let path = self.account_lock_path(id)?;
        AccountConfigLock::try_acquire(&path)?.ok_or_else(|| AccountError::InvalidInput {
            cause: "Another operation is using this account.".to_owned(),
            next_step: "Try again when the account operation finishes.".to_owned(),
        })
    }

    /// Builds the validated non-secret `--password-command`
    /// (`sync-cli credential-helper --account <install-id>/<account-id>`).
    pub fn password_command_for(&self, id: &AccountId) -> Result<PasswordCommand, AccountError> {
        validate_safe_path_id("account_id", id.as_str())?;
        let store = self.open_store()?;
        let cred_key = self.credential_key_for(&store, id)?;
        self.password_command_for_key(&cred_key)
    }

    fn password_command_for_key(&self, cred_key: &str) -> Result<PasswordCommand, AccountError> {
        if let Some(factory) = &self.password_command_factory {
            return factory(cred_key);
        }
        let helper_bin = match &self.credential_helper_binary {
            Some(p) => p.clone(),
            None => std::env::current_exe().map_err(|err| AccountError::Engine {
                cause: format!(
                    "Could not resolve current executable for credential-helper: {err}."
                ),
                next_step: "Pass an explicit credential-helper binary path.".to_string(),
            })?,
        };
        let canonical = helper_bin.canonicalize().unwrap_or(helper_bin);
        PasswordCommand::new(
            canonical,
            vec![
                "credential-helper".to_string(),
                "--account".to_string(),
                cred_key.to_string(),
            ],
        )
        .map_err(|err| AccountError::InvalidInput {
            cause: err.to_string(),
            next_step:
                "Configure a valid absolute path to the FolderSync credential helper binary."
                    .to_string(),
        })
    }

    fn register_or_replace_stale_credential(
        &self,
        cred_key: &str,
        passphrase: &SecretString,
    ) -> Result<(), AccountError> {
        match self.secret_store.create_secret(cred_key, passphrase) {
            Ok(()) => Ok(()),
            // Caller already verified (under per-account lock) that NO account row
            // exists in THIS state's SQLite DB and `cred_key` is scoped to THIS
            // state's random installation ID, so any existing item is stale from an
            // interrupted registration of this same installation.
            Err(SecretStoreError::AlreadyExists { .. }) => {
                self.secret_store.set_secret(cred_key, passphrase)?;
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
    }

    fn clear_orphaned_unregistered_config(&self, config_path: &Path) -> Result<(), AccountError> {
        check_config_recovery_state(config_path)?;
        if let Ok(meta) = fs::symlink_metadata(config_path)
            && !meta.file_type().is_symlink()
            && meta.is_file()
        {
            let _ = fs::remove_file(config_path);
        }
        Ok(())
    }

    fn verify_drive_root_listing_locked(
        &self,
        engine: EngineIdentity,
        config_path: PathBuf,
        remote_name: String,
        pw_cmd: PasswordCommand,
        runner: &dyn ProcessRunner,
    ) -> Result<(), AccountError> {
        let ls_req = LsJsonRequest::new(engine, config_path, remote_name, true, Some(pw_cmd))?;
        list_dirs_with_runner(&ls_req, "", runner)?;
        Ok(())
    }

    /// Creates a new app-owned encrypted Google Drive account config (`0600`)
    /// rooted at `req.root_folder_id` with the user's own OAuth client credentials,
    /// stores the config passphrase in [`SecretStore`] under `<install-id>/<account-id>`,
    /// persists the account row in SQLite, optionally runs the pinned rclone browser
    /// OAuth flow, and marks `ready` ONLY after verifying both saved token presence
    /// (`config redacted`) and a lightweight root listing (`lsjson --max-depth 1 --dirs-only`)
    /// under the per-account lock.
    ///
    /// If a prior Drive registration for the same ID, remote name, and root folder ID
    /// ended in a retryable non-ready state (`pending` or `authentication_required`),
    /// re-running `add_drive_account` resumes OAuth against the existing encrypted config.
    pub fn add_drive_account(
        &self,
        req: AddDriveAccountRequest,
        on_auth_url: Option<AuthUrlCallback>,
    ) -> Result<DriveAccountResult, AccountError> {
        self.add_drive_account_using(req, on_auth_url, None)
    }

    pub(crate) fn add_drive_account_using(
        &self,
        req: AddDriveAccountRequest,
        on_auth_url: Option<AuthUrlCallback>,
        control: Option<&ProcessControl>,
    ) -> Result<DriveAccountResult, AccountError> {
        let runner = self.process_runner.as_ref();
        let controlled = control.map(|control| ControlledProcessRunner { runner, control });
        let runner = controlled
            .as_ref()
            .map_or(runner, |r| r as &dyn ProcessRunner);
        validate_safe_path_id("account_id", req.id.as_str())?;
        Self::reject_reserved_account_id(&req.id)?;

        // Validate inputs BEFORE creating directories or touching SQLite/SecretStore.
        let remote_name = if req.remote_name.trim().is_empty() {
            "household".to_string()
        } else {
            req.remote_name.trim().to_string()
        };
        let scope = if req.scope.trim().is_empty() {
            "drive".to_string()
        } else {
            req.scope.trim().to_string()
        };
        let root_folder_id = req.root_folder_id.trim().to_string();
        if req.client_id.trim().is_empty()
            || req.client_secret.is_empty()
            || root_folder_id.is_empty()
        {
            return Err(AccountError::InvalidInput {
                cause: "Google Drive account requires non-empty client_id, client_secret, and root_folder_id.".to_string(),
                next_step: "Provide your OAuth client ID, client secret, and shared parent folder ID.".to_string(),
            });
        }

        let _state_lock = self.maybe_lock_state()?;
        // Hold ONE per-account lock across existence checks, credential registration,
        // config encryption/publication, DB persistence, OAuth, and root verification.
        let account_dir = self.account_dir(&req.id)?;
        ensure_private_dir_under("account_dir", &self.state_dir, &account_dir)?;
        let acct_lock = if control.is_some() {
            self.try_account_lock(&req.id)?
        } else {
            self.acquire_account_lock(&req.id)?
        };
        let config_path = self.account_config_path(&req.id)?;
        let lock_path = self.account_lock_path(&req.id)?;

        let store = self.open_store()?;
        let cred_key = self.credential_key_for(&store, &req.id)?;

        if let Some(mut existing) = store.get_account(&req.id)? {
            check_config_recovery_state(&config_path)?;
            let can_resume = existing.kind == "drive"
                && existing.auth_status != "ready"
                && existing.remote_name == remote_name
                && existing.remote_root_folder_id == root_folder_id
                && config_path.is_file()
                && req.run_oauth_flow;
            if !can_resume {
                return Err(AccountError::AlreadyExists {
                    account_id: req.id.clone(),
                });
            }

            let engine = self.qualify_engine()?;
            let pw_cmd = self.password_command_for_key(&cred_key)?;
            let auth_req = DriveAuthRequest::new(
                engine.clone(),
                config_path.clone(),
                lock_path,
                existing.remote_name.clone(),
                pw_cmd.clone(),
            )?
            .caller_opens_browser(control.is_some());
            let outcome = match run_drive_auth_locked(&auth_req, &acct_lock, runner, on_auth_url) {
                Ok(outcome) => outcome,
                Err(err) => {
                    existing.auth_status = "authentication_required".to_string();
                    existing.updated_at = self.now_rfc3339();
                    store.upsert_account(&existing)?;
                    return Err(err.into());
                }
            };
            if let Err(err) = self.verify_drive_root_listing_locked(
                engine,
                config_path,
                existing.remote_name.clone(),
                pw_cmd,
                runner,
            ) {
                existing.auth_status = "authentication_required".to_string();
                existing.updated_at = self.now_rfc3339();
                store.upsert_account(&existing)?;
                return Err(err);
            }
            existing.auth_status = "ready".to_string();
            existing.updated_at = self.now_rfc3339();
            store.upsert_account(&existing)?;
            return Ok(DriveAccountResult {
                account: existing,
                auth_url: outcome.auth_url,
            });
        }

        // No account row exists in this state DB. Refuse if recovery-needed backups
        // exist, and clean any orphaned unregistered config left by a prior crash.
        self.clear_orphaned_unregistered_config(&config_path)?;

        let engine = self.qualify_engine()?;

        let passphrase = SecretString::generate_random_passphrase()?;
        self.register_or_replace_stale_credential(&cred_key, &passphrase)?;
        let created_credential = true;

        let pw_cmd = match self.password_command_for_key(&cred_key) {
            Ok(cmd) => cmd,
            Err(err) => {
                if created_credential {
                    let _ = self.secret_store.delete_secret(&cred_key);
                }
                return Err(err);
            }
        };

        let drive_spec = DriveRemoteConfigSpec {
            remote_name: remote_name.clone(),
            client_id: req.client_id.clone(),
            client_secret: req.client_secret.expose().to_string(),
            scope,
            root_folder_id: root_folder_id.clone(),
        };

        if let Err(err) = create_drive_remote_config_locked(
            &engine,
            runner,
            &config_path,
            &acct_lock,
            &pw_cmd,
            &drive_spec,
        ) {
            if created_credential {
                let _ = self.secret_store.delete_secret(&cred_key);
            }
            let _ = cleanup_account_setup_temp_files(&account_dir);
            return Err(err.into());
        }

        let now = self.now_rfc3339();
        let mut record = AccountRecord {
            id: req.id.clone(),
            display_name: if req.display_name.trim().is_empty() {
                req.id.as_str().to_string()
            } else {
                req.display_name.trim().to_string()
            },
            kind: "drive".to_string(),
            remote_name: remote_name.clone(),
            remote_root_folder_id: root_folder_id,
            config_path: config_path.clone(),
            auth_status: "pending".to_string(),
            created_at: now.clone(),
            updated_at: now,
        };
        if let Err(err) = store.upsert_account(&record) {
            let _ = fs::remove_file(&config_path);
            if created_credential {
                let _ = self.secret_store.delete_secret(&cred_key);
            }
            return Err(err.into());
        }

        let mut auth_url = None;
        if req.run_oauth_flow {
            let auth_req = DriveAuthRequest::new(
                engine.clone(),
                config_path.clone(),
                lock_path,
                remote_name.clone(),
                pw_cmd.clone(),
            )?
            .caller_opens_browser(control.is_some());
            match run_drive_auth_locked(&auth_req, &acct_lock, runner, on_auth_url) {
                Ok(outcome) => {
                    if let Err(err) = self.verify_drive_root_listing_locked(
                        engine,
                        config_path,
                        remote_name,
                        pw_cmd,
                        runner,
                    ) {
                        record.auth_status = "authentication_required".to_string();
                        record.updated_at = self.now_rfc3339();
                        store.upsert_account(&record)?;
                        return Err(err);
                    }
                    auth_url = outcome.auth_url;
                    record.auth_status = "ready".to_string();
                    record.updated_at = self.now_rfc3339();
                    store.upsert_account(&record)?;
                }
                Err(err) => {
                    record.auth_status = "authentication_required".to_string();
                    record.updated_at = self.now_rfc3339();
                    store.upsert_account(&record)?;
                    return Err(err.into());
                }
            }
        }

        Ok(DriveAccountResult {
            account: record,
            auth_url,
        })
    }

    /// Resumes or refreshes Google Drive OAuth authorization for an existing
    /// Drive account, marking `ready` ONLY after both saved token presence
    /// (`config redacted`) and a lightweight root listing
    /// (`lsjson --max-depth 1 --dirs-only`) succeed under the per-account lock.
    pub fn reconnect_drive_account(
        &self,
        id: &AccountId,
        on_auth_url: Option<AuthUrlCallback>,
    ) -> Result<DriveAccountResult, AccountError> {
        self.reconnect_drive_account_using(id, on_auth_url, None)
    }

    pub(crate) fn reconnect_drive_account_using(
        &self,
        id: &AccountId,
        on_auth_url: Option<AuthUrlCallback>,
        control: Option<&ProcessControl>,
    ) -> Result<DriveAccountResult, AccountError> {
        let runner = self.process_runner.as_ref();
        let controlled = control.map(|control| ControlledProcessRunner { runner, control });
        let runner = controlled
            .as_ref()
            .map_or(runner, |r| r as &dyn ProcessRunner);
        validate_safe_path_id("account_id", id.as_str())?;
        let _state_lock = self.maybe_lock_state()?;
        let acct_lock = if control.is_some() {
            self.try_account_lock(id)?
        } else {
            self.acquire_account_lock(id)?
        };
        let store = self.open_store()?;
        let mut account = self.load_account_for_op(&store, id)?;

        if account.kind != "drive" {
            return Err(AccountError::InvalidInput {
                cause: format!(
                    "Account `{}` has kind `{}` and does not support Drive OAuth reconnect.",
                    account.id, account.kind
                ),
                next_step: "Only Google Drive accounts use `account reconnect`.".to_string(),
            });
        }

        let config_path =
            reject_symlinks_under_root("account_config", &self.state_dir, &account.config_path)?;
        check_config_recovery_state(&config_path)?;
        let lock_path = self.account_lock_path(&account.id)?;
        let cred_key = self.credential_key_for(&store, &account.id)?;

        // Fail fast if credential store is locked.
        if let Err(err) = self.secret_store.get_secret(&cred_key) {
            let acct_err = AccountError::from(err);
            if matches!(acct_err, AccountError::AuthenticationRequired { .. }) {
                account.auth_status = "authentication_required".to_string();
                account.updated_at = self.now_rfc3339();
                store.upsert_account(&account)?;
            }
            return Err(acct_err);
        }

        let engine = self.qualify_engine()?;
        let pw_cmd = self.password_command_for_key(&cred_key)?;
        let auth_req = DriveAuthRequest::new(
            engine.clone(),
            config_path.clone(),
            lock_path,
            account.remote_name.clone(),
            pw_cmd.clone(),
        )?
        .caller_opens_browser(control.is_some());
        match run_drive_auth_locked(&auth_req, &acct_lock, runner, on_auth_url) {
            Ok(outcome) => {
                if let Err(err) = self.verify_drive_root_listing_locked(
                    engine,
                    config_path,
                    account.remote_name.clone(),
                    pw_cmd,
                    runner,
                ) {
                    account.auth_status = "authentication_required".to_string();
                    account.updated_at = self.now_rfc3339();
                    store.upsert_account(&account)?;
                    return Err(err);
                }
                account.auth_status = "ready".to_string();
                account.updated_at = self.now_rfc3339();
                store.upsert_account(&account)?;
                Ok(DriveAccountResult {
                    account,
                    auth_url: outcome.auth_url,
                })
            }
            Err(err) => {
                account.auth_status = "authentication_required".to_string();
                account.updated_at = self.now_rfc3339();
                store.upsert_account(&account)?;
                Err(err.into())
            }
        }
    }

    /// Explicitly imports ONE named remote from a user-chosen rclone config into
    /// a newly encrypted app-owned `accounts/<id>/rclone.conf` without mutating
    /// the source file (§8).
    ///
    /// For Drive imports, accepts only unquoted allowlisted options, binds
    /// `root_folder_id` to the requested shared-parent ID, and verifies the
    /// effective root via `lsjson --max-depth 1 --dirs-only` before recording
    /// the account in SQLite.
    pub fn import_account(&self, req: ImportAccountRequest) -> Result<AccountRecord, AccountError> {
        validate_safe_path_id("account_id", req.id.as_str())?;
        Self::reject_reserved_account_id(&req.id)?;

        let source_abs = if req.source_config_path.is_absolute() {
            req.source_config_path.clone()
        } else {
            std::env::current_dir()
                .map_err(|err| AccountError::InvalidInput {
                    cause: format!("Failed to resolve relative source config path: {err}."),
                    next_step: "Pass an absolute path to --from.".to_string(),
                })?
                .join(&req.source_config_path)
        };

        // Refuse default rclone configs (including rclone's executable dir) BEFORE
        // creating directories, opening SQLite, or touching SecretStore.
        let exe_dir = self
            .rclone_binary_override
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| {
                pinned_binary_path()
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
            });
        if !req.allow_global_default_source
            && is_global_default_rclone_config(&source_abs, None, None, exe_dir.as_deref())
        {
            return Err(AccountError::GlobalDefaultConfigRefused { path: source_abs });
        }

        let engine = self.qualify_engine()?;
        if !req.allow_global_default_source
            && is_global_default_rclone_config(&source_abs, None, None, engine.path().parent())
        {
            return Err(AccountError::GlobalDefaultConfigRefused { path: source_abs });
        }

        let _state_lock = self.maybe_lock_state()?;
        // Hold ONE per-account lock across existence checks, credential registration,
        // config encryption/publication, root verification, and DB insert.
        let acct_lock = self.acquire_account_lock(&req.id)?;
        let account_dir = self.account_dir(&req.id)?;
        let target_config_path = self.account_config_path(&req.id)?;
        let lock_path = self.account_lock_path(&req.id)?;

        let store = self.open_store()?;
        if store.get_account(&req.id)?.is_some() {
            return Err(AccountError::AlreadyExists {
                account_id: req.id.clone(),
            });
        }

        // Refuse if recovery-needed backups exist; remove any orphaned unregistered config.
        self.clear_orphaned_unregistered_config(&target_config_path)?;

        let cred_key = self.credential_key_for(&store, &req.id)?;

        let passphrase = SecretString::generate_random_passphrase()?;
        self.register_or_replace_stale_credential(&cred_key, &passphrase)?;
        let created_credential = true;

        let pw_cmd = match self.password_command_for_key(&cred_key) {
            Ok(cmd) => cmd,
            Err(err) => {
                if created_credential {
                    let _ = self.secret_store.delete_secret(&cred_key);
                }
                return Err(err);
            }
        };

        let target_remote = if req.target_remote_name.trim().is_empty() {
            req.source_remote_name.trim().to_string()
        } else {
            req.target_remote_name.trim().to_string()
        };

        let imported = match import_single_remote_into_encrypted_config_locked(
            &engine,
            self.process_runner.as_ref(),
            &ImportRemoteSpec {
                source_config_path: source_abs,
                source_remote_name: req.source_remote_name.trim().to_string(),
                target_remote_name: target_remote,
                target_config_path: target_config_path.clone(),
                lock_path,
                password_command: pw_cmd.clone(),
                override_root_folder_id: req.root_folder_id.clone(),
                allow_global_default_source: req.allow_global_default_source,
            },
            &acct_lock,
        ) {
            Ok(info) => info,
            Err(err) => {
                if created_credential {
                    let _ = self.secret_store.delete_secret(&cred_key);
                }
                let _ = cleanup_account_setup_temp_files(&account_dir);
                return Err(err.into());
            }
        };

        // For Drive imports, verify the effective shared-parent root via
        // `lsjson --max-depth 1 --dirs-only` BEFORE recording in SQLite.
        if imported.remote_type.eq_ignore_ascii_case("drive")
            && let Err(err) = self.verify_drive_root_listing_locked(
                engine,
                target_config_path.clone(),
                imported.target_remote_name.clone(),
                pw_cmd,
                self.process_runner.as_ref(),
            )
        {
            let _ = fs::remove_file(&target_config_path);
            let _ = cleanup_account_setup_temp_files(&account_dir);
            if created_credential {
                let _ = self.secret_store.delete_secret(&cred_key);
            }
            return Err(err);
        }

        let root_folder_id = imported.root_folder_id.unwrap_or_default();
        let now = self.now_rfc3339();
        let record = AccountRecord {
            id: req.id.clone(),
            display_name: if req.display_name.trim().is_empty() {
                req.id.as_str().to_string()
            } else {
                req.display_name.trim().to_string()
            },
            kind: imported.remote_type,
            remote_name: imported.target_remote_name,
            remote_root_folder_id: root_folder_id,
            config_path: target_config_path.clone(),
            auth_status: "ready".to_string(),
            created_at: now.clone(),
            updated_at: now,
        };
        if let Err(err) = store.upsert_account(&record) {
            let _ = fs::remove_file(&target_config_path);
            if created_credential {
                let _ = self.secret_store.delete_secret(&cred_key);
            }
            return Err(err.into());
        }
        Ok(record)
    }

    /// Lists all persisted accounts ordered by creation time and ID through the
    /// checked [`SqliteStateStore`] (without reopening the database path).
    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>, AccountError> {
        let _state_lock = self.maybe_lock_state()?;
        let store = self.open_store()?;
        Ok(store.list_accounts()?)
    }

    /// Fetches one account record by ID.
    pub fn get_account(&self, id: &AccountId) -> Result<Option<AccountRecord>, AccountError> {
        let _state_lock = self.maybe_lock_state()?;
        let store = self.open_store()?;
        Ok(store.get_account(id)?)
    }

    fn load_account_for_op(
        &self,
        store: &SqliteStateStore,
        id: &AccountId,
    ) -> Result<AccountRecord, AccountError> {
        validate_safe_path_id("account_id", id.as_str())?;
        store
            .get_account(id)?
            .ok_or_else(|| AccountError::NotFound {
                account_id: id.clone(),
            })
    }

    /// Verifies the account's encrypted config and lists immediate child
    /// directories of its shared parent folder via `rclone lsjson --dirs-only`
    /// while holding the per-account config lock (so token refreshes serialize).
    pub fn check_account(&self, id: &AccountId) -> Result<AccountCheckReport, AccountError> {
        validate_safe_path_id("account_id", id.as_str())?;
        let _state_lock = self.maybe_lock_state()?;
        let _acct_lock = self.acquire_account_lock(id)?;
        let store = self.open_store()?;
        let mut account = self.load_account_for_op(&store, id)?;

        let config_path =
            reject_symlinks_under_root("account_config", &self.state_dir, &account.config_path)?;
        check_config_recovery_state(&config_path)?;
        let cred_key = self.credential_key_for(&store, &account.id)?;

        // Verify credential store is unlocked BEFORE building password-command
        // or qualifying rclone so a locked Keychain immediately records
        // `authentication_required` and fails without any hidden prompt.
        if let Err(err) = self.secret_store.get_secret(&cred_key) {
            let acct_err = AccountError::from(err);
            if matches!(acct_err, AccountError::AuthenticationRequired { .. }) {
                account.auth_status = "authentication_required".to_string();
                account.updated_at = self.now_rfc3339();
                store.upsert_account(&account)?;
            }
            return Err(acct_err);
        }

        let engine = self.qualify_engine()?;
        let pw_cmd = self.password_command_for_key(&cred_key)?;

        if let Err(err) =
            verify_encrypted_config(&engine, self.process_runner.as_ref(), &config_path, &pw_cmd)
        {
            let acct_err = AccountError::from(err);
            if matches!(acct_err, AccountError::AuthenticationRequired { .. }) {
                account.auth_status = "authentication_required".to_string();
                account.updated_at = self.now_rfc3339();
                store.upsert_account(&account)?;
            }
            return Err(acct_err);
        }

        let is_drive = account.kind == "drive";
        let ls_req = LsJsonRequest::new(
            engine,
            config_path,
            account.remote_name.clone(),
            is_drive,
            Some(pw_cmd),
        )?;
        let parent_subpath = if !is_drive && !account.remote_root_folder_id.is_empty() {
            account.remote_root_folder_id.as_str()
        } else {
            ""
        };
        let parent_directories =
            match list_dirs_with_runner(&ls_req, parent_subpath, self.process_runner.as_ref()) {
                Ok(dirs) => dirs,
                Err(err) => {
                    let acct_err = AccountError::from(err);
                    if matches!(acct_err, AccountError::AuthenticationRequired { .. }) {
                        account.auth_status = "authentication_required".to_string();
                        account.updated_at = self.now_rfc3339();
                        store.upsert_account(&account)?;
                    }
                    return Err(acct_err);
                }
            };

        let duplicate_directories = find_duplicate_dir_names(&parent_directories);
        account.auth_status = "ready".to_string();
        account.updated_at = self.now_rfc3339();
        store.upsert_account(&account)?;

        Ok(AccountCheckReport {
            account,
            encrypted_config_verified: true,
            parent_directories,
            duplicate_directories,
        })
    }

    /// Resolves every component of `relative_path` (e.g. `Data/Research`) under
    /// the account's remote via `rclone lsjson --dirs-only` while holding the
    /// per-account config lock, rejecting ambiguous duplicate folder names (§7).
    pub fn resolve_remote_path(
        &self,
        id: &AccountId,
        relative_path: &str,
    ) -> Result<ResolvedRemotePath, AccountError> {
        validate_safe_path_id("account_id", id.as_str())?;
        let _state_lock = self.maybe_lock_state()?;
        let _acct_lock = self.acquire_account_lock(id)?;
        let store = self.open_store()?;
        let account = self.load_account_for_op(&store, id)?;
        let config_path =
            reject_symlinks_under_root("account_config", &self.state_dir, &account.config_path)?;
        check_config_recovery_state(&config_path)?;
        let cred_key = self.credential_key_for(&store, &account.id)?;

        // Fast fail with AuthenticationRequired if credential store is locked.
        self.secret_store.get_secret(&cred_key)?;

        let engine = self.qualify_engine()?;
        let pw_cmd = self.password_command_for_key(&cred_key)?;

        let is_drive = account.kind == "drive";
        let ls_req = LsJsonRequest::new(
            engine,
            config_path,
            account.remote_name.clone(),
            is_drive,
            Some(pw_cmd),
        )?;

        if !is_drive && !account.remote_root_folder_id.is_empty() {
            let segments: Vec<&str> = relative_path.split('/').filter(|s| !s.is_empty()).collect();
            if segments.is_empty() {
                return Err(AccountError::InvalidInput {
                    cause: "Remote path must not be empty.".to_string(),
                    next_step: "Provide a relative folder path such as Data/Research.".to_string(),
                });
            }
            let mut cur_dir = account
                .remote_root_folder_id
                .trim_end_matches('/')
                .to_string();
            let mut rel_prefix = String::new();
            let mut components = Vec::with_capacity(segments.len());
            for seg in segments {
                let entries =
                    list_dirs_with_runner(&ls_req, &cur_dir, self.process_runner.as_ref())?;
                let matches: Vec<&LsJsonEntry> = entries
                    .iter()
                    .filter(|e| e.is_dir && e.name == seg)
                    .collect();
                let next_rel = if rel_prefix.is_empty() {
                    seg.to_string()
                } else {
                    format!("{rel_prefix}/{seg}")
                };
                match matches.as_slice() {
                    [] => {
                        return Err(AccountError::FolderNotFound {
                            path: next_rel,
                            component: seg.to_string(),
                        });
                    }
                    [single] => {
                        components.push(rclone_adapter::ResolvedComponent {
                            name: seg.to_string(),
                            path: next_rel.clone(),
                            folder_id: single.id.clone().filter(|s| !s.is_empty()),
                        });
                    }
                    many => {
                        let ids: Vec<Option<String>> = many.iter().map(|e| e.id.clone()).collect();
                        return Err(AccountError::AmbiguousDuplicate {
                            path: next_rel,
                            component: seg.to_string(),
                            matching_ids: ids,
                        });
                    }
                }
                cur_dir = format!("{cur_dir}/{seg}");
                rel_prefix = next_rel;
            }
            return Ok(ResolvedRemotePath {
                remote: account.remote_name,
                display_path: relative_path.to_string(),
                components,
            });
        }

        Ok(resolve_folder_path_with_runner(
            &ls_req,
            relative_path,
            is_drive,
            self.process_runner.as_ref(),
        )?)
    }

    pub(crate) fn locked_drive<'a>(
        &'a self,
        id: &AccountId,
        control: &'a ProcessControl,
    ) -> Result<LockedDrive<'a>, AccountError> {
        let store = self.open_store()?;
        self.load_account_for_op(&store, id)?;
        let lock = self.try_account_lock(id)?;
        let account = self.load_account_for_op(&store, id)?;
        if account.kind != "drive" || account.remote_root_folder_id.is_empty() {
            return Err(AccountError::InvalidInput {
                cause: "Choose a Drive account with a shared parent folder.".into(),
                next_step: "Review the shared parent configured in Accounts.".into(),
            });
        }
        if account.auth_status != "ready" {
            return Err(AccountError::AuthenticationRequired {
                cause: "Sign in to this Drive account before using its folders.".into(),
                next_step: "Reconnect the Drive account in Accounts.".into(),
            });
        }
        let config_path = self.account_config_path(id)?;
        if config_path != account.config_path {
            return Err(AccountError::InvalidInput {
                cause: "The saved Drive configuration is outside the account's expected path."
                    .into(),
                next_step: "Review the account configuration before using Drive folders.".into(),
            });
        }
        check_config_recovery_state(&config_path)?;
        let cred_key = self.credential_key_for(&store, id)?;
        self.secret_store.get_secret(&cred_key)?;
        let request = LsJsonRequest::new(
            self.qualify_engine()?,
            config_path,
            account.remote_name.clone(),
            true,
            Some(self.password_command_for_key(&cred_key)?),
        )?;
        Ok(LockedDrive {
            _lock: lock,
            account,
            request,
            runner: ControlledProcessRunner {
                runner: self.process_runner.as_ref(),
                control,
            },
        })
    }

    pub fn list_collection_candidates(
        &self,
        id: &AccountId,
    ) -> Result<CollectionListing, AccountError> {
        let _state_lock = self.maybe_lock_state()?;
        let control = ProcessControl::with_timeout(DRIVE_COMMAND_TIMEOUT);
        let drive = self.locked_drive(id, &control)?;
        drive.candidates(&drive.data_id()?)
    }

    pub fn create_collection_folder(
        &self,
        id: &AccountId,
        name: &str,
    ) -> Result<CollectionCandidate, AccountError> {
        validate_collection_name(name)?;
        let _state_lock = self.maybe_lock_state()?;
        let control = ProcessControl::with_timeout(DRIVE_COMMAND_TIMEOUT);
        let drive = self.locked_drive(id, &control)?;
        let data_id = drive.data_id()?;
        let siblings = drive.request.list_entries("Data", false, &drive.runner)?;
        if siblings.iter().any(|entry| {
            sync_core::validate_names(&[entry.name.as_str(), name])
                .iter()
                .any(|finding| {
                    matches!(
                        finding.kind,
                        sync_core::NameFindingKind::ExactDuplicateName { .. }
                            | sync_core::NameFindingKind::CaseOnlyCollision { .. }
                            | sync_core::NameFindingKind::UnicodeNormalizationCollision { .. }
                    )
                })
        }) {
            return Err(AccountError::InvalidInput {
                cause: format!("The name `{name}` is already used in Data."),
                next_step: "Choose a different folder name.".into(),
            });
        }
        drive
            .request
            .mkdir(&format!("Data/{name}"), &drive.runner)?;
        let actual_data_id = drive.data_id()?;
        if actual_data_id != data_id {
            return Err(AccountError::FolderIdMismatch {
                path: "Data".into(),
                expected_id: data_id,
                actual_id: actual_data_id,
            });
        }
        let entries = list_dirs_with_runner(&drive.request, "Data", &drive.runner)?;
        let mut matches = entries
            .into_iter()
            .filter(|entry| entry.is_dir && entry.name == name)
            .peekable();
        let path = format!("Data/{name}");
        let entry = matches.next().ok_or_else(|| AccountError::FolderNotFound {
            path: path.clone(),
            component: name.to_owned(),
        })?;
        if matches.peek().is_some() {
            let matching_ids = std::iter::once(entry.id)
                .chain(matches.map(|entry| entry.id))
                .collect();
            return Err(AccountError::AmbiguousDuplicate {
                path,
                component: name.to_owned(),
                matching_ids,
            });
        }
        drive.candidate(&data_id, entry)
    }

    pub(crate) fn measure_collection(
        &self,
        id: &AccountId,
        folder_id: &str,
        control: &ProcessControl,
    ) -> Result<rclone_adapter::RemoteSize, AccountError> {
        let drive = self.locked_drive(id, control)?;
        let data_id = drive.data_id()?;
        let mut candidates = drive
            .candidates(&data_id)?
            .candidates
            .into_iter()
            .filter(|candidate| candidate.collection.folder_id == folder_id);
        let selection = candidates
            .next()
            .filter(|_| candidates.next().is_none())
            .ok_or_else(|| AccountError::InvalidInput {
                cause: "The selected folder could not be identified in Data.".into(),
                next_step: "Refresh the Drive folder list and select the folder again.".into(),
            })?;
        let size = drive
            .request
            .size(&selection.collection.display_path, &drive.runner)?;
        // Size is path-addressed, so discard a result if the identity changed during the scan.
        drive.verify_collection(&selection.collection)?;
        Ok(size)
    }

    /// Pre-run cloud folder identity check (§7): acquires the per-account config
    /// lock, resolves `collection.display_path` via `lsjson --dirs-only`,
    /// rejects duplicates, and confirms that both `collection.folder_id` and
    /// `collection.ancestor_ids` still match.
    pub fn verify_collection_identity(
        &self,
        id: &AccountId,
        collection: &CloudCollection,
    ) -> Result<ResolvedRemotePath, AccountError> {
        validate_safe_path_id("account_id", id.as_str())?;
        let _state_lock = self.maybe_lock_state()?;
        let _acct_lock = self.acquire_account_lock(id)?;
        let store = self.open_store()?;
        let account = self.load_account_for_op(&store, id)?;
        let config_path =
            reject_symlinks_under_root("account_config", &self.state_dir, &account.config_path)?;
        check_config_recovery_state(&config_path)?;
        let cred_key = self.credential_key_for(&store, &account.id)?;
        self.secret_store.get_secret(&cred_key)?;

        let engine = self.qualify_engine()?;
        let pw_cmd = self.password_command_for_key(&cred_key)?;

        let ls_req = LsJsonRequest::new(
            engine,
            config_path,
            account.remote_name.clone(),
            account.kind == "drive",
            Some(pw_cmd),
        )?;
        let root_opt = (!account.remote_root_folder_id.is_empty())
            .then_some(account.remote_root_folder_id.as_str());
        let ancestors_opt =
            (!collection.ancestor_ids.is_empty()).then_some(collection.ancestor_ids.as_slice());

        Ok(verify_expected_folder_identity_with_runner(
            &ls_req,
            &collection.display_path,
            &collection.folder_id,
            root_opt,
            ancestors_opt,
            self.process_runner.as_ref(),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use rclone_adapter::{
        FakeProcessRunner, FakeRunScript, OutputLine, StreamKind, pinned_binary_path,
    };
    use tempfile::TempDir;

    use crate::secrets::InMemorySecretStore;

    #[cfg(unix)]
    fn make_helper_factory(tmp: &Path, store: Arc<InMemorySecretStore>) -> PasswordCommandFactory {
        let helper_dir = tmp.join("test-helper");
        fs::create_dir_all(&helper_dir).unwrap();
        Arc::new(move |cred_key: &str| {
            let secret = store.get_secret(cred_key).map_err(AccountError::from)?;
            let safe_name = cred_key.replace('/', "__");
            let secret_file = helper_dir.join(format!("{safe_name}.secret"));
            fs::write(&secret_file, secret.expose().as_bytes()).unwrap();
            fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o600)).unwrap();

            let script_path = helper_dir.join(format!("cred-helper-{safe_name}"));
            let script = format!("#!/bin/sh\ncat \"{}\"\n", secret_file.display());
            fs::write(&script_path, script.as_bytes()).unwrap();
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();

            Ok(PasswordCommand::new(
                script_path,
                vec!["--account".to_string(), cred_key.to_string()],
            )
            .unwrap())
        })
    }

    #[test]
    fn account_service_caches_successful_qualification_for_its_lifetime() {
        let Some(pinned) = pinned_binary_path() else {
            eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
            return;
        };
        let tmp = TempDir::new().unwrap();
        let engine_path = tmp.path().join("rclone");
        let mut cfg = AccountServiceConfig::with_state_dir(tmp.path().join("state"));
        cfg.secret_store = Arc::new(InMemorySecretStore::new());
        cfg.rclone_binary_override = Some(engine_path.clone());
        let svc = AccountService::new(cfg).unwrap();

        assert!(matches!(
            svc.qualify_engine(),
            Err(AccountError::Engine { .. })
        ));
        fs::copy(pinned, &engine_path).unwrap();
        let original = svc.qualify_engine().unwrap();

        fs::write(&engine_path, b"replacement after qualification").unwrap();
        assert_eq!(svc.qualify_engine().unwrap(), original);
    }

    #[test]
    fn duplicate_error_renders_matching_folder_ids() {
        let err = AccountError::AmbiguousDuplicate {
            path: "Data/Research".to_string(),
            component: "Research".to_string(),
            matching_ids: vec![Some("1ResA".to_string()), Some("1ResB".to_string())],
        };
        let rendered = err.cause();
        assert!(rendered.contains("1ResA"), "got: {rendered}");
        assert!(rendered.contains("1ResB"), "got: {rendered}");
    }

    #[test]
    fn import_account_check_and_resolve_end_to_end_with_fake_store_and_pinned_rclone() {
        if pinned_binary_path().is_none() {
            eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        let tree = tmp.path().join("local-parent");
        fs::create_dir_all(tree.join("Data/Research")).unwrap();
        fs::create_dir_all(tree.join("Data/Family")).unwrap();

        let source_cfg = tmp.path().join("external/source.conf");
        fs::create_dir_all(source_cfg.parent().unwrap()).unwrap();
        let source_bytes =
            b"[local_demo]\ntype = local\n\n[ignored]\ntype = drive\ntoken = secret\n";
        fs::write(&source_cfg, source_bytes).unwrap();

        let secret_store = Arc::new(InMemorySecretStore::new());
        let mut cfg = AccountServiceConfig::with_state_dir(&state_dir);
        cfg.secret_store = Arc::clone(&secret_store) as Arc<dyn SecretStore>;
        cfg.password_command_factory =
            Some(make_helper_factory(tmp.path(), Arc::clone(&secret_store)));
        let svc = AccountService::new(cfg).unwrap();

        let imported = svc
            .import_account(ImportAccountRequest {
                id: AccountId::new("local-1"),
                display_name: "Local Fixture".to_string(),
                source_config_path: source_cfg.clone(),
                source_remote_name: "local_demo".to_string(),
                target_remote_name: "household".to_string(),
                root_folder_id: Some(tree.to_string_lossy().into_owned()),
                allow_global_default_source: false,
            })
            .unwrap();

        assert_eq!(imported.id.as_str(), "local-1");
        assert_eq!(imported.kind, "local");
        assert_eq!(imported.remote_name, "household");
        assert_eq!(secret_store.len(), 1);
        assert_eq!(fs::read(&source_cfg).unwrap(), source_bytes);

        let listed = svc.list_accounts().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.as_str(), "local-1");

        let report = svc.check_account(&AccountId::new("local-1")).unwrap();
        assert!(report.encrypted_config_verified);
        assert_eq!(report.parent_directories.len(), 1);
        assert_eq!(report.parent_directories[0].name, "Data");

        let resolved = svc
            .resolve_remote_path(&AccountId::new("local-1"), "Data/Research")
            .unwrap();
        assert_eq!(resolved.components.len(), 2);
        assert_eq!(resolved.components[0].name, "Data");
        assert_eq!(resolved.components[1].name, "Research");

        // Locking the fake secret store causes check_account to return AuthenticationRequired.
        secret_store.set_locked(true);
        let err = svc.check_account(&AccountId::new("local-1")).unwrap_err();
        assert!(matches!(err, AccountError::AuthenticationRequired { .. }));
        let updated = svc
            .get_account(&AccountId::new("local-1"))
            .unwrap()
            .unwrap();
        assert_eq!(updated.auth_status, "authentication_required");
    }

    #[test]
    fn verify_collection_identity_uses_drive_lsjson_fixtures_and_rejects_mismatch() {
        let Some(rclone_path) = pinned_binary_path() else {
            eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
            return;
        };
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");

        let acct_dir = state_dir.join("accounts/drive-1");
        fs::create_dir_all(&acct_dir).unwrap();
        let cfg_path = acct_dir.join("rclone.conf");
        fs::write(
            &cfg_path,
            "# Encrypted rclone configuration File\n\nRCLONE_ENCRYPT_V0:\nFakeCiphertext==\n",
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&cfg_path, fs::Permissions::from_mode(0o600)).unwrap();

        let store = SqliteStateStore::open(&state_dir.join("state.sqlite")).unwrap();
        let dev = store
            .ensure_device(&DeviceRecord {
                id: DeviceId::new("device-test-1"),
                name: "test".to_string(),
                created_at: "2026-11-20T00:00:00Z".to_string(),
            })
            .unwrap();
        store
            .upsert_account(&AccountRecord {
                id: AccountId::new("drive-1"),
                display_name: "Household Drive".to_string(),
                kind: "drive".to_string(),
                remote_name: "household".to_string(),
                remote_root_folder_id: "0AParentRoot".to_string(),
                config_path: cfg_path,
                auth_status: "ready".to_string(),
                created_at: "2026-11-20T00:00:00Z".to_string(),
                updated_at: "2026-11-20T00:00:00Z".to_string(),
            })
            .unwrap();
        drop(store);

        let secret_store = Arc::new(InMemorySecretStore::new());
        let cred_key = scoped_credential_key(dev.id.as_str(), "drive-1").unwrap();
        secret_store
            .create_secret(&cred_key, &SecretString::new("passphrase-123"))
            .unwrap();

        let runner = Arc::new(FakeProcessRunner::new());
        // Pass 1: matching IDs -> Ok
        runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: r#"[{"Path":"Data","Name":"Data","Size":-1,"MimeType":"inode/directory","ModTime":"2026-11-20T00:00:00Z","IsDir":true,"ID":"1DataId"}]"#.to_string(),
                truncated: false,
            }],
            ..Default::default()
        });
        runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: r#"[{"Path":"Research","Name":"Research","Size":-1,"MimeType":"inode/directory","ModTime":"2026-11-20T00:00:00Z","IsDir":true,"ID":"1ResearchId"}]"#.to_string(),
                truncated: false,
            }],
            ..Default::default()
        });
        // Pass 2: replaced folder ID -> FolderIdMismatch
        runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: r#"[{"Path":"Data","Name":"Data","Size":-1,"MimeType":"inode/directory","ModTime":"2026-11-20T00:00:00Z","IsDir":true,"ID":"1DataId"}]"#.to_string(),
                truncated: false,
            }],
            ..Default::default()
        });
        runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: r#"[{"Path":"Research","Name":"Research","Size":-1,"MimeType":"inode/directory","ModTime":"2026-11-20T00:00:00Z","IsDir":true,"ID":"1ReplacedId"}]"#.to_string(),
                truncated: false,
            }],
            ..Default::default()
        });

        let mut cfg = AccountServiceConfig::with_state_dir(&state_dir);
        cfg.secret_store = Arc::clone(&secret_store) as Arc<dyn SecretStore>;
        cfg.process_runner = Arc::clone(&runner) as Arc<dyn ProcessRunner>;
        cfg.rclone_binary_override = Some(rclone_path);
        cfg.password_command_factory = Some(Arc::new(|key: &str| {
            Ok(PasswordCommand::new(
                "/usr/local/bin/foldersync-cred-helper",
                vec!["--account".to_string(), key.to_string()],
            )
            .unwrap())
        }));

        let svc = AccountService::new(cfg).unwrap();
        let collection = CloudCollection {
            display_path: "Data/Research".to_string(),
            folder_id: "1ResearchId".to_string(),
            ancestor_ids: vec!["0AParentRoot".to_string(), "1DataId".to_string()],
        };

        let ok = svc
            .verify_collection_identity(&AccountId::new("drive-1"), &collection)
            .unwrap();
        assert_eq!(ok.leaf_folder_id(), Some("1ResearchId"));

        let err = svc
            .verify_collection_identity(&AccountId::new("drive-1"), &collection)
            .unwrap_err();
        assert!(matches!(
            err,
            AccountError::FolderIdMismatch {
                ref expected_id,
                ref actual_id,
                ..
            } if expected_id == "1ResearchId" && actual_id == "1ReplacedId"
        ));
    }
}
