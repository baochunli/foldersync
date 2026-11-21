//! Actionable manager errors carrying both cause and next step.

use std::path::PathBuf;

use rclone_adapter::{LaunchError, QualificationError, RequestError};
use sync_core::{MappingId, ValidationError};
use thiserror::Error;

/// Errors returned by [`crate::SyncManager`] and its storage/locking subsystems.
#[derive(Debug, Error)]
pub enum ManagerError {
    /// Another FolderSync instance already holds the exclusive state lock.
    #[error("{cause} Next step: {next_step}")]
    StateLocked {
        lock_path: PathBuf,
        cause: String,
        next_step: String,
    },
    /// Domain or path validation failed (from `sync-core`).
    #[error("{0}")]
    Validation(#[from] ValidationError),
    /// Multiple validation errors occurred.
    #[error("{}", format_validation_errors(.0))]
    ValidationList(Vec<ValidationError>),
    /// Requested entity was not found.
    #[error("{cause} Next step: {next_step}")]
    NotFound { cause: String, next_step: String },
    /// Command is invalid for the mapping's current state.
    #[error("{cause} Next step: {next_step}")]
    InvalidState {
        mapping_id: MappingId,
        cause: String,
        next_step: String,
    },
    /// Initialization refused because both local and remote trees contain data (§9).
    #[error("{cause} Next step: {next_step}")]
    BothSidesPopulated {
        mapping_id: MappingId,
        cause: String,
        next_step: String,
    },
    /// Access marker on an endpoint is unrecognized or belongs to another collection.
    #[error("{cause} Next step: {next_step}")]
    AccessMarkerConflict {
        path: PathBuf,
        cause: String,
        next_step: String,
    },
    /// Unsupported filesystem entry or filename compatibility findings blocked onboarding.
    #[error("{cause} Next step: {next_step}")]
    PreflightRejected { cause: String, next_step: String },
    /// Another mutating job is currently active globally (§3).
    #[error("{cause} Next step: {next_step}")]
    ActiveJobConflict {
        active_mapping_id: MappingId,
        cause: String,
        next_step: String,
    },
    /// Another operation holds the Drive account's configuration lock.
    #[error("{cause} Next step: {next_step}")]
    AccountBusy { cause: String, next_step: String },
    /// SQLite storage or migration error.
    #[error("{cause} Next step: {next_step}")]
    Storage { cause: String, next_step: String },
    /// Schema version in SQLite is newer than this binary supports.
    #[error("{cause} Next step: {next_step}")]
    UnsupportedSchemaVersion {
        found: i64,
        max_supported: i64,
        cause: String,
        next_step: String,
    },
    /// Pinned rclone engine qualification failed or binary is missing.
    #[error("{cause} Next step: {next_step}")]
    Engine { cause: String, next_step: String },
    /// I/O error on state directory, run artifacts, or endpoints.
    #[error("{cause} Next step: {next_step}")]
    Io { cause: String, next_step: String },
    /// Request validation failed inside `rclone-adapter`.
    #[error(
        "Invalid sync request: {0}. Next step: Review the mapping configuration and recovery directories."
    )]
    Request(#[from] RequestError),
    /// Launch failed inside `rclone-adapter`.
    #[error(
        "Failed to launch rclone: {0}. Next step: Verify the bundled rclone binary integrity and try again."
    )]
    Launch(#[from] LaunchError),
    /// Qualification error inside `rclone-adapter`.
    #[error(
        "Engine qualification failed: {0}. Next step: Run scripts/fetch-rclone.sh or reinstall FolderSync."
    )]
    Qualification(#[from] QualificationError),
}

fn format_validation_errors(errors: &[ValidationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

impl ManagerError {
    /// Plain-language cause suitable for UI/CLI presentation.
    pub fn cause(&self) -> String {
        match self {
            Self::StateLocked { cause, .. }
            | Self::NotFound { cause, .. }
            | Self::InvalidState { cause, .. }
            | Self::BothSidesPopulated { cause, .. }
            | Self::AccessMarkerConflict { cause, .. }
            | Self::PreflightRejected { cause, .. }
            | Self::ActiveJobConflict { cause, .. }
            | Self::AccountBusy { cause, .. }
            | Self::Storage { cause, .. }
            | Self::UnsupportedSchemaVersion { cause, .. }
            | Self::Engine { cause, .. }
            | Self::Io { cause, .. } => cause.clone(),
            Self::Validation(err) => err.cause.clone(),
            Self::ValidationList(errs) => errs
                .first()
                .map(|e| e.cause.clone())
                .unwrap_or_else(|| "Configuration validation failed.".to_string()),
            Self::Request(err) => err.to_string(),
            Self::Launch(err) => err.to_string(),
            Self::Qualification(err) => err.to_string(),
        }
    }

    /// Actionable next step suitable for UI/CLI presentation.
    pub fn next_step(&self) -> String {
        match self {
            Self::StateLocked { next_step, .. }
            | Self::NotFound { next_step, .. }
            | Self::InvalidState { next_step, .. }
            | Self::BothSidesPopulated { next_step, .. }
            | Self::AccessMarkerConflict { next_step, .. }
            | Self::PreflightRejected { next_step, .. }
            | Self::ActiveJobConflict { next_step, .. }
            | Self::AccountBusy { next_step, .. }
            | Self::Storage { next_step, .. }
            | Self::UnsupportedSchemaVersion { next_step, .. }
            | Self::Engine { next_step, .. }
            | Self::Io { next_step, .. } => next_step.clone(),
            Self::Validation(err) => err.next_step.clone(),
            Self::ValidationList(errs) => errs
                .first()
                .map(|e| e.next_step.clone())
                .unwrap_or_else(|| "Fix the configuration issues and try again.".to_string()),
            Self::Request(_) => {
                "Review the mapping paths and recovery directories, then try again.".to_string()
            }
            Self::Launch(_) => {
                "Verify the bundled rclone executable and filesystem permissions, then try again."
                    .to_string()
            }
            Self::Qualification(_) => {
                "Install or verify the pinned rclone executable against packaging/qualification-manifest.toml."
                    .to_string()
            }
        }
    }
}
