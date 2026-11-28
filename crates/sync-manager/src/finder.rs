//! Atomic writer for the v1 Finder Sync extension `status.json` contract,
//! Darwin snapshot notification publisher, and strict v1 Finder action file
//! consumer (ADR 0002).

use std::{
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sync_core::{Execution, Intent, MappingId, Readiness, RunOutcome};

use crate::{
    error::ManagerError,
    platform::{validate_safe_path_id, write_private_file_atomic_under},
    snapshot::{MappingStatusSnapshot, StatusSnapshot},
};
use uuid::Uuid;

/// Schema version matching `apps/finder-extension/Sources/SnapshotModel.swift`.
pub const FINDER_SNAPSHOT_VERSION: u32 = 1;

/// Default staleness TTL in seconds matching ADR 0002.
pub const DEFAULT_STALE_AFTER_SECONDS: u64 = 300;

/// Darwin notification posted after every `status.json` write (ADR 0002).
pub const DARWIN_SNAPSHOT_UPDATED_NOTIFICATION: &str = "dev.foldersync.finder.snapshot-updated";

/// Darwin notification posted by the Finder extension when writing an action file (ADR 0002).
pub const DARWIN_ACTION_REQUESTED_NOTIFICATION: &str = "dev.foldersync.finder.action-requested";

/// Maximum accepted action file size (defends against malformed/oversized input).
const MAX_ACTION_FILE_BYTES: u64 = 16 * 1024;

/// Injectable notification sink for Darwin notifications after `status.json` writes.
pub trait FinderNotifier: Send + Sync + std::fmt::Debug {
    /// Posts `notification_name` (e.g. `dev.foldersync.finder.snapshot-updated`).
    fn post_notification(&self, notification_name: &str);
}

/// No-op [`FinderNotifier`] used by default in tests and CLI runs so tests
/// never post live macOS Darwin notifications.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopFinderNotifier;

impl FinderNotifier for NoopFinderNotifier {
    fn post_notification(&self, _notification_name: &str) {}
}

/// Real macOS Darwin notification publisher using `libSystem`'s `notify_post`.
/// Instantiated only in the real interactive desktop app.
#[derive(Debug, Default, Clone, Copy)]
pub struct DarwinFinderNotifier;

impl FinderNotifier for DarwinFinderNotifier {
    fn post_notification(&self, notification_name: &str) {
        #[cfg(target_os = "macos")]
        {
            use std::ffi::CString;
            unsafe extern "C" {
                fn notify_post(name: *const libc::c_char) -> u32;
            }
            if let Ok(cname) = CString::new(notification_name) {
                let _ = unsafe { notify_post(cname.as_ptr()) };
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = notification_name;
        }
    }
}

/// Recording fake [`FinderNotifier`] for unit/integration tests.
#[derive(Debug, Default)]
pub struct RecordingFinderNotifier {
    posted: Mutex<Vec<String>>,
}

impl RecordingFinderNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn posted_notifications(&self) -> Vec<String> {
        self.posted.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.posted.lock().unwrap().clear();
    }
}

impl FinderNotifier for RecordingFinderNotifier {
    fn post_notification(&self, notification_name: &str) {
        self.posted
            .lock()
            .unwrap()
            .push(notification_name.to_string());
    }
}

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

/// Maps a manager [`MappingStatusSnapshot`] into the v1 Finder badge state enum,
/// accounting for the global scheduling pause hold.
pub fn derive_finder_state_with_pause(
    mapping: &MappingStatusSnapshot,
    scheduling_paused: bool,
) -> FinderMappingState {
    if matches!(
        mapping.execution,
        Execution::Running | Execution::Stopping | Execution::Queued
    ) {
        return FinderMappingState::Syncing;
    }

    if matches!(
        mapping.readiness,
        Readiness::NeedsInitialization | Readiness::RecoveryRequired
    ) && mapping
        .stopped_first_sync
        .as_ref()
        .is_some_and(|stopped| stopped.cause.is_none())
    {
        return FinderMappingState::Paused;
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

    if mapping.unreviewed_conflict_run_id.is_some() {
        return FinderMappingState::Attention;
    }

    if mapping.intent == Intent::Paused {
        return FinderMappingState::Paused;
    }

    match mapping.last_run_outcome {
        Some(RunOutcome::RetryableFailure) => FinderMappingState::Attention,
        _ if scheduling_paused => FinderMappingState::Paused,
        _ => FinderMappingState::Synced,
    }
}

/// Maps a manager [`MappingStatusSnapshot`] into the v1 Finder badge state enum.
pub fn derive_finder_state(mapping: &MappingStatusSnapshot) -> FinderMappingState {
    derive_finder_state_with_pause(mapping, false)
}

/// Atomically writes `status.json` into a configured handoff directory and
/// posts the Darwin notification `dev.foldersync.finder.snapshot-updated`.
#[derive(Debug, Clone)]
pub struct FinderStatusWriter {
    dir: PathBuf,
    notifier: Arc<dyn FinderNotifier>,
}

impl FinderStatusWriter {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_notifier(dir, Arc::new(NoopFinderNotifier))
    }

    pub fn with_notifier(dir: impl Into<PathBuf>, notifier: Arc<dyn FinderNotifier>) -> Self {
        Self {
            dir: dir.into(),
            notifier,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn status_file_path(&self) -> PathBuf {
        self.dir.join("status.json")
    }

    pub fn actions_dir_path(&self) -> PathBuf {
        self.dir.join("actions")
    }

    pub fn build_payload(&self, snapshot: &StatusSnapshot) -> FinderStatusSnapshot {
        let mappings = snapshot
            .mappings
            .iter()
            .filter(|m| m.readiness != Readiness::Unconfigured)
            .map(|m| {
                let state = derive_finder_state_with_pause(m, snapshot.scheduling_paused);
                let label = if snapshot.scheduling_paused
                    && m.readiness == Readiness::Ready
                    && m.execution == Execution::Idle
                    && m.intent == Intent::Enabled
                    && m.unreviewed_conflict_run_id.is_none()
                    && m.last_run_outcome != Some(RunOutcome::RetryableFailure)
                {
                    "Scheduling paused".to_string()
                } else {
                    m.status_text.clone()
                };
                FinderMappingSnapshot {
                    id: m.id.as_str().to_string(),
                    root_path: m.local_root.to_string_lossy().into_owned(),
                    state,
                    label,
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
        // Refuse symlinked handoff directory before writing.
        if let Ok(meta) = fs::symlink_metadata(&self.dir) {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(ManagerError::PreflightRejected {
                    cause: format!(
                        "Refused symbolic link or non-directory Finder handoff path `{}`.",
                        self.dir.display()
                    ),
                    next_step: "Use an ordinary private directory for Finder status handoff."
                        .to_string(),
                });
            }
        } else {
            crate::platform::ensure_private_dir(&self.dir)?;
        }
        let target = self.status_file_path();
        write_private_file_atomic_under("finder_status", &self.dir, &target, &bytes)?;
        self.notifier
            .post_notification(DARWIN_SNAPSHOT_UPDATED_NOTIFICATION);
        Ok(target)
    }
}

/// Strictly validated ADR 0002 v1 Finder action verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinderActionKind {
    SyncNow,
    Pause,
    OpenApp,
    ShowLastRun,
}

impl FinderActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SyncNow => "sync_now",
            Self::Pause => "pause",
            Self::OpenApp => "open_app",
            Self::ShowLastRun => "show_last_run",
        }
    }

    pub fn requires_mapping(self) -> bool {
        matches!(self, Self::SyncNow | Self::Pause | Self::ShowLastRun)
    }
}

/// Strictly validated ADR 0002 v1 Finder action file payload (`actions/action-<uuid>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinderActionRequest {
    pub version: u32,
    pub id: String,
    pub requested_at: String,
    pub action: FinderActionKind,
    #[serde(default)]
    pub mapping_id: Option<String>,
}

/// Validated action ready to be dispatched by the manager / UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedFinderAction {
    SyncNow {
        request_id: String,
        mapping_id: MappingId,
    },
    Pause {
        request_id: String,
        mapping_id: MappingId,
    },
    OpenApp {
        request_id: String,
        mapping_id: Option<MappingId>,
    },
    ShowLastRun {
        request_id: String,
        mapping_id: MappingId,
    },
}

/// Outcome of draining one file from `<finder_dir>/actions/*.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinderActionFileOutcome {
    Accepted {
        file_name: String,
        action: ValidatedFinderAction,
    },
    Rejected {
        file_name: String,
        reason: String,
    },
}

/// Parses and strictly validates one ADR 0002 v1 Finder action JSON payload.
pub fn parse_and_validate_finder_action(bytes: &[u8]) -> Result<ValidatedFinderAction, String> {
    let raw: FinderActionRequest =
        serde_json::from_slice(bytes).map_err(|err| format!("Invalid action JSON: {err}"))?;

    if raw.version != FINDER_SNAPSHOT_VERSION {
        return Err(format!(
            "Unsupported Finder action version {} (expected {}).",
            raw.version, FINDER_SNAPSHOT_VERSION
        ));
    }

    if raw.id.trim().is_empty() || raw.id.len() > 128 {
        return Err("Finder action `id` must be a non-empty string (<= 128 chars).".to_string());
    }

    if DateTime::parse_from_rfc3339(&raw.requested_at).is_err() {
        return Err(format!(
            "Finder action `requested_at` is not a valid RFC 3339 timestamp: `{}`.",
            raw.requested_at
        ));
    }

    let validated_mapping_id = match raw.mapping_id.as_deref().map(str::trim) {
        Some(mid) if !mid.is_empty() => {
            validate_safe_path_id("mapping_id", mid)
                .map_err(|err| format!("Invalid Finder action mapping_id: {}", err.cause()))?;
            Some(MappingId::new(mid))
        }
        _ => None,
    };

    if raw.action.requires_mapping() && validated_mapping_id.is_none() {
        return Err(format!(
            "Finder action `{}` requires a non-empty `mapping_id`.",
            raw.action.as_str()
        ));
    }

    Ok(match raw.action {
        FinderActionKind::SyncNow => ValidatedFinderAction::SyncNow {
            request_id: raw.id,
            mapping_id: validated_mapping_id.expect("checked above"),
        },
        FinderActionKind::Pause => ValidatedFinderAction::Pause {
            request_id: raw.id,
            mapping_id: validated_mapping_id.expect("checked above"),
        },
        FinderActionKind::OpenApp => ValidatedFinderAction::OpenApp {
            request_id: raw.id,
            mapping_id: validated_mapping_id,
        },
        FinderActionKind::ShowLastRun => ValidatedFinderAction::ShowLastRun {
            request_id: raw.id,
            mapping_id: validated_mapping_id.expect("checked above"),
        },
    })
}

/// One claimed Finder action file renamed atomically to a hidden `.processing-*` path
/// prior to dispatch (ADR 0002). Caller deletes `claimed_path` only AFTER dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedFinderAction {
    pub file_name: String,
    pub claimed_path: Option<PathBuf>,
    pub outcome: FinderActionFileOutcome,
}

/// Atomically claims each `*.json` action file in `actions_dir` by renaming it to a
/// hidden `.processing-<uuid>-<name>` path, validates it without following symlinks,
/// and returns it ready for dispatch. If atomic claim (rename) fails, the file is NOT
/// dispatched (`Rejected`, `claimed_path: None`).
pub fn claim_finder_action_files(actions_dir: &Path) -> Vec<ClaimedFinderAction> {
    // Refuse symlinked parent or actions_dir itself.
    if let Some(parent) = actions_dir.parent()
        && fs::symlink_metadata(parent).is_ok_and(|m| m.file_type().is_symlink() || !m.is_dir())
    {
        return Vec::new();
    }
    let dir_meta = match fs::symlink_metadata(actions_dir) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        return Vec::new();
    }

    let mut files: Vec<(String, PathBuf)> = match fs::read_dir(actions_dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || !name.ends_with(".json") {
                    return None;
                }
                Some((name, e.path()))
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut claimed = Vec::with_capacity(files.len());
    for (file_name, path) in files {
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(err) => {
                claimed.push(ClaimedFinderAction {
                    file_name: file_name.clone(),
                    claimed_path: None,
                    outcome: FinderActionFileOutcome::Rejected {
                        file_name,
                        reason: format!("Could not inspect action file: {err}"),
                    },
                });
                continue;
            }
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            let _ = fs::remove_file(&path);
            claimed.push(ClaimedFinderAction {
                file_name: file_name.clone(),
                claimed_path: None,
                outcome: FinderActionFileOutcome::Rejected {
                    file_name,
                    reason:
                        "Refused non-regular file or symbolic link in Finder actions directory."
                            .to_string(),
                },
            });
            continue;
        }

        // Atomically claim via rename to hidden `.processing-<uuid>-<file_name>` BEFORE dispatch.
        // If rename/unlink fails (e.g. read-only directory), do NOT dispatch so it cannot replay!
        let processing_path =
            actions_dir.join(format!(".processing-{}-{file_name}", Uuid::new_v4()));
        if let Err(err) = fs::rename(&path, &processing_path) {
            claimed.push(ClaimedFinderAction {
                file_name: file_name.clone(),
                claimed_path: None,
                outcome: FinderActionFileOutcome::Rejected {
                    file_name,
                    reason: format!("Could not claim Finder action file for processing: {err}"),
                },
            });
            continue;
        }

        let outcome = read_single_action_file(&file_name, &processing_path);
        match &outcome {
            FinderActionFileOutcome::Rejected { .. } => {
                let _ = fs::remove_file(&processing_path);
                claimed.push(ClaimedFinderAction {
                    file_name,
                    claimed_path: None,
                    outcome,
                });
            }
            FinderActionFileOutcome::Accepted { .. } => {
                claimed.push(ClaimedFinderAction {
                    file_name,
                    claimed_path: Some(processing_path),
                    outcome,
                });
            }
        }
    }
    claimed
}

/// Drains and deletes all `*.json` action files in `actions_dir` (refusing symlinks
/// and ignoring hidden/temporary files such as `.action-*.tmp`).
pub fn drain_finder_action_files(actions_dir: &Path) -> Vec<FinderActionFileOutcome> {
    let claimed = claim_finder_action_files(actions_dir);
    claimed
        .into_iter()
        .map(|item| {
            if let Some(p) = item.claimed_path {
                let _ = fs::remove_file(p);
            }
            item.outcome
        })
        .collect()
}

fn read_single_action_file(file_name: &str, path: &Path) -> FinderActionFileOutcome {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) => {
            return FinderActionFileOutcome::Rejected {
                file_name: file_name.to_string(),
                reason: format!("Could not inspect action file: {err}"),
            };
        }
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return FinderActionFileOutcome::Rejected {
            file_name: file_name.to_string(),
            reason: "Refused non-regular file or symbolic link in Finder actions directory."
                .to_string(),
        };
    }
    if meta.len() > MAX_ACTION_FILE_BYTES {
        return FinderActionFileOutcome::Rejected {
            file_name: file_name.to_string(),
            reason: format!(
                "Action file exceeds maximum allowed size ({MAX_ACTION_FILE_BYTES} bytes)."
            ),
        };
    }

    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    opts.custom_flags(libc::O_NOFOLLOW);

    let mut file = match opts.open(path) {
        Ok(f) => f,
        Err(err) => {
            return FinderActionFileOutcome::Rejected {
                file_name: file_name.to_string(),
                reason: format!("Could not open action file safely: {err}"),
            };
        }
    };
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    if let Err(err) = file
        .by_ref()
        .take(MAX_ACTION_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        return FinderActionFileOutcome::Rejected {
            file_name: file_name.to_string(),
            reason: format!("Failed reading action file: {err}"),
        };
    }
    if bytes.len() as u64 > MAX_ACTION_FILE_BYTES {
        return FinderActionFileOutcome::Rejected {
            file_name: file_name.to_string(),
            reason: "Action file grew beyond maximum allowed size.".to_string(),
        };
    }

    match parse_and_validate_finder_action(&bytes) {
        Ok(action) => FinderActionFileOutcome::Accepted {
            file_name: file_name.to_string(),
            action,
        },
        Err(reason) => FinderActionFileOutcome::Rejected {
            file_name: file_name.to_string(),
            reason,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::MappingCounters;
    use sync_core::{AccountId, MappingId, RunId, VolumeKind};

    fn sample_mapping(
        readiness: Readiness,
        execution: Execution,
        intent: Intent,
        last_run_outcome: Option<RunOutcome>,
        unreviewed_conflict_run_id: Option<RunId>,
    ) -> MappingStatusSnapshot {
        MappingStatusSnapshot {
            id: MappingId::new("research"),
            display_name: "Research".to_string(),
            account_id: AccountId::new("local-1"),
            collection_display_path: "Data/Research".to_string(),
            collection_folder_id: "col-1".to_string(),
            local_root: PathBuf::from("/tmp/research"),
            volume_name: "WorkSSD".to_string(),
            volume_kind: VolumeKind::External,
            volume_stable_id: "vol-1".to_string(),
            volume_space: None,
            volume_filesystem: None,
            readiness,
            execution,
            intent,
            scheduling_enabled: false,
            schedule_interval_secs: 300,
            next_poll_at: None,
            retry_at: None,
            active_generation_id: None,
            active_generation_fingerprint: None,
            last_run_id: None,
            last_run_outcome,
            last_run_completed_at: None,
            last_run_summary: None,
            stopped_first_sync: None,
            unreviewed_conflict_run_id,
            unreviewed_conflict_paths: Vec::new(),
            exclusions: Vec::new(),
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
                Some(RunOutcome::Success),
                None,
            )),
            FinderMappingState::Synced
        );
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::Ready,
                Execution::Running,
                Intent::Enabled,
                None,
                None,
            )),
            FinderMappingState::Syncing
        );
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::RecoveryRequired,
                Execution::Idle,
                Intent::Enabled,
                None,
                None,
            )),
            FinderMappingState::Attention
        );
        // Unreviewed conflict shows Attention; once reviewed, Synced even though last_run_outcome stays SuccessWithConflicts!
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::Ready,
                Execution::Idle,
                Intent::Enabled,
                Some(RunOutcome::SuccessWithConflicts),
                Some(RunId::new("run-1")),
            )),
            FinderMappingState::Attention
        );
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::Ready,
                Execution::Idle,
                Intent::Enabled,
                Some(RunOutcome::SuccessWithConflicts),
                None,
            )),
            FinderMappingState::Synced
        );
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::Ready,
                Execution::Idle,
                Intent::Paused,
                Some(RunOutcome::Success),
                None,
            )),
            FinderMappingState::Paused
        );
        assert_eq!(
            derive_finder_state(&sample_mapping(
                Readiness::VolumeUnavailable,
                Execution::Idle,
                Intent::Enabled,
                None,
                None,
            )),
            FinderMappingState::DiskMissing
        );
    }

    #[test]
    fn action_parser_enforces_strict_v1_contract() {
        let valid = br#"{"version":1,"id":"req-1","requested_at":"2026-11-20T18:46:12Z","action":"sync_now","mapping_id":"research"}"#;
        assert_eq!(
            parse_and_validate_finder_action(valid).unwrap(),
            ValidatedFinderAction::SyncNow {
                request_id: "req-1".to_string(),
                mapping_id: MappingId::new("research"),
            }
        );

        let open_no_id = br#"{"version":1,"id":"req-2","requested_at":"2026-11-20T18:46:12Z","action":"open_app"}"#;
        assert_eq!(
            parse_and_validate_finder_action(open_no_id).unwrap(),
            ValidatedFinderAction::OpenApp {
                request_id: "req-2".to_string(),
                mapping_id: None,
            }
        );

        // Missing mapping_id on pause -> rejected
        let bad_pause =
            br#"{"version":1,"id":"req-3","requested_at":"2026-11-20T18:46:12Z","action":"pause"}"#;
        assert!(parse_and_validate_finder_action(bad_pause).is_err());

        // Traversal mapping_id -> rejected
        let bad_id = br#"{"version":1,"id":"req-4","requested_at":"2026-11-20T18:46:12Z","action":"sync_now","mapping_id":"../escape"}"#;
        assert!(parse_and_validate_finder_action(bad_id).is_err());

        // Unknown version -> rejected
        let bad_ver = br#"{"version":2,"id":"req-5","requested_at":"2026-11-20T18:46:12Z","action":"open_app"}"#;
        assert!(parse_and_validate_finder_action(bad_ver).is_err());

        // Unknown extra field (e.g. injected rclone flags) -> rejected
        let extra = br#"{"version":1,"id":"req-6","requested_at":"2026-11-20T18:46:12Z","action":"open_app","flags":"--force"}"#;
        assert!(parse_and_validate_finder_action(extra).is_err());
    }
}
