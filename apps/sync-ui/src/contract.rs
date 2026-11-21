//! View-model contract between the NSH interface and the synchronization manager (§16).
//!
//! Snapshots are immutable data projected for rendering; commands carry only
//! identifiers and user intent — never secrets, raw shell strings, or rclone
//! command lines.

use sync_core::{
    AccountId, AvailableAction, Execution, Intent, LastRunSummary, MappingId, MappingState,
    Readiness, RunId, VolumeIdentity, VolumeKind, available_actions, status_text,
};

/// Top-level route shown in the main window content area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenRoute {
    /// Overview grouped by volume (or Welcome when no account/mappings exist).
    Overview,
    /// Detail view for one mapping.
    MappingDetail(MappingId),
    /// 6-step Add-mapping flow (and First sync progress view).
    AddMapping,
    /// Live activity, queue, recent events, and expandable raw log.
    Activity,
    /// Run-specific log and recovery copies inspection (§12, uit3-review1 #9).
    RunInspection {
        mapping_id: MappingId,
        run_id: RunId,
    },
    /// Ordered attention cards for held mappings and unreviewed outcomes.
    Attention,
    /// Guided recovery checklist for one mapping in `RecoveryRequired`.
    Recovery(MappingId),
    /// Connected accounts, access status, reconnect, and remove.
    Accounts,
    /// General preferences and Diagnostics tab.
    Preferences,
}

impl ScreenRoute {
    /// Sidebar section identifier highlighted for this route.
    pub fn sidebar_id(&self) -> &'static str {
        match self {
            Self::Overview | Self::MappingDetail(_) | Self::AddMapping => "overview",
            Self::Activity | Self::RunInspection { .. } => "activity",
            Self::Attention | Self::Recovery(_) => "attention",
            Self::Accounts => "accounts",
            Self::Preferences => "preferences",
        }
    }
}

/// Visual tone for status pills, icons, and attention borders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Completed / healthy.
    Ok,
    /// Active run / primary accent.
    Run,
    /// Needs review, waiting for volume, conflicts, or low space.
    Attn,
    /// Blocking error: sign-in expired, recovery required, blocked wizard review.
    Err,
    /// Neutral: paused, queued, stopping, draft.
    Neutral,
}

/// Semantic icon shape paired with every state so color is never the only signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    Grid,
    Pulse,
    Alert,
    User,
    Gear,
    DriveExternal,
    DriveInternal,
    DriveMissing,
    Cloud,
    Check,
    Sync,
    Clock,
    Pause,
    Bang,
    Octagon,
    Key,
    Dash,
    Folder,
    Plus,
    Play,
    Stop,
    Swap,
}

/// Active transfer progress phase shown on Activity and Overview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunPhase {
    /// Listing and comparing both sides: indeterminate bar + elapsed time only.
    Scanning {
        /// Elapsed seconds since the run started.
        elapsed_secs: u64,
    },
    /// Transferring with a known byte total: determinate bar + percentage.
    TransferringKnown {
        transferred_label: String,
        total_label: String,
        percent: u8,
        elapsed_secs: u64,
    },
    /// Transferring with an unknown total: indeterminate bar + "Total not known yet".
    TransferringUnknown {
        transferred_label: String,
        elapsed_secs: u64,
    },
    /// Graceful stop in flight (~90 seconds).
    Stopping { elapsed_secs: u64 },
}

/// Snapshot of an actively running or stopping job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRunSnapshot {
    pub run_id: RunId,
    pub mapping_id: MappingId,
    pub mapping_name: String,
    pub volume_name: String,
    pub started_label: String,
    pub trigger_label: String,
    pub phase: RunPhase,
}

/// Reason a mapping is held or has a special modifier on top of its state machine state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusModifier {
    None,
    /// Last completed run produced conflict copies not yet marked reviewed.
    UnreviewedConflicts {
        count: usize,
        paths: Vec<String>,
    },
    /// Last run failed transiently; automatic retry is scheduled.
    RetryPending {
        retry_at: String,
    },
    /// Never completed a run yet while Ready / Idle / Enabled.
    NeverCompleted,
    /// Paused on a specific date.
    PausedOn {
        date_label: String,
    },
    /// Queued behind an active run.
    QueuedBehind {
        active_name: String,
    },
    /// Auth problem kind (expired vs locked keychain).
    AuthProblem {
        account_label: String,
        keychain_locked: bool,
    },
    /// Endpoint change detail.
    EndpointChange {
        is_cloud: bool,
        expected_id: String,
        found_id: String,
    },
    /// Recovery cause detail.
    RecoveryCause {
        kind: RecoveryCauseKind,
        detail: String,
        recovery_copies_count: usize,
        ambiguous_ownership: bool,
    },
}

/// Distinct recovery-required causes from the spec (§5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryCauseKind {
    Interrupted,
    DeletionLimit,
    EarlierSyncRunning,
}

/// Immutable snapshot of one storage volume (Internal SSD, WorkSSD, PhotoSSD, ArchiveSSD).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeSnapshot {
    pub identity: VolumeIdentity,
    pub display_name: String,
    pub filesystem: String,
    pub connected: bool,
    pub total_gb: u64,
    pub free_gb: u64,
    pub free_label: String,
    pub total_label: String,
    /// Used percentage (0..=100) for the capacity meter bar.
    pub used_percent: u8,
    pub low_space: bool,
    pub last_seen_label: Option<String>,
    pub recovery_root_label: Option<String>,
}

/// Summary of one historical run shown in Mapping detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecordSnapshot {
    pub run_id: RunId,
    pub started_label: String,
    pub duration_label: String,
    pub result_label: String,
    pub result_tone: Tone,
    pub result_icon: IconKind,
    pub changes_label: String,
}

/// Unreviewed conflict notice stored independently of transient status modifiers
/// so Pause/Resume/SyncReady never erase it, and Mark as reviewed clears ONLY
/// this notice while preserving `RunOutcome::SuccessWithConflicts` (Q5, #7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreviewedConflicts {
    pub run_id: RunId,
    pub paths: Vec<String>,
}

/// One recovery copy preserved by a run (§12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryCopyItem {
    pub side_label: String,
    pub is_remote: bool,
    pub relative_path: String,
    pub size_label: String,
}

/// Run-specific log and recovery copies view (§12, uit3-review1 #9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedRunSnapshot {
    pub run_id: RunId,
    pub mapping_id: MappingId,
    pub mapping_name: String,
    pub started_label: String,
    pub result_label: String,
    pub log_lines: Vec<String>,
    pub log_path: Option<String>,
    pub recovery_copies: Vec<RecoveryCopyItem>,
    pub copy_notice: Option<String>,
}

/// Live recovery checklist evidence loaded from `InspectRecovery` (§5.6, #9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryChecklistData {
    pub mapping_id: MappingId,
    pub inspected: bool,
    pub is_paused: bool,
    pub orphan_alive: Option<bool>,
    pub has_ambiguous_case: bool,
    pub has_lock_file: bool,
    pub workdir_files: Vec<String>,
    pub cause: String,
    pub next_step: String,
    pub latest_run_id: Option<RunId>,
    pub recovery_copies: Vec<RecoveryCopyItem>,
}

/// Immutable snapshot of one mapping for UI rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingSnapshot {
    pub id: MappingId,
    pub display_name: String,
    pub account_id: AccountId,
    pub account_email: String,
    pub cloud_path: String,
    pub cloud_folder_id: String,
    pub local_path: String,
    pub volume_id: VolumeIdentity,
    pub volume_name: String,
    pub state: MappingState,
    pub last_run: Option<LastRunSummary>,
    pub last_completed_label: String,
    pub modifier: StatusModifier,
    /// Persistent unreviewed conflict notice (independent of Pause/Queued modifiers).
    pub unreviewed_conflicts: Option<UnreviewedConflicts>,
    pub available_actions: Vec<AvailableAction>,
    pub schedule_minutes: u32,
    pub next_check_label: String,
    pub included_subfolders: usize,
    pub total_subfolders: usize,
    pub included_size_label: String,
    pub total_size_label: String,
    pub excluded_subfolders: Vec<(String, String)>,
    pub custom_exclusions: Vec<String>,
    pub local_recovery_path: String,
    pub local_recovery_size_label: String,
    pub cloud_recovery_path: String,
    pub found_symlinks: Vec<String>,
    pub recent_runs: Vec<RunRecordSnapshot>,
    pub generation: u32,
    pub fingerprint: String,
    pub engine_options: Vec<String>,
    /// True for extra demo rows in the §6 state showcase section (keeps §2 volume counts intact).
    pub is_variant_showcase: bool,
}

/// Presented status triad derived from `MappingSnapshot` per spec §6 precedence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentedStatus {
    pub pill_label: Option<String>,
    pub pill_tone: Tone,
    pub icon: IconKind,
    pub status_line: String,
    pub why_line: Option<String>,
    pub secondary_link: Option<StatusLink>,
    pub sync_now_enabled: bool,
    pub show_stop: bool,
    pub show_force_stop: bool,
    pub show_pause: bool,
    pub show_resume: bool,
    pub is_draft: bool,
    pub needs_init: bool,
    pub sync_now_tooltip: Option<&'static str>,
}

/// Inline action link shown on an Overview row's why/next line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLink {
    ShowConflicts,
    StartRecovery,
    Reconnect,
    ReviewEndpoint,
}

impl StatusLink {
    pub fn label(self) -> &'static str {
        match self {
            Self::ShowConflicts => "Show conflicts",
            Self::StartRecovery => "Start recovery…",
            Self::Reconnect => "Reconnect…",
            Self::ReviewEndpoint => "Review…",
        }
    }
}

impl MappingSnapshot {
    /// Computes the exact status wording, icon shape, pill, and button enablement
    /// per `ui-spec.md` §6 and `sync_core::status_text` / `available_actions`.
    pub fn presented_status(&self, active_run: Option<&ActiveRunSnapshot>) -> PresentedStatus {
        let state = &self.state;
        let readiness = state.readiness();
        let execution = state.execution;
        let intent = state.intent;
        let core_text = status_text(
            state,
            &self.display_name,
            &self.volume_name,
            self.last_run.as_ref(),
        );
        let actions = available_actions(state);

        let paused_suffix = if intent == Intent::Paused
            && !matches!(
                (readiness, execution),
                (Readiness::Ready, Execution::Idle) | (Readiness::Unconfigured, _)
            ) {
            Some("Paused by you")
        } else {
            None
        };

        // 1. Stopping
        if execution == Execution::Stopping {
            let elapsed = active_run
                .filter(|r| r.mapping_id == self.id)
                .and_then(|r| match r.phase {
                    RunPhase::Stopping { elapsed_secs } => Some(format_mm_ss(elapsed_secs)),
                    _ => None,
                })
                .unwrap_or_else(|| "0:24".to_owned());
            return PresentedStatus {
                pill_label: Some("Stopping".to_owned()),
                pill_tone: Tone::Neutral,
                icon: IconKind::Stop,
                status_line: format!("Stopping · {elapsed}"),
                why_line: Some("Letting the current step finish (about 90 seconds)".to_owned()),
                secondary_link: None,
                sync_now_enabled: false,
                show_stop: false,
                show_force_stop: true,
                show_pause: intent == Intent::Enabled,
                show_resume: intent == Intent::Paused,
                is_draft: false,
                needs_init: false,
                sync_now_tooltip: Some("Wait for the current run to stop"),
            };
        }

        // 2. Running
        if execution == Execution::Running {
            let run_line = active_run
                .filter(|r| r.mapping_id == self.id)
                .map(|r| match &r.phase {
                    RunPhase::Scanning { elapsed_secs } => {
                        format!(
                            "Scanning {} · {}",
                            self.display_name,
                            format_duration(*elapsed_secs)
                        )
                    }
                    RunPhase::TransferringKnown {
                        transferred_label,
                        total_label,
                        ..
                    } => format!("Transferring · {transferred_label} of {total_label}"),
                    RunPhase::TransferringUnknown {
                        transferred_label, ..
                    } => format!("Transferring · {transferred_label} so far"),
                    RunPhase::Stopping { elapsed_secs } => {
                        format!("Stopping · {}", format_mm_ss(*elapsed_secs))
                    }
                })
                .unwrap_or_else(|| format!("{core_text} · 1m 04s"));

            let status_line = if intent == Intent::Paused {
                format!("{run_line} · pauses after this run")
            } else {
                run_line
            };

            return PresentedStatus {
                pill_label: Some("Running".to_owned()),
                pill_tone: Tone::Run,
                icon: IconKind::Sync,
                status_line,
                why_line: None,
                secondary_link: None,
                sync_now_enabled: false,
                show_stop: true,
                show_force_stop: false,
                show_pause: intent == Intent::Enabled,
                show_resume: intent == Intent::Paused,
                is_draft: false,
                needs_init: false,
                sync_now_tooltip: None,
            };
        }

        // 3. Readiness other than Ready
        match readiness {
            Readiness::Unconfigured => {
                return PresentedStatus {
                    pill_label: Some("Not set up".to_owned()),
                    pill_tone: Tone::Neutral,
                    icon: IconKind::Dash,
                    status_line: "Setup not finished".to_owned(),
                    why_line: Some("Nothing has been synced. Automatic sync is off.".to_owned()),
                    secondary_link: None,
                    sync_now_enabled: false,
                    show_stop: false,
                    show_force_stop: false,
                    show_pause: false,
                    show_resume: false,
                    is_draft: true,
                    needs_init: false,
                    sync_now_tooltip: None,
                };
            }
            Readiness::NeedsInitialization => {
                let why = match paused_suffix {
                    Some(p) => format!("{p} · automatic sync starts after the first sync."),
                    None => "Automatic sync starts after the first sync is checked.".to_owned(),
                };
                return PresentedStatus {
                    pill_label: Some("Not set up".to_owned()),
                    pill_tone: Tone::Attn,
                    icon: IconKind::Dash,
                    status_line: "First sync needed".to_owned(),
                    why_line: Some(why),
                    secondary_link: None,
                    sync_now_enabled: false,
                    show_stop: false,
                    show_force_stop: false,
                    show_pause: intent == Intent::Enabled,
                    show_resume: intent == Intent::Paused,
                    is_draft: false,
                    needs_init: true,
                    sync_now_tooltip: Some("Finish first sync setup first"),
                };
            }
            Readiness::VolumeUnavailable => {
                let status_line = format!("Waiting for {}", self.volume_name);
                let why = if intent == Intent::Paused {
                    format!(
                        "Paused by you · stays paused when {} returns.",
                        self.volume_name
                    )
                } else {
                    "Nothing is deleted in the cloud while the drive is away.".to_owned()
                };
                return PresentedStatus {
                    pill_label: Some("Waiting".to_owned()),
                    pill_tone: Tone::Attn,
                    icon: IconKind::DriveMissing,
                    status_line,
                    why_line: Some(why),
                    secondary_link: None,
                    sync_now_enabled: false,
                    show_stop: false,
                    show_force_stop: false,
                    show_pause: intent == Intent::Enabled,
                    show_resume: intent == Intent::Paused,
                    is_draft: false,
                    needs_init: false,
                    sync_now_tooltip: Some("Connect the drive to sync"),
                };
            }
            Readiness::AuthenticationRequired => {
                let (status_line, why_line) = match &self.modifier {
                    StatusModifier::AuthProblem {
                        keychain_locked: true,
                        ..
                    } => (
                        "Keychain is locked".to_owned(),
                        Some("Unlock the login keychain, then choose Check again.".to_owned()),
                    ),
                    StatusModifier::AuthProblem { account_label, .. } => (
                        format!("Sign-in for {account_label} has expired"),
                        paused_suffix.map(|p| format!("{p}.")),
                    ),
                    _ => (
                        format!("Sign-in for {} has expired", self.account_id),
                        paused_suffix.map(|p| format!("{p}.")),
                    ),
                };
                return PresentedStatus {
                    pill_label: Some("Sign-in required".to_owned()),
                    pill_tone: Tone::Err,
                    icon: IconKind::Key,
                    status_line,
                    why_line,
                    secondary_link: Some(StatusLink::Reconnect),
                    sync_now_enabled: false,
                    show_stop: false,
                    show_force_stop: false,
                    show_pause: intent == Intent::Enabled,
                    show_resume: intent == Intent::Paused,
                    is_draft: false,
                    needs_init: false,
                    sync_now_tooltip: Some("Reconnect the account first"),
                };
            }
            Readiness::EndpointChanged => {
                let status_line = match &self.modifier {
                    StatusModifier::EndpointChange {
                        is_cloud: false, ..
                    } => "Local folder changed".to_owned(),
                    _ => "Cloud folder changed".to_owned(),
                };
                let why = match paused_suffix {
                    Some(p) => format!("{p} · held until you review it"),
                    None => "Held until you review it".to_owned(),
                };
                return PresentedStatus {
                    pill_label: Some("Review needed".to_owned()),
                    pill_tone: Tone::Attn,
                    icon: IconKind::Bang,
                    status_line,
                    why_line: Some(why),
                    secondary_link: Some(StatusLink::ReviewEndpoint),
                    sync_now_enabled: false,
                    show_stop: false,
                    show_force_stop: false,
                    show_pause: intent == Intent::Enabled,
                    show_resume: intent == Intent::Paused,
                    is_draft: false,
                    needs_init: false,
                    sync_now_tooltip: Some("Review the changed folder first"),
                };
            }
            Readiness::RecoveryRequired => {
                let status_line = match &self.modifier {
                    StatusModifier::RecoveryCause {
                        kind: RecoveryCauseKind::DeletionLimit,
                        ..
                    } => "Stopped by the deletion limit".to_owned(),
                    StatusModifier::RecoveryCause {
                        kind: RecoveryCauseKind::EarlierSyncRunning,
                        ..
                    } => "Earlier sync still running".to_owned(),
                    StatusModifier::RecoveryCause { detail, .. } if !detail.is_empty() => {
                        detail.clone()
                    }
                    _ => match self.last_run {
                        Some(r) => format!("Last run was interrupted at {}", r.completed_at),
                        None => core_text,
                    },
                };
                let why = match paused_suffix {
                    Some(p) => format!("{p} · held until recovery"),
                    None => "Held until recovery".to_owned(),
                };
                return PresentedStatus {
                    pill_label: Some("Recovery required".to_owned()),
                    pill_tone: Tone::Err,
                    icon: IconKind::Octagon,
                    status_line,
                    why_line: Some(why),
                    secondary_link: Some(StatusLink::StartRecovery),
                    sync_now_enabled: false,
                    show_stop: false,
                    show_force_stop: false,
                    show_pause: intent == Intent::Enabled,
                    show_resume: intent == Intent::Paused,
                    is_draft: false,
                    needs_init: false,
                    sync_now_tooltip: Some("Complete recovery first"),
                };
            }
            Readiness::Ready => {}
        }

        // 4. Queued
        if execution == Execution::Queued {
            let status_line = match &self.modifier {
                StatusModifier::QueuedBehind { active_name } => {
                    format!("Runs after {active_name}")
                }
                _ => match active_run {
                    Some(run) => format!("Runs after {}", run.mapping_name),
                    None => "Queued to sync".to_owned(),
                },
            };
            return PresentedStatus {
                pill_label: Some("Queued".to_owned()),
                pill_tone: Tone::Neutral,
                icon: IconKind::Clock,
                status_line,
                why_line: None,
                secondary_link: None,
                sync_now_enabled: true,
                show_stop: false,
                show_force_stop: false,
                show_pause: intent == Intent::Enabled,
                show_resume: intent == Intent::Paused,
                is_draft: false,
                needs_init: false,
                sync_now_tooltip: Some("Moves to the front of the queue"),
            };
        }

        // 5. Paused
        if intent == Intent::Paused {
            let status_line = match &self.modifier {
                StatusModifier::PausedOn { date_label } => format!("Paused by you on {date_label}"),
                _ => "Paused by you".to_owned(),
            };
            return PresentedStatus {
                pill_label: Some("Paused".to_owned()),
                pill_tone: Tone::Neutral,
                icon: IconKind::Pause,
                status_line,
                why_line: None,
                secondary_link: None,
                sync_now_enabled: false,
                show_stop: false,
                show_force_stop: false,
                show_pause: false,
                show_resume: true,
                is_draft: false,
                needs_init: false,
                sync_now_tooltip: Some("Resume to run"),
            };
        }

        // 6. Last outcome (Ready / Idle / Enabled)
        let sync_now_enabled = actions.contains(&AvailableAction::RunNow);
        if let Some(unrev) = &self.unreviewed_conflicts {
            let count = unrev.paths.len().max(1);
            let at = self
                .last_run
                .map(|r| format!(" at {}", r.completed_at))
                .unwrap_or_default();
            let copies = if count == 1 {
                "1 conflict copy kept".to_owned()
            } else {
                format!("{count} conflict copies kept")
            };
            return PresentedStatus {
                pill_label: Some("Conflicts".to_owned()),
                pill_tone: Tone::Attn,
                icon: IconKind::Bang,
                status_line: format!("Completed with conflicts{at}"),
                why_line: Some(copies),
                secondary_link: Some(StatusLink::ShowConflicts),
                sync_now_enabled,
                show_stop: false,
                show_force_stop: false,
                show_pause: true,
                show_resume: false,
                is_draft: false,
                needs_init: false,
                sync_now_tooltip: None,
            };
        }
        match &self.modifier {
            StatusModifier::UnreviewedConflicts { count, .. } => {
                let at = self
                    .last_run
                    .map(|r| format!(" at {}", r.completed_at))
                    .unwrap_or_default();
                let copies = if *count == 1 {
                    "1 conflict copy kept".to_owned()
                } else {
                    format!("{count} conflict copies kept")
                };
                PresentedStatus {
                    pill_label: Some("Conflicts".to_owned()),
                    pill_tone: Tone::Attn,
                    icon: IconKind::Bang,
                    status_line: format!("Completed with conflicts{at}"),
                    why_line: Some(copies),
                    secondary_link: Some(StatusLink::ShowConflicts),
                    sync_now_enabled,
                    show_stop: false,
                    show_force_stop: false,
                    show_pause: true,
                    show_resume: false,
                    is_draft: false,
                    needs_init: false,
                    sync_now_tooltip: None,
                }
            }
            StatusModifier::RetryPending { retry_at } => PresentedStatus {
                pill_label: None,
                pill_tone: Tone::Attn,
                icon: IconKind::Clock,
                status_line: format!("Couldn't reach Google Drive · retrying at {retry_at}"),
                why_line: Some("Check the network connection. Nothing was changed.".to_owned()),
                secondary_link: None,
                sync_now_enabled,
                show_stop: false,
                show_force_stop: false,
                show_pause: true,
                show_resume: false,
                is_draft: false,
                needs_init: false,
                sync_now_tooltip: None,
            },
            StatusModifier::NeverCompleted => PresentedStatus {
                pill_label: None,
                pill_tone: Tone::Neutral,
                icon: IconKind::Dash,
                status_line: "Not run yet".to_owned(),
                why_line: None,
                secondary_link: None,
                sync_now_enabled,
                show_stop: false,
                show_force_stop: false,
                show_pause: true,
                show_resume: false,
                is_draft: false,
                needs_init: false,
                sync_now_tooltip: None,
            },
            _ => PresentedStatus {
                pill_label: None,
                pill_tone: Tone::Ok,
                icon: IconKind::Check,
                status_line: core_text,
                why_line: None,
                secondary_link: None,
                sync_now_enabled,
                show_stop: false,
                show_force_stop: false,
                show_pause: true,
                show_resume: false,
                is_draft: false,
                needs_init: false,
                sync_now_tooltip: None,
            },
        }
    }

    /// True if this mapping requires attention (blocking readiness hold or unreviewed conflicts).
    pub fn needs_attention(&self) -> bool {
        if self.is_variant_showcase {
            return false;
        }
        matches!(
            self.state.readiness(),
            Readiness::AuthenticationRequired
                | Readiness::RecoveryRequired
                | Readiness::EndpointChanged
                | Readiness::VolumeUnavailable
        ) || self.unreviewed_conflicts.is_some()
            || matches!(self.modifier, StatusModifier::UnreviewedConflicts { .. })
    }
}

/// Ordered category for Attention cards (§5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AttentionPriority {
    SignIn = 0,
    Recovery = 1,
    EndpointChanged = 2,
    VolumeMissing = 3,
    Conflicts = 4,
    LowSpaceAdvisory = 5,
}

/// Kind of Attention card with structured cause / what FolderSync did / next step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionKind {
    Authentication {
        account_id: AccountId,
        keychain_locked: bool,
    },
    VolumeMissing {
        volume_name: String,
    },
    EndpointChanged {
        is_cloud: bool,
        expected_id: String,
        found_id: String,
    },
    RecoveryRequired {
        cause_kind: RecoveryCauseKind,
    },
    Conflicts {
        conflict_paths: Vec<String>,
    },
    LowSpaceAdvisory {
        volume_name: String,
        recovery_path: String,
    },
}

/// One card on the Attention screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionItem {
    pub id: String,
    pub priority: AttentionPriority,
    pub mapping_id: Option<MappingId>,
    pub mapping_name: Option<String>,
    pub pill_label: String,
    pub pill_tone: Tone,
    pub icon: IconKind,
    pub is_error_accent: bool,
    pub title: String,
    pub cause: String,
    pub what_did: String,
    pub next_step: String,
    pub kind: AttentionKind,
}

/// Connection state of a configured Google Drive account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountStatus {
    Connected,
    SignInRequired,
    KeychainLocked,
    Reconnecting,
}

/// Snapshot of one configured Google account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSnapshot {
    pub id: AccountId,
    pub email: String,
    pub shared_parent_label: String,
    pub status: AccountStatus,
    pub access_summary: String,
    pub mapping_count: usize,
}

/// One bounded entry in the Activity recent-events list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEvent {
    pub id: u64,
    pub time_label: String,
    pub message: String,
    pub link: Option<(String, ScreenRoute)>,
}

/// Steps in the 6-step Add-mapping flow (plus post-start First sync progress).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WizardStep {
    Account = 1,
    Folder = 2,
    Location = 3,
    Subfolders = 4,
    Recovery = 5,
    Review = 6,
    FirstSync = 7,
}

impl WizardStep {
    pub const SIX_STEPS: [(WizardStep, &'static str); 6] = [
        (WizardStep::Account, "Account"),
        (WizardStep::Folder, "Folder"),
        (WizardStep::Location, "Location"),
        (WizardStep::Subfolders, "Subfolders"),
        (WizardStep::Recovery, "Recovery"),
        (WizardStep::Review, "Review"),
    ];

    pub fn number(self) -> usize {
        self as usize
    }

    pub fn prev(self, skip_account: bool) -> Option<Self> {
        match self {
            Self::Account => None,
            Self::Folder => (!skip_account).then_some(Self::Account),
            Self::Location => Some(Self::Folder),
            Self::Subfolders => Some(Self::Location),
            Self::Recovery => Some(Self::Subfolders),
            Self::Review => Some(Self::Recovery),
            Self::FirstSync => None,
        }
    }

    pub fn next(self) -> Option<Self> {
        match self {
            Self::Account => Some(Self::Folder),
            Self::Folder => Some(Self::Location),
            Self::Location => Some(Self::Subfolders),
            Self::Subfolders => Some(Self::Recovery),
            Self::Recovery => Some(Self::Review),
            Self::Review => Some(Self::FirstSync),
            Self::FirstSync => None,
        }
    }
}

/// Immediate child folder under `household:Data` listed in Wizard Step 2.
#[derive(Debug, Clone, PartialEq)]
pub struct CloudFolderCandidate {
    pub name: String,
    pub size_gb: Option<f64>,
    pub size_label: String,
    pub on_this_mac: Option<String>,
    pub used_by_other_computer: bool,
}

/// One node in the Wizard Step 4 subfolder selection tree.
#[derive(Debug, Clone, PartialEq)]
pub struct SubfolderNode {
    pub id: String,
    pub name: String,
    pub depth: u32,
    pub has_children: bool,
    pub size_gb: f64,
    pub size_label: String,
    pub included: bool,
}

/// One validation check line in Wizard Step 3 (Location).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationCheck {
    pub ok: bool,
    pub message: String,
}

/// Initial-sync condition shown on Wizard Step 6 (Review).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewScenario {
    /// Cloud has files, local folder is empty -> download first sync.
    CloudToLocal,
    /// Local has files, cloud folder is empty -> upload first sync.
    LocalToCloud,
    /// Both sides already contain files -> BLOCKED, no Start button.
    BothPopulated,
    /// Both sides are empty -> marker setup, nothing to transfer.
    BothEmpty,
}

/// Draft state for the 6-step Add-mapping flow.
#[derive(Debug, Clone, PartialEq)]
pub struct WizardDraft {
    pub step: WizardStep,
    /// Mapping ID when resuming an existing draft or finishing first sync (#3).
    pub editing_mapping_id: Option<MappingId>,
    pub selected_account: AccountId,
    pub selected_folder: String,
    /// Local-to-local fixture remote folder path (used by real ManagerSource).
    pub remote_folder_path: Option<String>,
    pub local_path: String,
    pub volume_name: String,
    pub volume_kind: VolumeKind,
    pub volume_fs: String,
    pub volume_free_gb: u64,
    pub volume_total_gb: u64,
    pub location_checks: Vec<LocationCheck>,
    pub subfolders: Vec<SubfolderNode>,
    pub custom_patterns: Vec<String>,
    pub show_default_exclusions: bool,
    pub local_recovery_path: String,
    pub cloud_recovery_path: String,
    pub recovery_checks: Vec<LocationCheck>,
    pub review_scenario: ReviewScenario,
    /// Real inventory counts & byte totals for Review & FirstSync (#4).
    pub cloud_file_count: Option<usize>,
    pub cloud_total_bytes: Option<u64>,
    pub local_file_count: Option<usize>,
    pub local_total_bytes: Option<u64>,
    /// True only when a recognized collection marker already exists on the remote side (§9 step 5).
    pub marker_reused: bool,
    /// Unsupported items observed during inventory (§18).
    pub found_symlinks: Vec<String>,
    pub found_name_issues: Vec<String>,
    /// When `BothPopulated` is triggered in the demo, remembers the `(folder, normalized_local_path)`
    /// pair that was blocked so Start stays blocked for that pair while a different valid destination
    /// re-derives the scenario from demo data.
    pub blocked_populated_pair: Option<(String, String)>,
    pub no_other_sync_confirmed: bool,
    pub first_sync_mapping_id: Option<MappingId>,
    pub first_sync_percent: u8,
    pub first_sync_elapsed_secs: u64,
    pub first_sync_completed: bool,
    pub first_sync_error: Option<String>,
}

impl WizardDraft {
    /// Sum of included subfolder sizes (GB) at depth 1, or cloud/local byte total if flat.
    pub fn included_gb(&self) -> f64 {
        let sub_sum: f64 = self
            .subfolders
            .iter()
            .filter(|n| n.depth == 1 && n.included)
            .map(|n| n.size_gb)
            .sum();
        if self.subfolders.iter().any(|n| n.depth == 1) {
            sub_sum
        } else if let Some(bytes) = self.cloud_total_bytes.or(self.local_total_bytes) {
            (bytes as f64) / 1_000_000_000.0
        } else {
            self.subfolders
                .iter()
                .find(|n| n.depth == 0 && n.included)
                .map(|n| n.size_gb)
                .unwrap_or(0.0)
        }
    }

    /// Total size of the folder (GB), returning `None` when unknown ("Measuring…").
    pub fn total_gb_opt(&self) -> Option<f64> {
        if let Some(root) = self.subfolders.iter().find(|n| n.depth == 0) {
            return Some(root.size_gb);
        }
        self.cloud_total_bytes
            .or(self.local_total_bytes)
            .map(|b| (b as f64) / 1_000_000_000.0)
    }

    /// Total size of the folder (GB).
    pub fn total_gb(&self) -> f64 {
        self.total_gb_opt().unwrap_or(0.0)
    }

    /// Excluded subfolder names.
    pub fn excluded_names(&self) -> Vec<String> {
        self.subfolders
            .iter()
            .filter(|n| n.depth == 1 && !n.included)
            .map(|n| n.name.clone())
            .collect()
    }

    /// Whether Location step checks all pass.
    pub fn location_valid(&self) -> bool {
        !self.location_checks.is_empty() && self.location_checks.iter().all(|c| c.ok)
    }

    /// Whether Recovery step checks all pass.
    pub fn recovery_valid(&self) -> bool {
        !self.recovery_checks.is_empty() && self.recovery_checks.iter().all(|c| c.ok)
    }

    /// Whether the user may jump to `target` without skipping past a failing step.
    pub fn can_visit_step(&self, target: WizardStep) -> bool {
        if target <= WizardStep::Location {
            return true;
        }
        if !self.location_valid() {
            return false;
        }
        if target > WizardStep::Recovery && !self.recovery_valid() {
            return false;
        }
        true
    }

    /// Whether the Start first sync button may be enabled.
    pub fn can_start_first_sync(&self) -> bool {
        self.location_valid()
            && self.recovery_valid()
            && self.review_scenario != ReviewScenario::BothPopulated
            && self.no_other_sync_confirmed
    }
}

/// Finder Sync extension status shown in Preferences → General.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinderExtensionStatus {
    Enabled { last_status_label: String },
    Disabled,
    Unavailable,
}

/// Preferences snapshot (General + Diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferencesSnapshot {
    pub open_at_login: bool,
    pub open_at_login_pending: bool,
    pub notifications_enabled: bool,
    pub finder_extension: FinderExtensionStatus,
    pub bandwidth_limit_mb_s: Option<u32>,
    pub app_version: String,
    pub engine_version: String,
    pub engine_checksum: String,
    pub nsh_version: String,
    pub system_summary: String,
}

/// Demo presets accessible from the in-memory demo switcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoPreset {
    /// Full §2 household dataset + all remaining §6 state variants visible at once.
    AllStates,
    /// Exact §2 household dataset (8 mappings on 4 volumes).
    HouseholdOnly,
    /// First-run Welcome state with no account connected.
    WelcomeNoAccount,
    /// Welcome state with account connected, no mappings yet.
    WelcomeNoMappings,
}

/// Immutable snapshot delivered from the manager (or demo source) to the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct UiSnapshot {
    pub is_demo_mode: bool,
    pub device_id: String,
    pub scheduling_paused: bool,
    pub demo_preset: DemoPreset,
    pub accounts: Vec<AccountSnapshot>,
    pub volumes: Vec<VolumeSnapshot>,
    pub mappings: Vec<MappingSnapshot>,
    pub active_run: Option<ActiveRunSnapshot>,
    /// Actual ordered queue of waiting mapping IDs (front first, #6).
    pub queue_order: Vec<MappingId>,
    pub attention_items: Vec<AttentionItem>,
    pub recent_events: Vec<ActivityEvent>,
    pub raw_log: Vec<String>,
    pub inspected_run: Option<InspectedRunSnapshot>,
    pub recovery_checklist: Option<RecoveryChecklistData>,
    pub cloud_catalog: Vec<CloudFolderCandidate>,
    pub cloud_catalog_measured_at: String,
    pub wizard: WizardDraft,
    pub preferences: PreferencesSnapshot,
    pub reconnecting_account: Option<AccountId>,
    pub status_notice: Option<String>,
    /// Actionable manager error displayed prominently in the UI with a Dismiss control.
    pub action_error: Option<String>,
}

impl UiSnapshot {
    /// Number of primary attention items shown on the sidebar badge and Overview summary.
    /// Excludes low-space advisory so the household sample shows the spec's 5 blocking/review items.
    pub fn attention_badge_count(&self) -> usize {
        self.attention_items
            .iter()
            .filter(|item| item.priority != AttentionPriority::LowSpaceAdvisory)
            .count()
    }

    /// Number of real (non-variant-showcase) mappings currently ready, idle, and enabled.
    pub fn ready_mapping_count(&self) -> usize {
        self.mappings
            .iter()
            .filter(|m| {
                m.state.readiness() == Readiness::Ready
                    && m.state.execution == Execution::Idle
                    && m.state.intent == Intent::Enabled
            })
            .count()
    }

    /// Primary mappings belonging to the given volume (non-variant rows).
    pub fn mappings_for_volume(&self, volume: &VolumeIdentity) -> Vec<&MappingSnapshot> {
        self.mappings
            .iter()
            .filter(|m| !m.is_variant_showcase && &m.volume_id == volume)
            .collect()
    }

    /// Additional §6 state showcase rows (shown under "All states" demo preset).
    pub fn variant_showcase_mappings(&self) -> Vec<&MappingSnapshot> {
        self.mappings
            .iter()
            .filter(|m| m.is_variant_showcase)
            .collect()
    }

    /// Finds a mapping snapshot by its identifier.
    pub fn mapping(&self, id: &MappingId) -> Option<&MappingSnapshot> {
        self.mappings.iter().find(|m| &m.id == id)
    }

    /// True if any mapping is currently running or stopping.
    pub fn has_active_run(&self) -> bool {
        self.active_run.is_some()
            || self.mappings.iter().any(|m| {
                !m.is_variant_showcase
                    && matches!(m.state.execution, Execution::Running | Execution::Stopping)
            })
    }

    /// One-line tray status summary matching spec §5.9.
    pub fn tray_status_line(&self) -> String {
        if self.scheduling_paused {
            return "Scheduling paused".to_owned();
        }
        let attn = self.attention_badge_count();
        match (&self.active_run, attn) {
            (Some(run), n) if n > 0 => {
                format!("Running: {} · {n} need attention", run.mapping_name)
            }
            (Some(run), _) => format!("Running: {}", run.mapping_name),
            (None, n) if n > 0 => format!("{n} need attention"),
            (None, _) => "Scheduling on".to_owned(),
        }
    }

    /// Tray tooltip matching spec §5.9.
    pub fn tray_tooltip(&self) -> String {
        let attn = self.attention_badge_count();
        if self.scheduling_paused {
            "FolderSync · scheduling paused".to_owned()
        } else if attn > 0 {
            format!("FolderSync · {attn} need attention")
        } else {
            "FolderSync".to_owned()
        }
    }
}

/// Commands sent from the UI to the manager (or demo source).
/// Carry IDs and user intent only — never secrets or raw engine commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    SyncNow(MappingId),
    SyncReadyMappings,
    PauseMapping(MappingId),
    ResumeMapping(MappingId),
    StopRun(MappingId),
    ForceStopRun(MappingId),
    SetSchedulingPaused(bool),
    MarkConflictsReviewed(MappingId),
    CheckAgain(MappingId),
    InspectRecovery(MappingId),
    InspectRun {
        mapping_id: MappingId,
        run_id: RunId,
    },
    CopyRecoveryCopyToInspection {
        run_id: RunId,
        is_remote: bool,
        relative_path: String,
        dest_dir: String,
    },
    CompleteRecovery {
        mapping_id: MappingId,
        resume_and_run: bool,
    },
    RemoveMapping(MappingId),
    DiscardDraft(MappingId),
    ResumeDraft(MappingId),
    FinishSetup(MappingId),
    SetScheduleInterval {
        mapping_id: MappingId,
        minutes: u32,
    },
    ConnectDemoAccount,
    AddLocalFixtureAccount {
        id: String,
        display_name: String,
        parent_dir: String,
    },
    ReconnectAccount(AccountId),
    CancelReconnect,
    CheckAccountAccess(AccountId),
    RemoveAccount(AccountId),
    SetOpenAtLogin(bool),
    SetNotifications(bool),
    SetBandwidthLimit(Option<u32>),
    WizardSetStep(WizardStep),
    WizardSelectAccount(AccountId),
    WizardSelectFolder(String),
    WizardCreateNewFolder(String),
    WizardSetRemotePath(String),
    WizardSetLocalPath(String),
    WizardToggleSubfolder(String),
    WizardToggleDefaultExclusions,
    WizardAddPattern(String),
    WizardSetLocalRecoveryPath(String),
    WizardSetReviewScenario(ReviewScenario),
    WizardSetNoOtherSyncConfirmed(bool),
    WizardSaveDraft,
    WizardStartFirstSync,
    WizardStopFirstSync,
    WizardCompleteDemoFirstSync,
    WizardReset,
    /// Injectable system actions (routed to RealMacOsSystemActions or RecordingSystemActions).
    RevealPath(String),
    OpenCloudUrl(String),
    OpenExtensionSettings,
    ExportDiagnostics(String),
    DismissError,
    PollFinderActions,
    /// Switch active demo progress variant on the Activity screen.
    DemoSetActivityPhase(DemoActivityPhase),
    /// Switch entire demo preset (AllStates / HouseholdOnly / Welcome states).
    DemoSetPreset(DemoPreset),
    /// Step a mapping to the next Readiness × Execution × Intent state in the demo switcher.
    DemoStepMappingState(MappingId),
    /// Advance throttled demo elapsed timer by 1 second (no filesystem mutation).
    DemoTickElapsed,
    /// Record a simulated system action (Reveal, Open URL, Export) in Activity without touching the OS.
    DemoAction(String),
    /// Controlled quit handler (marks active run stopping and shuts down manager).
    RequestControlledQuit,
}

/// Determines whether the main window should be shown at launch when
/// `nsh.json` starts it hidden (`defaultVisible: false`, spec §3).
///
/// Always shows when no account or mapping exists yet (Welcome flow) or on a
/// manual launch; stays hidden in the menu bar on a login launch when mappings
/// are already configured.
pub fn should_show_window_at_launch(snap: &UiSnapshot, launched_at_login: bool) -> bool {
    snap.accounts.is_empty() || snap.mappings.is_empty() || !launched_at_login
}

/// Returns the mapping targeted by `⌘R` (Sync now): only the currently displayed
/// mapping in `MappingDetail` when its `Sync now` action is enabled; `None` otherwise (§8).
pub fn selected_sync_now_target(route: &ScreenRoute, snap: &UiSnapshot) -> Option<MappingId> {
    let ScreenRoute::MappingDetail(id) = route else {
        return None;
    };
    let m = snap.mapping(id)?;
    let pres = m.presented_status(snap.active_run.as_ref());
    pres.sync_now_enabled.then(|| id.clone())
}

/// Returns the mapping targeted by `⌘.` (Stop): only the displayed running mapping
/// in `MappingDetail` or the active running mapping displayed on `Activity`.
pub fn selected_stop_target(route: &ScreenRoute, snap: &UiSnapshot) -> Option<MappingId> {
    match route {
        ScreenRoute::MappingDetail(id) => {
            let m = snap.mapping(id)?;
            (m.state.execution == Execution::Running).then(|| id.clone())
        }
        ScreenRoute::Activity => snap
            .active_run
            .as_ref()
            .filter(|r| !matches!(r.phase, RunPhase::Stopping { .. }))
            .map(|r| r.mapping_id.clone()),
        _ => None,
    }
}

/// Progress display variants on the Activity screen (§5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoActivityPhase {
    Scanning,
    TransferringKnown,
    TransferringUnknown,
    Stopping,
}

/// Formats seconds as `Xm Ys` (or `Ys` when under a minute).
pub fn format_duration(secs: u64) -> String {
    let mins = secs / 60;
    let rem = secs % 60;
    if mins > 0 {
        format!("{mins}m {rem:02}s")
    } else {
        format!("{rem}s")
    }
}

/// Formats seconds as `m:ss` (used for Stopping · 0:24).
pub fn format_mm_ss(secs: u64) -> String {
    let mins = secs / 60;
    let rem = secs % 60;
    format!("{mins}:{rem:02}")
}
