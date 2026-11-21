//! Atomic writer for the v1 Finder Sync extension `status.json` contract (ADR 0002).

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sync_core::{Execution, Intent, Readiness, RunOutcome};

use crate::{
    error::ManagerError,
    platform::write_private_file_atomic,
    snapshot::{MappingStatusSnapshot, StatusSnapshot},
};

/// Schema version matching `apps/finder-extension/Sources/SnapshotModel.swift`.
pub const FINDER_SNAPSHOT_VERSION: u32 = 1;

/// Default staleness TTL in seconds matching ADR 0002.
pub const DEFAULT_STALE_AFTER_SECONDS: u64 = 300;

/// Mapped-folder root badge state matching ADR 0002.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinderMappingState {
    Synced,
    Syncing,
    Attention,
    Paused,
    DiskMissing,
}

/// Per-mapping entry inside Finder `status.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinderMappingSnapshot {
    pub id: String,
    pub root_path: String,
    pub state: FinderMappingState,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_summary: Option<String>,
}

/// Top-level Finder `status.json` payload (ADR 0002 v1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinderStatusSnapshot {
    pub version: u32,
    pub generated_at: String,
    pub stale_after_seconds: u64,
    pub mappings: Vec<FinderMappingSnapshot>,
}

/// Maps a manager [`MappingStatusSnapshot`] into the v1 Finder badge state enum.
pub fn derive_finder_state(mapping: &MappingStatusSnapshot) -> FinderMappingState {
    if matches!(
        mapping.execution,
        Execution::Running | Execution::Stopping | Execution::Queued
    ) {
        return FinderMappingState::Syncing;
    }

    match mapping.readiness {
        Readiness::VolumeUnavailable => return FinderMappingState::DiskMissing,
        Readiness::RecoveryRequired
        | Readiness::AuthenticationRequired
        | Readiness::EndpointChanged
        | Readiness::Unconfigured
        | Readiness::NeedsInitialization => return FinderMappingState::Attention,
        Readiness::Ready => {}
    }

    if mapping.execution == Execution::Interrupted {
        return FinderMappingState::Attention;
    }

    if mapping.intent == Intent::Paused {
        return FinderMappingState::Paused;
    }

    match mapping.last_run_outcome {
        Some(RunOutcome::SuccessWithConflicts | RunOutcome::RetryableFailure) => {
            FinderMappingState::Attention
        }
        _ => FinderMappingState::Synced,
    }
}

/// Atomically writes `status.json` into a configured handoff directory.
#[derive(Debug, Clone)]
pub struct FinderStatusWriter {
    dir: PathBuf,
}

impl FinderStatusWriter {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn status_file_path(&self) -> PathBuf {
        self.dir.join("status.json")
    }

    pub fn build_payload(&self, snapshot: &StatusSnapshot) -> FinderStatusSnapshot {
        let mappings = snapshot
            .mappings
            .iter()
            .map(|m| {
                let canonical =
                    fs::canonicalize(&m.local_root).unwrap_or_else(|_| m.local_root.clone());
                FinderMappingSnapshot {
                    id: m.id.as_str().to_string(),
                    root_path: canonical.to_string_lossy().into_owned(),
                    state: derive_finder_state(m),
                    label: m.status_text.clone(),
                    last_run_summary: m.last_run_summary.clone(),
                }
            })
            .collect();

        FinderStatusSnapshot {
            version: FINDER_SNAPSHOT_VERSION,
            generated_at: snapshot.generated_at.clone(),
            stale_after_seconds: DEFAULT_STALE_AFTER_SECONDS,
            mappings,
        }
    }

    pub fn write_snapshot(&self, snapshot: &StatusSnapshot) -> Result<PathBuf, ManagerError> {
        let payload = self.build_payload(snapshot);
        let bytes = serde_json::to_vec_pretty(&payload).map_err(|err| ManagerError::Io {
            cause: format!("Failed to serialize Finder status snapshot: {err}."),
            next_step: "Check application logs and retry.".to_string(),
        })?;
        let target = self.status_file_path();
        write_private_file_atomic(&target, &bytes)?;
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::MappingCounters;
    use sync_core::{AccountId, MappingId};

    fn sample_mapping(
        readiness: Readiness,
        execution: Execution,
        intent: Intent,
        last_run_outcome: Option<RunOutcome>,
    ) -> MappingStatusSnapshot {
        MappingStatusSnapshot {
            id: MappingId::new("research"),
            display_name: "Research".to_string(),
            account_id: AccountId::new("local-1"),
            collection_display_path: "Data/Research".to_string(),
            local_root: PathBuf::from("/tmp/research"),
            volume_name: "WorkSSD".to_string(),
            readiness,
            execution,
            intent,
            scheduling_enabled: false,
            active_generation_id: None,
            active_generation_fingerprint: None,
            last_run_id: None,
            last_run_outcome,
            last_run_completed_at: None,
            last_run_summary: None,
            counters: MappingCounters::default(),
            available_actions: Vec::new(),
            status_text: String::new(),
            local_recovery_root: PathBuf::from("/tmp/recovery"),
            cloud_recovery_path: "/tmp/remote_recovery".to_string(),
        }
    }

    #[test]
    fn maps_all_five_finder_states() {
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::Ready,
                Execution::Idle,
                Intent::Enabled,
                Some(RunOutcome::Success)
            )),
            FinderMappingState::Synced
        );
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::Ready,
                Execution::Running,
                Intent::Enabled,
                None
            )),
            FinderMappingState::Syncing
        );
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::RecoveryRequired,
                Execution::Idle,
                Intent::Enabled,
                None
            )),
            FinderMappingState::Attention
        );
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::Ready,
                Execution::Idle,
                Intent::Enabled,
                Some(RunOutcome::SuccessWithConflicts)
            )),
            FinderMappingState::Attention
        );
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::Ready,
                Execution::Idle,
                Intent::Paused,
                Some(RunOutcome::Success)
            )),
            FinderMappingState::Paused
        );
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::VolumeUnavailable,
                Execution::Idle,
                Intent::Enabled,
                None
            )),
            FinderMappingState::DiskMissing
        );
    }
}
