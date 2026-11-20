//! Mapping readiness, execution, and intent state machine, pure transition
//! functions, available action queries, and household-friendly status text.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Readiness dimension describing whether a mapping is eligible to sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Readiness {
    /// Mapping draft has not finished configuration.
    Unconfigured,
    /// Configuration is valid but initial baseline synchronization has not completed.
    NeedsInitialization,
    /// Configuration, volume, credentials, and baseline are all ready.
    Ready,
    /// Bound local volume is absent or unmounted.
    VolumeUnavailable,
    /// Account credentials are missing, expired, revoked, or locked.
    AuthenticationRequired,
    /// Cloud folder identity or local endpoint binding changed and requires review.
    EndpointChanged,
    /// Critical, unknown, corrupt, or interrupted state requires explicit recovery.
    RecoveryRequired,
}

/// Execution dimension describing the live run lifecycle for a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Execution {
    /// No synchronization job is queued or running.
    Idle,
    /// A synchronization run is queued to start.
    Queued,
    /// A synchronization child process is actively running.
    Running,
    /// Graceful cancellation has been requested and the child is winding down.
    Stopping,
    /// The previous run ended in an uncertain interrupted state.
    Interrupted,
}

/// User intent dimension describing whether automatic scheduling is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Intent {
    /// Scheduling and runs are enabled when readiness permits.
    Enabled,
    /// User explicitly paused scheduling; only [`UserAction::Resume`] clears this.
    Paused,
}

/// Full three-dimensional state of a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MappingState {
    /// Readiness of endpoints, credentials, volume, and recovery holds.
    pub readiness: Readiness,
    /// Current run execution status.
    pub execution: Execution,
    /// Persisted user pause/enable intent.
    pub intent: Intent,
}

impl MappingState {
    /// Constructs a mapping state from its three orthogonal dimensions.
    pub const fn new(readiness: Readiness, execution: Execution, intent: Intent) -> Self {
        Self {
            readiness,
            execution,
            intent,
        }
    }

    /// Returns the default state for a newly created draft mapping.
    pub const fn unconfigured() -> Self {
        Self {
            readiness: Readiness::Unconfigured,
            execution: Execution::Idle,
            intent: Intent::Paused,
        }
    }
}

/// Explicit user actions accepted by the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UserAction {
    /// Hide the UI window while keeping the manager running.
    CloseWindow,
    /// Pause future scheduling without stopping a currently active run.
    Pause,
    /// Clear user pause; starts or queues a run only when [`Readiness::Ready`].
    Resume,
    /// Request an immediate manual run if ready, enabled, and idle.
    RunNow,
    /// Request graceful cancellation of an active run.
    StopRun,
    /// Emergency forced termination retaining an interrupted recovery state.
    ForceStop,
    /// Explicit application quit requesting controlled shutdown.
    QuitRequested,
}

/// Classified outcome of a finished synchronization run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RunOutcome {
    /// Run completed and verified with no conflicts.
    Success,
    /// Run completed and verified, and created conflict copies.
    SuccessWithConflicts,
    /// Transient network or service failure eligible for qualified backoff.
    RetryableFailure,
    /// Credentials expired, revoked, or locked during the run.
    AuthenticationFailure,
    /// Local volume or cloud endpoint identity no longer matched.
    EndpointOrVolumeMismatch,
    /// Invalid command or configuration detected.
    InvalidConfiguration,
    /// Critical engine error requiring recovery.
    CriticalFailure,
    /// Unclassified or unknown engine failure requiring recovery.
    UnknownFailure,
    /// Corrupt baseline or bisync work directory detected.
    CorruptState,
    /// Run was interrupted before completion.
    Interrupted,
}

/// Readiness states that an explicit recovery workflow may transition into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PostRecoveryReadiness {
    /// Baseline and endpoints verified; mapping is ready.
    Ready,
    /// Recovery determined that guided reinitialization is required.
    NeedsInitialization,
    /// Recovery completed, but the bound volume is currently absent.
    VolumeUnavailable,
    /// Recovery completed, but account credentials still need renewal.
    AuthenticationRequired,
    /// Recovery completed, but endpoint changes still await review.
    EndpointChanged,
}

impl From<PostRecoveryReadiness> for Readiness {
    fn from(value: PostRecoveryReadiness) -> Self {
        match value {
            PostRecoveryReadiness::Ready => Self::Ready,
            PostRecoveryReadiness::NeedsInitialization => Self::NeedsInitialization,
            PostRecoveryReadiness::VolumeUnavailable => Self::VolumeUnavailable,
            PostRecoveryReadiness::AuthenticationRequired => Self::AuthenticationRequired,
            PostRecoveryReadiness::EndpointChanged => Self::EndpointChanged,
        }
    }
}

/// External or manager-observed events fed into the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObservedEvent {
    /// Draft configuration passed validation and now awaits initialization.
    Configured,
    /// Guided initialization succeeded and established a verified baseline.
    InitializationCompleted,
    /// Periodic scheduler or file watcher requested a run.
    ScheduleTriggered,
    /// Manager launched the child process for a queued run.
    RunStarted,
    /// Bound volume disappeared or became inaccessible.
    VolumeLost,
    /// Bound volume reconnected and passed identity validation.
    VolumeAvailable,
    /// Account authentication check failed or credential store is locked.
    AuthenticationFailed,
    /// Account authentication was restored and verified.
    AuthenticationRestored,
    /// Cloud folder identity or local endpoint path changed unexpectedly.
    EndpointChanged,
    /// Operator reviewed and revalidated the endpoint identity.
    EndpointValidated,
    /// Active synchronization run finished with the given outcome.
    RunFinished(RunOutcome),
    /// Explicit recovery procedure completed and cleared the recovery hold.
    RecoveryCompleted {
        /// Verified readiness state following recovery.
        next_readiness: PostRecoveryReadiness,
    },
}

/// Side effect recommended to the manager by a pure state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransitionEffect {
    /// No external manager action is required.
    None,
    /// Queue or launch a synchronization run for this mapping.
    ScheduleRun,
    /// Send graceful cancellation to the active child process.
    RequestGracefulStop,
    /// Force-terminate the child process after emergency user confirmation.
    ForceTerminateChild,
    /// Stop future scheduling during application shutdown.
    StopScheduling,
    /// Stop scheduling and gracefully cancel the active child during shutdown.
    StopSchedulingAndRequestGracefulStop,
    /// Mapping became ready while enabled and may be picked up by the scheduler.
    EligibleForScheduling,
}

/// Result of a pure state transition containing the next state and effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TransitionResult {
    /// Resulting mapping state.
    pub state: MappingState,
    /// Recommended manager side effect.
    pub effect: TransitionEffect,
}

/// Applies a user action to `state` and returns the next state and effect.
pub fn apply_user_action(state: MappingState, action: UserAction) -> TransitionResult {
    match action {
        UserAction::CloseWindow => TransitionResult {
            state,
            effect: TransitionEffect::None,
        },
        UserAction::Pause => {
            let execution = if state.execution == Execution::Queued {
                Execution::Idle
            } else {
                state.execution
            };
            TransitionResult {
                state: MappingState {
                    intent: Intent::Paused,
                    execution,
                    ..state
                },
                effect: TransitionEffect::None,
            }
        }
        UserAction::Resume => {
            let can_run = state.readiness == Readiness::Ready && state.execution == Execution::Idle;
            let (execution, effect) = if can_run {
                (Execution::Queued, TransitionEffect::ScheduleRun)
            } else {
                (state.execution, TransitionEffect::None)
            };
            TransitionResult {
                state: MappingState {
                    intent: Intent::Enabled,
                    execution,
                    ..state
                },
                effect,
            }
        }
        UserAction::RunNow => {
            if state.readiness == Readiness::Ready
                && state.intent == Intent::Enabled
                && state.execution == Execution::Idle
            {
                TransitionResult {
                    state: MappingState {
                        execution: Execution::Queued,
                        ..state
                    },
                    effect: TransitionEffect::ScheduleRun,
                }
            } else {
                TransitionResult {
                    state,
                    effect: TransitionEffect::None,
                }
            }
        }
        UserAction::StopRun => match state.execution {
            Execution::Running => TransitionResult {
                state: MappingState {
                    execution: Execution::Stopping,
                    ..state
                },
                effect: TransitionEffect::RequestGracefulStop,
            },
            Execution::Queued => TransitionResult {
                state: MappingState {
                    execution: Execution::Idle,
                    ..state
                },
                effect: TransitionEffect::None,
            },
            Execution::Idle | Execution::Stopping | Execution::Interrupted => TransitionResult {
                state,
                effect: TransitionEffect::None,
            },
        },
        UserAction::ForceStop => match state.execution {
            Execution::Running | Execution::Stopping => TransitionResult {
                state: MappingState {
                    readiness: Readiness::RecoveryRequired,
                    execution: Execution::Interrupted,
                    ..state
                },
                effect: TransitionEffect::ForceTerminateChild,
            },
            Execution::Queued => TransitionResult {
                state: MappingState {
                    execution: Execution::Idle,
                    ..state
                },
                effect: TransitionEffect::None,
            },
            Execution::Idle | Execution::Interrupted => TransitionResult {
                state,
                effect: TransitionEffect::None,
            },
        },
        UserAction::QuitRequested => match state.execution {
            Execution::Running => TransitionResult {
                state: MappingState {
                    execution: Execution::Stopping,
                    ..state
                },
                effect: TransitionEffect::StopSchedulingAndRequestGracefulStop,
            },
            Execution::Queued => TransitionResult {
                state: MappingState {
                    execution: Execution::Idle,
                    ..state
                },
                effect: TransitionEffect::StopScheduling,
            },
            Execution::Idle | Execution::Stopping | Execution::Interrupted => TransitionResult {
                state,
                effect: TransitionEffect::StopScheduling,
            },
        },
    }
}

/// Applies an observed event to `state` and returns the next state and effect.
pub fn apply_event(state: MappingState, event: ObservedEvent) -> TransitionResult {
    match event {
        ObservedEvent::Configured => {
            let readiness = if state.readiness == Readiness::Unconfigured {
                Readiness::NeedsInitialization
            } else {
                state.readiness
            };
            TransitionResult {
                state: MappingState { readiness, ..state },
                effect: TransitionEffect::None,
            }
        }
        ObservedEvent::InitializationCompleted => {
            if state.readiness == Readiness::NeedsInitialization {
                let next = MappingState {
                    readiness: Readiness::Ready,
                    ..state
                };
                let effect = if next.intent == Intent::Enabled && next.execution == Execution::Idle
                {
                    TransitionEffect::EligibleForScheduling
                } else {
                    TransitionEffect::None
                };
                TransitionResult {
                    state: next,
                    effect,
                }
            } else {
                TransitionResult {
                    state,
                    effect: TransitionEffect::None,
                }
            }
        }
        ObservedEvent::ScheduleTriggered => {
            if state.readiness == Readiness::Ready
                && state.intent == Intent::Enabled
                && state.execution == Execution::Idle
            {
                TransitionResult {
                    state: MappingState {
                        execution: Execution::Queued,
                        ..state
                    },
                    effect: TransitionEffect::ScheduleRun,
                }
            } else {
                TransitionResult {
                    state,
                    effect: TransitionEffect::None,
                }
            }
        }
        ObservedEvent::RunStarted => {
            if state.readiness == Readiness::Ready && state.execution == Execution::Queued {
                TransitionResult {
                    state: MappingState {
                        execution: Execution::Running,
                        ..state
                    },
                    effect: TransitionEffect::None,
                }
            } else {
                TransitionResult {
                    state,
                    effect: TransitionEffect::None,
                }
            }
        }
        ObservedEvent::VolumeLost => {
            let readiness = match state.readiness {
                Readiness::RecoveryRequired | Readiness::Unconfigured => state.readiness,
                _ => Readiness::VolumeUnavailable,
            };
            let (execution, effect) = match state.execution {
                Execution::Running => (Execution::Stopping, TransitionEffect::RequestGracefulStop),
                Execution::Queued => (Execution::Idle, TransitionEffect::None),
                other => (other, TransitionEffect::None),
            };
            TransitionResult {
                state: MappingState {
                    readiness,
                    execution,
                    ..state
                },
                effect,
            }
        }
        ObservedEvent::VolumeAvailable => {
            if state.readiness == Readiness::VolumeUnavailable {
                let next = MappingState {
                    readiness: Readiness::Ready,
                    ..state
                };
                let effect = if next.intent == Intent::Enabled && next.execution == Execution::Idle
                {
                    TransitionEffect::EligibleForScheduling
                } else {
                    TransitionEffect::None
                };
                TransitionResult {
                    state: next,
                    effect,
                }
            } else {
                TransitionResult {
                    state,
                    effect: TransitionEffect::None,
                }
            }
        }
        ObservedEvent::AuthenticationFailed => {
            let readiness = match state.readiness {
                Readiness::RecoveryRequired | Readiness::Unconfigured => state.readiness,
                _ => Readiness::AuthenticationRequired,
            };
            let execution = if state.execution == Execution::Queued {
                Execution::Idle
            } else {
                state.execution
            };
            TransitionResult {
                state: MappingState {
                    readiness,
                    execution,
                    ..state
                },
                effect: TransitionEffect::None,
            }
        }
        ObservedEvent::AuthenticationRestored => {
            if state.readiness == Readiness::AuthenticationRequired {
                let next = MappingState {
                    readiness: Readiness::Ready,
                    ..state
                };
                let effect = if next.intent == Intent::Enabled && next.execution == Execution::Idle
                {
                    TransitionEffect::EligibleForScheduling
                } else {
                    TransitionEffect::None
                };
                TransitionResult {
                    state: next,
                    effect,
                }
            } else {
                TransitionResult {
                    state,
                    effect: TransitionEffect::None,
                }
            }
        }
        ObservedEvent::EndpointChanged => {
            let readiness = match state.readiness {
                Readiness::RecoveryRequired | Readiness::Unconfigured => state.readiness,
                _ => Readiness::EndpointChanged,
            };
            let execution = if state.execution == Execution::Queued {
                Execution::Idle
            } else {
                state.execution
            };
            TransitionResult {
                state: MappingState {
                    readiness,
                    execution,
                    ..state
                },
                effect: TransitionEffect::None,
            }
        }
        ObservedEvent::EndpointValidated => {
            if state.readiness == Readiness::EndpointChanged {
                let next = MappingState {
                    readiness: Readiness::Ready,
                    ..state
                };
                let effect = if next.intent == Intent::Enabled && next.execution == Execution::Idle
                {
                    TransitionEffect::EligibleForScheduling
                } else {
                    TransitionEffect::None
                };
                TransitionResult {
                    state: next,
                    effect,
                }
            } else {
                TransitionResult {
                    state,
                    effect: TransitionEffect::None,
                }
            }
        }
        ObservedEvent::RunFinished(outcome) => {
            let (readiness, execution) = match outcome {
                RunOutcome::Success
                | RunOutcome::SuccessWithConflicts
                | RunOutcome::RetryableFailure => (state.readiness, Execution::Idle),
                RunOutcome::AuthenticationFailure => {
                    let next_r = if state.readiness == Readiness::RecoveryRequired {
                        Readiness::RecoveryRequired
                    } else {
                        Readiness::AuthenticationRequired
                    };
                    (next_r, Execution::Idle)
                }
                RunOutcome::EndpointOrVolumeMismatch => {
                    let next_r = match state.readiness {
                        Readiness::RecoveryRequired | Readiness::VolumeUnavailable => {
                            state.readiness
                        }
                        _ => Readiness::EndpointChanged,
                    };
                    (next_r, Execution::Idle)
                }
                RunOutcome::InvalidConfiguration
                | RunOutcome::CriticalFailure
                | RunOutcome::UnknownFailure
                | RunOutcome::CorruptState => (Readiness::RecoveryRequired, Execution::Idle),
                RunOutcome::Interrupted => (Readiness::RecoveryRequired, Execution::Interrupted),
            };
            TransitionResult {
                state: MappingState {
                    readiness,
                    execution,
                    ..state
                },
                effect: TransitionEffect::None,
            }
        }
        ObservedEvent::RecoveryCompleted { next_readiness } => {
            if state.readiness == Readiness::RecoveryRequired {
                let readiness: Readiness = next_readiness.into();
                let execution = if state.execution == Execution::Interrupted {
                    Execution::Idle
                } else {
                    state.execution
                };
                let next = MappingState {
                    readiness,
                    execution,
                    ..state
                };
                let effect = if next.readiness == Readiness::Ready
                    && next.intent == Intent::Enabled
                    && next.execution == Execution::Idle
                {
                    TransitionEffect::EligibleForScheduling
                } else {
                    TransitionEffect::None
                };
                TransitionResult {
                    state: next,
                    effect,
                }
            } else {
                TransitionResult {
                    state,
                    effect: TransitionEffect::None,
                }
            }
        }
    }
}

/// User-facing actions available for a mapping in its current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AvailableAction {
    /// Complete initial mapping configuration.
    Configure,
    /// Run the guided first-synchronization workflow.
    Initialize,
    /// Start a manual run immediately.
    RunNow,
    /// Pause future scheduling.
    Pause,
    /// Resume scheduling.
    Resume,
    /// Gracefully stop the currently running job.
    StopRun,
    /// Force-terminate a running or stopping job.
    ForceStop,
    /// Reconnect the cloud account.
    ReconnectAccount,
    /// Review an endpoint identity change.
    ReviewEndpointChange,
    /// Open the guided recovery workflow.
    InspectRecovery,
}

/// Returns the actions that are valid for `state`.
pub fn available_actions(state: &MappingState) -> Vec<AvailableAction> {
    let mut actions = Vec::new();

    match state.execution {
        Execution::Running => {
            actions.push(AvailableAction::StopRun);
            actions.push(AvailableAction::ForceStop);
        }
        Execution::Stopping => {
            actions.push(AvailableAction::ForceStop);
        }
        Execution::Idle | Execution::Queued | Execution::Interrupted => {}
    }

    let is_active = matches!(state.execution, Execution::Running | Execution::Stopping);
    if !is_active {
        match state.readiness {
            Readiness::Unconfigured => actions.push(AvailableAction::Configure),
            Readiness::NeedsInitialization => actions.push(AvailableAction::Initialize),
            Readiness::Ready => {
                if state.execution == Execution::Idle && state.intent == Intent::Enabled {
                    actions.push(AvailableAction::RunNow);
                }
            }
            Readiness::VolumeUnavailable => {}
            Readiness::AuthenticationRequired => actions.push(AvailableAction::ReconnectAccount),
            Readiness::EndpointChanged => actions.push(AvailableAction::ReviewEndpointChange),
            Readiness::RecoveryRequired => actions.push(AvailableAction::InspectRecovery),
        }
    }

    if state.readiness != Readiness::Unconfigured {
        match state.intent {
            Intent::Enabled => actions.push(AvailableAction::Pause),
            Intent::Paused => actions.push(AvailableAction::Resume),
        }
    }

    actions
}

/// 24-hour local wall-clock time (`HH:MM`) used for status text formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WallTime {
    hour: u8,
    minute: u8,
}

impl WallTime {
    /// Creates a validated wall-clock time (`hour < 24`, `minute < 60`).
    pub const fn new(hour: u8, minute: u8) -> Option<Self> {
        if hour < 24 && minute < 60 {
            Some(Self { hour, minute })
        } else {
            None
        }
    }

    /// Returns the hour (`0..24`).
    pub const fn hour(self) -> u8 {
        self.hour
    }

    /// Returns the minute (`0..60`).
    pub const fn minute(self) -> u8 {
        self.minute
    }
}

impl fmt::Display for WallTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

/// Compact summary of the last finished run used to render honest status text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LastRunSummary {
    /// Observed outcome of the run.
    pub outcome: RunOutcome,
    /// Local wall-clock time when the run finished.
    pub completed_at: WallTime,
}

/// Formats explicit, honest household status text for a mapping.
///
/// Uses the wording from plan §4 (for example, `"Waiting for PhotoSSD"`,
/// `"Scanning Research"`, `"Last completed at 14:32"`,
/// `"Completed with conflicts"`, and `"Recovery required"`) and never claims
/// `"Everything is up to date"`.
pub fn status_text(
    state: &MappingState,
    mapping_name: &str,
    volume_name: &str,
    last_run: Option<&LastRunSummary>,
) -> String {
    match state.execution {
        Execution::Stopping => return format!("Stopping {mapping_name}"),
        Execution::Running => return format!("Scanning {mapping_name}"),
        Execution::Idle | Execution::Queued | Execution::Interrupted => {}
    }

    match state.readiness {
        Readiness::RecoveryRequired => return "Recovery required".to_owned(),
        Readiness::Unconfigured => return "Not configured".to_owned(),
        Readiness::AuthenticationRequired => return "Authentication required".to_owned(),
        Readiness::EndpointChanged => return "Endpoint changed — review required".to_owned(),
        Readiness::VolumeUnavailable => {
            return match state.intent {
                Intent::Enabled => format!("Waiting for {volume_name}"),
                Intent::Paused => format!("Waiting for {volume_name} (paused)"),
            };
        }
        Readiness::NeedsInitialization => {
            return match state.intent {
                Intent::Enabled => "Needs initialization".to_owned(),
                Intent::Paused => "Needs initialization (paused)".to_owned(),
            };
        }
        Readiness::Ready => {}
    }

    if state.execution == Execution::Queued {
        return format!("Queued to scan {mapping_name}");
    }
    if state.execution == Execution::Interrupted {
        return "Recovery required".to_owned();
    }

    if state.intent == Intent::Paused {
        return match last_run {
            Some(LastRunSummary {
                outcome: RunOutcome::SuccessWithConflicts,
                ..
            }) => "Paused — completed with conflicts".to_owned(),
            Some(LastRunSummary {
                outcome: RunOutcome::Success,
                completed_at,
            }) => format!("Paused — last completed at {completed_at}"),
            _ => "Paused".to_owned(),
        };
    }

    match last_run {
        Some(LastRunSummary {
            outcome: RunOutcome::SuccessWithConflicts,
            ..
        }) => "Completed with conflicts".to_owned(),
        Some(LastRunSummary {
            outcome: RunOutcome::Success,
            completed_at,
        }) => format!("Last completed at {completed_at}"),
        Some(LastRunSummary {
            outcome: RunOutcome::RetryableFailure,
            ..
        }) => "Last run failed — will retry".to_owned(),
        Some(LastRunSummary {
            outcome: RunOutcome::AuthenticationFailure,
            ..
        }) => "Authentication required".to_owned(),
        Some(LastRunSummary {
            outcome: RunOutcome::EndpointOrVolumeMismatch,
            ..
        }) => "Endpoint changed — review required".to_owned(),
        Some(LastRunSummary {
            outcome:
                RunOutcome::InvalidConfiguration
                | RunOutcome::CriticalFailure
                | RunOutcome::UnknownFailure
                | RunOutcome::CorruptState
                | RunOutcome::Interrupted,
            ..
        }) => "Recovery required".to_owned(),
        None => "Ready to sync".to_owned(),
    }
}
