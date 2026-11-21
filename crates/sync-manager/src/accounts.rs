//! High-level account management service coordinating app-owned encrypted
//! `accounts/<id>/rclone.conf` files, OS secret storage, per-account config
//! mutation locks, Drive OAuth flows, explicit single-remote import, and
//! `lsjson --dirs-only` folder-identity resolution (§7, §8, §19).
//!
//! Designed as a small, self-contained API that both `sync-cli` and the
//! `SyncManager` / UI wiring layer (`connect-t4`) can call cleanly.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use rclone_adapter::{
    AccountConfigLock, AuthUrlCallback, ConfigError, DriveAuthError, DriveAuthRequest,
    DriveRemoteConfigSpec, DuplicateDirFinding, EngineIdentity, ImportRemoteSpec, LsJsonEntry,
    LsJsonError, LsJsonRequest, PasswordCommand, ProcessRunner, RealProcessRunner,
    ResolvedRemotePath, create_drive_remote_config, find_duplicate_dir_names,
    import_single_remote_into_encrypted_config, list_dirs_with_runner, pinned_binary_path,
    qualify_binary, resolve_folder_path_with_runner, run_drive_auth, verify_encrypted_config,
    verify_expected_folder_identity_with_runner,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sync_core::{AccountId, CloudCollection};
use thiserror::Error;

use crate::{
    clock::{Clock, SystemClock},
    error::ManagerError,
    platform::{
        StateLock, ensure_private_dir_under, reject_symlinks_under_root, resolve_state_dir,
        validate_safe_path_id,
    },
    secrets::{SecretStore, SecretStoreError, SecretString, SystemSecretStore},
    store::{AccountRecord, SqliteStateStore},
};

/// Factory producing the non-secret `--password-command` argv for a given account.
pub type PasswordCommandFactory =
    Arc<dyn Fn(&AccountId) -> Result<PasswordCommand, AccountError> + Send + Sync>;

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
        "Ambiguous duplicate cloud folder `{component}` at `{path}` ({} matching folders).",
        matching_ids.len()
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
                "Choose a different account ID or reconnect the existing account.".to_string()
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
            SecretStoreError::NotFound { account_id } => Self::AuthenticationRequired {
                cause: format!(
                    "No configuration passphrase was found in the credential store for account `{account_id}`."
                ),
                next_step: "Reconnect the account to restore its credential store entry."
                    .to_string(),
            },
            SecretStoreError::InvalidAccountId(id) => Self::InvalidInput {
                cause: format!("Invalid account identifier `{id}`."),
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
                next_step: "Unlock your macOS Keychain and complete Google Drive authorization in your browser."
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

/// Result returned by [`AccountService::add_drive_account`].
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
    /// Optional shared parent folder ID (or local parent directory for local-backend fixtures).
    pub root_folder_id: Option<String>,
    /// Must be `true` to allow reading the user's global default rclone config.
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

/// Account management service for encrypted rclone configs, credentials, Drive
/// OAuth flows, explicit single-remote import, and folder-identity resolution.
pub struct AccountService {
    state_dir: PathBuf,
    secret_store: Arc<dyn SecretStore>,
    process_runner: Arc<dyn ProcessRunner>,
    clock: Arc<dyn Clock>,
    rclone_binary_override: Option<PathBuf>,
    credential_helper_binary: Option<PathBuf>,
    password_command_factory: Option<PasswordCommandFactory>,
    acquire_state_lock: bool,
}

impl AccountService {
    /// Opens an [`AccountService`] bound to `config.state_dir`.
    pub fn new(config: AccountServiceConfig) -> Result<Self, AccountError> {
        let state_dir = resolve_state_dir(Some(&config.state_dir))?;
        Ok(Self {
            state_dir,
            secret_store: config.secret_store,
            process_runner: config.process_runner,
            clock: config.clock,
            rclone_binary_override: config.rclone_binary_override,
            credential_helper_binary: config.credential_helper_binary,
            password_command_factory: config.password_command_factory,
            acquire_state_lock: config.acquire_state_lock,
        })
    }

    /// Convenience constructor for callers (such as `SyncManager`) that already
    /// hold the exclusive `StateLock` on `state_dir`.
    pub fn for_locked_manager(mut config: AccountServiceConfig) -> Result<Self, AccountError> {
        config.acquire_state_lock = false;
        Self::new(config)
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
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

    fn now_rfc3339(&self) -> String {
        self.clock.now_rfc3339()
    }

    fn qualify_engine(&self) -> Result<EngineIdentity, AccountError> {
        let path = match &self.rclone_binary_override {
            Some(p) => p.clone(),
            None => pinned_binary_path().ok_or_else(|| AccountError::Engine {
                cause: "Pinned rclone binary is not installed.".to_string(),
                next_step:
                    "Run scripts/fetch-rclone.sh (or set FOLDERSYNC_RCLONE to the verified binary)."
                        .to_string(),
            })?,
        };
        qualify_binary(&path).map_err(|err| AccountError::Engine {
            cause: err.to_string(),
            next_step:
                "Verify the bundled rclone executable against packaging/qualification-manifest.toml."
                    .to_string(),
        })
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

    /// Builds the validated non-secret `--password-command` (`sync-cli credential-helper --account <id>`).
    pub fn password_command_for(&self, id: &AccountId) -> Result<PasswordCommand, AccountError> {
        validate_safe_path_id("account_id", id.as_str())?;
        if let Some(factory) = &self.password_command_factory {
            return factory(id);
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
                id.as_str().to_string(),
            ],
        )
        .map_err(|err| AccountError::InvalidInput {
            cause: err.to_string(),
            next_step:
                "Configure a valid absolute path to the FolderSync credential helper binary."
                    .to_string(),
        })
    }

    /// Creates a new app-owned encrypted Google Drive account config (`0600`)
    /// rooted at `req.root_folder_id` with the user's own OAuth client credentials,
    /// stores the config passphrase in [`SecretStore`], persists the account row
    /// in SQLite, and optionally runs the pinned rclone browser OAuth flow.
    pub fn add_drive_account(
        &self,
        req: AddDriveAccountRequest,
        on_auth_url: Option<AuthUrlCallback>,
    ) -> Result<DriveAccountResult, AccountError> {
        validate_safe_path_id("account_id", req.id.as_str())?;
        let _state_lock = self.maybe_lock_state()?;
        let store = self.open_store()?;

        if store.get_account(&req.id)?.is_some() {
            return Err(AccountError::AlreadyExists {
                account_id: req.id.clone(),
            });
        }

        let account_dir = self.account_dir(&req.id)?;
        let config_path = self.account_config_path(&req.id)?;
        let lock_path = self.account_lock_path(&req.id)?;

        // Validate inputs BEFORE creating directories or touching SecretStore.
        if req.client_id.trim().is_empty()
            || req.client_secret.is_empty()
            || req.root_folder_id.trim().is_empty()
        {
            return Err(AccountError::InvalidInput {
                cause: "Google Drive account requires non-empty client_id, client_secret, and root_folder_id.".to_string(),
                next_step: "Provide your OAuth client ID, client secret, and shared parent folder ID.".to_string(),
            });
        }

        let engine = self.qualify_engine()?;
        ensure_private_dir_under("account_dir", &self.state_dir, &account_dir)?;

        // Store a fresh 256-bit passphrase in SecretStore BEFORE invoking rclone.
        let passphrase = SecretString::generate_random_passphrase()?;
        self.secret_store.set_secret(req.id.as_str(), &passphrase)?;
        let pw_cmd = match self.password_command_for(&req.id) {
            Ok(cmd) => cmd,
            Err(err) => {
                let _ = self.secret_store.delete_secret(req.id.as_str());
                return Err(err);
            }
        };

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
        let drive_spec = DriveRemoteConfigSpec {
            remote_name: remote_name.clone(),
            client_id: req.client_id.clone(),
            client_secret: req.client_secret.expose().to_string(),
            scope,
            root_folder_id: req.root_folder_id.trim().to_string(),
        };

        if let Err(err) = create_drive_remote_config(
            &engine,
            self.process_runner.as_ref(),
            &config_path,
            &lock_path,
            &pw_cmd,
            &drive_spec,
        ) {
            let _ = self.secret_store.delete_secret(req.id.as_str());
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
            remote_root_folder_id: req.root_folder_id.trim().to_string(),
            config_path: config_path.clone(),
            auth_status: "pending".to_string(),
            created_at: now.clone(),
            updated_at: now,
        };
        store.upsert_account(&record)?;

        let mut auth_url = None;
        if req.run_oauth_flow {
            let auth_req =
                DriveAuthRequest::new(engine, config_path, lock_path, remote_name, pw_cmd)?;
            match run_drive_auth(&auth_req, self.process_runner.as_ref(), on_auth_url) {
                Ok(outcome) => {
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

    /// Explicitly imports ONE named remote from a user-chosen rclone config into
    /// a newly encrypted app-owned `accounts/<id>/rclone.conf` without mutating
    /// the source file (§8).
    pub fn import_account(&self, req: ImportAccountRequest) -> Result<AccountRecord, AccountError> {
        validate_safe_path_id("account_id", req.id.as_str())?;
        let _state_lock = self.maybe_lock_state()?;
        let store = self.open_store()?;

        if store.get_account(&req.id)?.is_some() {
            return Err(AccountError::AlreadyExists {
                account_id: req.id.clone(),
            });
        }

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

        // Refuse user's global default rclone config BEFORE touching directories, rclone, or SecretStore.
        if !req.allow_global_default_source
            && rclone_adapter::is_global_default_rclone_config(&source_abs, None, None)
        {
            return Err(AccountError::GlobalDefaultConfigRefused { path: source_abs });
        }

        let engine = self.qualify_engine()?;
        let account_dir = self.account_dir(&req.id)?;
        ensure_private_dir_under("account_dir", &self.state_dir, &account_dir)?;
        let target_config_path = self.account_config_path(&req.id)?;
        let lock_path = self.account_lock_path(&req.id)?;

        let passphrase = SecretString::generate_random_passphrase()?;
        self.secret_store.set_secret(req.id.as_str(), &passphrase)?;
        let pw_cmd = match self.password_command_for(&req.id) {
            Ok(cmd) => cmd,
            Err(err) => {
                let _ = self.secret_store.delete_secret(req.id.as_str());
                return Err(err);
            }
        };

        let target_remote = if req.target_remote_name.trim().is_empty() {
            req.source_remote_name.trim().to_string()
        } else {
            req.target_remote_name.trim().to_string()
        };

        let imported = match import_single_remote_into_encrypted_config(
            &engine,
            self.process_runner.as_ref(),
            &ImportRemoteSpec {
                source_config_path: source_abs,
                source_remote_name: req.source_remote_name.trim().to_string(),
                target_remote_name: target_remote,
                target_config_path: target_config_path.clone(),
                lock_path,
                password_command: pw_cmd,
                allow_global_default_source: req.allow_global_default_source,
            },
        ) {
            Ok(info) => info,
            Err(err) => {
                let _ = self.secret_store.delete_secret(req.id.as_str());
                return Err(err.into());
            }
        };

        let root_folder_id = req
            .root_folder_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .or(imported.root_folder_id)
            .unwrap_or_default();

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
            config_path: target_config_path,
            auth_status: "ready".to_string(),
            created_at: now.clone(),
            updated_at: now,
        };
        store.upsert_account(&record)?;
        Ok(record)
    }

    /// Lists all persisted accounts ordered by creation time and ID.
    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>, AccountError> {
        let _state_lock = self.maybe_lock_state()?;
        // Ensure migrations and DB permissions are initialized first.
        let store = self.open_store()?;
        let db_path = store.path().to_path_buf();
        drop(store);

        let conn = Connection::open(&db_path).map_err(|err| ManagerError::Storage {
            cause: format!("Failed to open SQLite DB `{}`: {err}.", db_path.display()),
            next_step: "Verify the FolderSync state directory is readable.".to_string(),
        })?;
        let mut stmt = conn
            .prepare(
                "SELECT id, display_name, kind, remote_name, remote_root_folder_id,
                        config_path, auth_status, created_at, updated_at
                 FROM accounts ORDER BY created_at ASC, id ASC",
            )
            .map_err(|err| ManagerError::Storage {
                cause: format!("Failed to prepare account listing query: {err}."),
                next_step: "Verify state.sqlite schema integrity.".to_string(),
            })?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AccountRecord {
                    id: AccountId::new(row.get::<_, String>(0)?),
                    display_name: row.get(1)?,
                    kind: row.get(2)?,
                    remote_name: row.get(3)?,
                    remote_root_folder_id: row.get(4)?,
                    config_path: PathBuf::from(row.get::<_, String>(5)?),
                    auth_status: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            })
            .map_err(|err| ManagerError::Storage {
                cause: format!("Failed to query accounts: {err}."),
                next_step: "Verify state.sqlite integrity.".to_string(),
            })?;
        let accounts =
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|err| ManagerError::Storage {
                    cause: format!("Failed to collect account rows: {err}."),
                    next_step: "Verify state.sqlite integrity.".to_string(),
                })?;
        Ok(accounts)
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
    /// directories of its shared parent folder via `rclone lsjson --dirs-only`.
    pub fn check_account(&self, id: &AccountId) -> Result<AccountCheckReport, AccountError> {
        let _state_lock = self.maybe_lock_state()?;
        let store = self.open_store()?;
        let mut account = self.load_account_for_op(&store, id)?;

        let config_path =
            reject_symlinks_under_root("account_config", &self.state_dir, &account.config_path)?;

        // Verify credential store is unlocked BEFORE building password-command
        // or qualifying rclone so a locked Keychain immediately records
        // `authentication_required` and fails without any hidden prompt.
        if let Err(err) = self.secret_store.get_secret(account.id.as_str()) {
            let acct_err = AccountError::from(err);
            if matches!(acct_err, AccountError::AuthenticationRequired { .. }) {
                account.auth_status = "authentication_required".to_string();
                account.updated_at = self.now_rfc3339();
                store.upsert_account(&account)?;
            }
            return Err(acct_err);
        }

        let engine = self.qualify_engine()?;
        let pw_cmd = self.password_command_for(&account.id)?;

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
    /// the account's remote via `rclone lsjson --dirs-only`, rejecting ambiguous
    /// duplicate folder names (§7).
    pub fn resolve_remote_path(
        &self,
        id: &AccountId,
        relative_path: &str,
    ) -> Result<ResolvedRemotePath, AccountError> {
        let _state_lock = self.maybe_lock_state()?;
        let store = self.open_store()?;
        let account = self.load_account_for_op(&store, id)?;
        let config_path =
            reject_symlinks_under_root("account_config", &self.state_dir, &account.config_path)?;

        // Fast fail with AuthenticationRequired if credential store is locked.
        self.secret_store.get_secret(account.id.as_str())?;

        let engine = self.qualify_engine()?;
        let pw_cmd = self.password_command_for(&account.id)?;

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

    /// Pre-run cloud folder identity check (§7): resolves `collection.display_path`
    /// via `lsjson --dirs-only`, rejects duplicates, and confirms that both
    /// `collection.folder_id` and `collection.ancestor_ids` still match.
    pub fn verify_collection_identity(
        &self,
        id: &AccountId,
        collection: &CloudCollection,
    ) -> Result<ResolvedRemotePath, AccountError> {
        let _state_lock = self.maybe_lock_state()?;
        let store = self.open_store()?;
        let account = self.load_account_for_op(&store, id)?;
        let config_path =
            reject_symlinks_under_root("account_config", &self.state_dir, &account.config_path)?;
        let engine = self.qualify_engine()?;
        let pw_cmd = self.password_command_for(&account.id)?;
        self.secret_store.get_secret(account.id.as_str())?;

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
    use std::fs;
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
        Arc::new(move |account_id: &AccountId| {
            let secret = store
                .get_secret(account_id.as_str())
                .map_err(AccountError::from)?;
            let secret_file = helper_dir.join(format!("{}.secret", account_id.as_str()));
            fs::write(&secret_file, secret.expose().as_bytes()).unwrap();
            fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o600)).unwrap();

            let script_path = helper_dir.join(format!("cred-helper-{}", account_id.as_str()));
            let script = format!("#!/bin/sh\ncat \"{}\"\n", secret_file.display());
            fs::write(&script_path, script.as_bytes()).unwrap();
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();

            Ok(PasswordCommand::new(
                script_path,
                vec!["--account".to_string(), account_id.as_str().to_string()],
            )
            .unwrap())
        })
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
        assert!(secret_store.contains("local-1"));
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
        let secret_store = Arc::new(InMemorySecretStore::new());
        secret_store
            .set_secret("drive-1", &SecretString::new("passphrase-123"))
            .unwrap();

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
        cfg.password_command_factory = Some(Arc::new(|id: &AccountId| {
            Ok(PasswordCommand::new(
                "/usr/local/bin/foldersync-cred-helper",
                vec!["--account".to_string(), id.as_str().to_string()],
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
