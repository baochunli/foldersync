//! Unit and property tests for the `sync-core` state machine, invariants,
//! available actions, and household status text.

use proptest::prelude::*;
use sync_core::{
    AvailableAction, Execution, Intent, LastRunSummary, MappingState, ObservedEvent,
    PostRecoveryReadiness, Readiness, RunOutcome, TransitionEffect, UserAction, WallTime,
    apply_event, apply_user_action, available_actions, status_text,
};

fn all_readiness() -> [Readiness; 7] {
    [
        Readiness::Unconfigured,
        Readiness::NeedsInitialization,
        Readiness::Ready,
        Readiness::VolumeUnavailable,
        Readiness::AuthenticationRequired,
        Readiness::EndpointChanged,
        Readiness::RecoveryRequired,
    ]
}

fn all_execution() -> [Execution; 5] {
    [
        Execution::Idle,
        Execution::Queued,
        Execution::Running,
        Execution::Stopping,
        Execution::Interrupted,
    ]
}

fn all_intent() -> [Intent; 2] {
    [Intent::Enabled, Intent::Paused]
}

fn all_outcomes() -> [RunOutcome; 10] {
    [
        RunOutcome::Success,
        RunOutcome::SuccessWithConflicts,
        RunOutcome::RetryableFailure,
        RunOutcome::AuthenticationFailure,
        RunOutcome::EndpointOrVolumeMismatch,
        RunOutcome::InvalidConfiguration,
        RunOutcome::CriticalFailure,
        RunOutcome::UnknownFailure,
        RunOutcome::CorruptState,
        RunOutcome::Interrupted,
    ]
}

#[test]
fn reconnecting_volume_never_clears_paused() {
    let state = MappingState::new(
        Readiness::VolumeUnavailable,
        Execution::Idle,
        Intent::Paused,
    );
    let res = apply_event(state, ObservedEvent::VolumeAvailable);
    assert_eq!(res.state.readiness, Readiness::Ready);
    assert_eq!(res.state.intent, Intent::Paused);
    assert_eq!(res.state.execution, Execution::Idle);
    assert_eq!(res.effect, TransitionEffect::None);
}

#[test]
fn resume_queues_run_only_when_ready() {
    for readiness in all_readiness() {
        let state = MappingState::new(readiness, Execution::Idle, Intent::Paused);
        let res = apply_user_action(state, UserAction::Resume);
        assert_eq!(res.state.intent, Intent::Enabled);
        assert_eq!(res.state.readiness, readiness);

        if readiness == Readiness::Ready {
            assert_eq!(res.state.execution, Execution::Queued);
            assert_eq!(res.effect, TransitionEffect::ScheduleRun);
        } else {
            assert_eq!(res.state.execution, Execution::Idle);
            assert_eq!(res.effect, TransitionEffect::None);
        }
    }
}

#[test]
fn pause_does_not_stop_a_running_run() {
    let running = MappingState::new(Readiness::Ready, Execution::Running, Intent::Enabled);
    let res = apply_user_action(running, UserAction::Pause);
    assert_eq!(res.state.intent, Intent::Paused);
    assert_eq!(res.state.execution, Execution::Running);
    assert_eq!(res.effect, TransitionEffect::None);

    // Pause does not stop a running run, and Resume during Running keeps Running.
    let resumed_while_running = apply_user_action(res.state, UserAction::Resume);
    assert_eq!(resumed_while_running.state.intent, Intent::Enabled);
    assert_eq!(resumed_while_running.state.execution, Execution::Running);
    assert_eq!(resumed_while_running.effect, TransitionEffect::None);

    // Pause does cancel a merely queued run before launch.
    let queued = MappingState::new(Readiness::Ready, Execution::Queued, Intent::Enabled);
    let res = apply_user_action(queued, UserAction::Pause);
    assert_eq!(res.state.intent, Intent::Paused);
    assert_eq!(res.state.execution, Execution::Idle);
}

#[test]
fn stop_force_stop_and_quit_follow_plan_semantics() {
    let running = MappingState::new(Readiness::Ready, Execution::Running, Intent::Enabled);

    // StopRun requests graceful stop and shows Stopping.
    let stopped = apply_user_action(running, UserAction::StopRun);
    assert_eq!(stopped.state.execution, Execution::Stopping);
    assert_eq!(stopped.state.readiness, Readiness::Ready);
    assert_eq!(stopped.effect, TransitionEffect::RequestGracefulStop);

    // ForceStop terminates and enters Interrupted + RecoveryRequired.
    let forced = apply_user_action(stopped.state, UserAction::ForceStop);
    assert_eq!(forced.state.execution, Execution::Interrupted);
    assert_eq!(forced.state.readiness, Readiness::RecoveryRequired);
    assert_eq!(forced.effect, TransitionEffect::ForceTerminateChild);

    // QuitRequested stops scheduling and requests graceful stop without touching intent.
    let quit = apply_user_action(running, UserAction::QuitRequested);
    assert_eq!(quit.state.execution, Execution::Stopping);
    assert_eq!(quit.state.intent, Intent::Enabled);
    assert_eq!(
        quit.effect,
        TransitionEffect::StopSchedulingAndRequestGracefulStop
    );

    // CloseWindow leaves state untouched.
    let closed = apply_user_action(running, UserAction::CloseWindow);
    assert_eq!(closed.state, running);
    assert_eq!(closed.effect, TransitionEffect::None);
}

#[test]
fn critical_unknown_corrupt_and_interrupted_outcomes_latch_recovery_required() {
    for outcome in [
        RunOutcome::CriticalFailure,
        RunOutcome::UnknownFailure,
        RunOutcome::CorruptState,
        RunOutcome::Interrupted,
    ] {
        let running = MappingState::new(Readiness::Ready, Execution::Running, Intent::Enabled);
        let after_fail = apply_event(running, ObservedEvent::RunFinished(outcome));
        assert_eq!(after_fail.state.readiness, Readiness::RecoveryRequired);
        if outcome == RunOutcome::Interrupted {
            assert_eq!(after_fail.state.execution, Execution::Interrupted);
        }

        // Non-recovery events and user actions cannot clear RecoveryRequired.
        let mut s = after_fail.state;
        for action in [
            UserAction::CloseWindow,
            UserAction::Pause,
            UserAction::Resume,
            UserAction::RunNow,
            UserAction::StopRun,
            UserAction::ForceStop,
            UserAction::QuitRequested,
        ] {
            s = apply_user_action(s, action).state;
            assert_eq!(s.readiness, Readiness::RecoveryRequired);
        }
        for event in [
            ObservedEvent::Configured,
            ObservedEvent::InitializationCompleted,
            ObservedEvent::ScheduleTriggered,
            ObservedEvent::RunStarted,
            ObservedEvent::VolumeLost,
            ObservedEvent::VolumeAvailable,
            ObservedEvent::AuthenticationFailed,
            ObservedEvent::AuthenticationRestored,
            ObservedEvent::EndpointChanged,
            ObservedEvent::EndpointValidated,
            ObservedEvent::RunFinished(RunOutcome::Success),
        ] {
            s = apply_event(s, event).state;
            assert_eq!(s.readiness, Readiness::RecoveryRequired);
        }

        // Only explicit RecoveryCompleted clears RecoveryRequired.
        let recovered = apply_event(
            s,
            ObservedEvent::RecoveryCompleted {
                next_readiness: PostRecoveryReadiness::Ready,
            },
        );
        assert_eq!(recovered.state.readiness, Readiness::Ready);
        assert_eq!(recovered.state.execution, Execution::Idle);
    }
}

#[test]
fn available_actions_match_state_capabilities() {
    let ready_enabled = MappingState::new(Readiness::Ready, Execution::Idle, Intent::Enabled);
    let actions = available_actions(&ready_enabled);
    assert!(actions.contains(&AvailableAction::RunNow));
    assert!(actions.contains(&AvailableAction::Pause));
    assert!(!actions.contains(&AvailableAction::Resume));
    assert!(!actions.contains(&AvailableAction::StopRun));

    let running = MappingState::new(Readiness::Ready, Execution::Running, Intent::Enabled);
    let actions = available_actions(&running);
    assert!(actions.contains(&AvailableAction::Pause));
    assert!(actions.contains(&AvailableAction::StopRun));
    assert!(actions.contains(&AvailableAction::ForceStop));
    assert!(!actions.contains(&AvailableAction::RunNow));

    let recovery = MappingState::new(
        Readiness::RecoveryRequired,
        Execution::Interrupted,
        Intent::Paused,
    );
    let actions = available_actions(&recovery);
    assert!(actions.contains(&AvailableAction::InspectRecovery));
    assert!(actions.contains(&AvailableAction::Resume));
    assert!(!actions.contains(&AvailableAction::RunNow));
}

#[test]
fn status_text_matches_plan_wording_and_never_claims_up_to_date() {
    let t = WallTime::new(14, 32).expect("valid time");
    assert_eq!(t.to_string(), "14:32");
    assert!(WallTime::new(24, 0).is_none());
    assert!(WallTime::new(12, 60).is_none());

    let waiting = MappingState::new(
        Readiness::VolumeUnavailable,
        Execution::Idle,
        Intent::Enabled,
    );
    assert_eq!(
        status_text(&waiting, "Research", "PhotoSSD", None),
        "Waiting for PhotoSSD"
    );

    let scanning = MappingState::new(Readiness::Ready, Execution::Running, Intent::Enabled);
    assert_eq!(
        status_text(&scanning, "Research", "PhotoSSD", None),
        "Scanning Research"
    );

    let ready = MappingState::new(Readiness::Ready, Execution::Idle, Intent::Enabled);
    let ok_run = LastRunSummary {
        outcome: RunOutcome::Success,
        completed_at: t,
    };
    assert_eq!(
        status_text(&ready, "Research", "PhotoSSD", Some(&ok_run)),
        "Last completed at 14:32"
    );

    let conflict_run = LastRunSummary {
        outcome: RunOutcome::SuccessWithConflicts,
        completed_at: t,
    };
    assert_eq!(
        status_text(&ready, "Research", "PhotoSSD", Some(&conflict_run)),
        "Completed with conflicts"
    );

    let recovery = MappingState::new(
        Readiness::RecoveryRequired,
        Execution::Interrupted,
        Intent::Enabled,
    );
    assert_eq!(
        status_text(&recovery, "Research", "PhotoSSD", Some(&ok_run)),
        "Recovery required"
    );

    // Exhaustive check across every state and outcome: never say "up to date".
    for readiness in all_readiness() {
        for execution in all_execution() {
            for intent in all_intent() {
                let s = MappingState::new(readiness, execution, intent);
                let text_none = status_text(&s, "Research", "PhotoSSD", None);
                assert!(!text_none.to_lowercase().contains("up to date"));
                for outcome in all_outcomes() {
                    let summary = LastRunSummary {
                        outcome,
                        completed_at: t,
                    };
                    let text = status_text(&s, "Research", "PhotoSSD", Some(&summary));
                    assert!(!text.to_lowercase().contains("up to date"));
                }
            }
        }
    }
}

#[test]
fn unresolved_needs_initialization_survives_resume_volume_lost_and_volume_available() {
    let draft = MappingState::new(
        Readiness::NeedsInitialization,
        Execution::Idle,
        Intent::Paused,
    );
    let after_resume = apply_user_action(draft, UserAction::Resume);
    assert_eq!(after_resume.state.intent, Intent::Enabled);
    assert_eq!(after_resume.state.readiness, Readiness::NeedsInitialization);
    assert_eq!(after_resume.state.execution, Execution::Idle);

    let after_lost = apply_event(after_resume.state, ObservedEvent::VolumeLost);
    assert_eq!(after_lost.state.readiness, Readiness::VolumeUnavailable);

    let after_back = apply_event(after_lost.state, ObservedEvent::VolumeAvailable);
    assert_eq!(
        after_back.state.readiness,
        Readiness::NeedsInitialization,
        "reconnecting volume must not bypass NeedsInitialization"
    );
    assert_eq!(after_back.effect, TransitionEffect::None);

    let after_tick = apply_event(after_back.state, ObservedEvent::ScheduleTriggered);
    assert_eq!(after_tick.state.execution, Execution::Idle);
    assert_eq!(after_tick.effect, TransitionEffect::None);

    let after_init = apply_event(after_back.state, ObservedEvent::InitializationCompleted);
    assert_eq!(after_init.state.readiness, Readiness::Ready);
    assert_eq!(after_init.effect, TransitionEffect::EligibleForScheduling);
}

#[test]
fn unresolved_auth_endpoint_and_recovery_gates_survive_unrelated_events() {
    // AuthenticationRequired survives VolumeLost -> VolumeAvailable.
    let auth_hold = MappingState::new(
        Readiness::AuthenticationRequired,
        Execution::Idle,
        Intent::Enabled,
    );
    let lost = apply_event(auth_hold, ObservedEvent::VolumeLost).state;
    let back = apply_event(lost, ObservedEvent::VolumeAvailable).state;
    assert_eq!(back.readiness, Readiness::AuthenticationRequired);
    let restored = apply_event(back, ObservedEvent::AuthenticationRestored).state;
    assert_eq!(restored.readiness, Readiness::Ready);

    // EndpointChanged survives AuthenticationFailed -> AuthenticationRestored.
    let endpoint_hold =
        MappingState::new(Readiness::EndpointChanged, Execution::Idle, Intent::Enabled);
    let auth_bad = apply_event(endpoint_hold, ObservedEvent::AuthenticationFailed).state;
    let auth_ok = apply_event(auth_bad, ObservedEvent::AuthenticationRestored).state;
    assert_eq!(auth_ok.readiness, Readiness::EndpointChanged);
    let validated = apply_event(auth_ok, ObservedEvent::EndpointValidated).state;
    assert_eq!(validated.readiness, Readiness::Ready);

    // VolumeUnavailable survives AuthenticationFailed -> AuthenticationRestored.
    let vol_hold = MappingState::new(
        Readiness::VolumeUnavailable,
        Execution::Idle,
        Intent::Enabled,
    );
    let auth_bad = apply_event(vol_hold, ObservedEvent::AuthenticationFailed).state;
    let auth_ok = apply_event(auth_bad, ObservedEvent::AuthenticationRestored).state;
    assert_eq!(auth_ok.readiness, Readiness::VolumeUnavailable);
    let vol_ok = apply_event(auth_ok, ObservedEvent::VolumeAvailable).state;
    assert_eq!(vol_ok.readiness, Readiness::Ready);
}

#[test]
fn status_text_labels_past_failures_as_history_once_hold_is_cleared() {
    let t = WallTime::new(14, 32).expect("valid time");
    let ready = MappingState::new(Readiness::Ready, Execution::Idle, Intent::Enabled);

    for (outcome, forbidden_current_hold) in [
        (RunOutcome::Interrupted, "Recovery required"),
        (RunOutcome::CriticalFailure, "Recovery required"),
        (RunOutcome::AuthenticationFailure, "Authentication required"),
        (
            RunOutcome::EndpointOrVolumeMismatch,
            "Endpoint changed — review required",
        ),
    ] {
        let summary = LastRunSummary {
            outcome,
            completed_at: t,
        };
        let text = status_text(&ready, "Research", "PhotoSSD", Some(&summary));
        assert_ne!(
            text, forbidden_current_hold,
            "cleared hold for {outcome:?} must not render as current blocker"
        );
        assert!(
            text.contains("Last run") && text.contains("14:32"),
            "expected historical wording with timestamp for {outcome:?}, got: {text}"
        );
    }
}

fn arb_readiness() -> impl Strategy<Value = Readiness> {
    prop::sample::select(all_readiness().to_vec())
}

fn arb_execution() -> impl Strategy<Value = Execution> {
    prop::sample::select(all_execution().to_vec())
}

fn arb_non_resume_action() -> impl Strategy<Value = UserAction> {
    prop::sample::select(vec![
        UserAction::CloseWindow,
        UserAction::Pause,
        UserAction::RunNow,
        UserAction::StopRun,
        UserAction::ForceStop,
        UserAction::QuitRequested,
    ])
}

fn arb_event() -> impl Strategy<Value = ObservedEvent> {
    prop_oneof![
        Just(ObservedEvent::Configured),
        Just(ObservedEvent::InitializationCompleted),
        Just(ObservedEvent::ScheduleTriggered),
        Just(ObservedEvent::RunStarted),
        Just(ObservedEvent::VolumeLost),
        Just(ObservedEvent::VolumeAvailable),
        Just(ObservedEvent::AuthenticationFailed),
        Just(ObservedEvent::AuthenticationRestored),
        Just(ObservedEvent::EndpointChanged),
        Just(ObservedEvent::EndpointValidated),
        prop::sample::select(all_outcomes().to_vec()).prop_map(ObservedEvent::RunFinished),
        prop::sample::select(vec![
            PostRecoveryReadiness::Ready,
            PostRecoveryReadiness::NeedsInitialization,
            PostRecoveryReadiness::VolumeUnavailable,
            PostRecoveryReadiness::AuthenticationRequired,
            PostRecoveryReadiness::EndpointChanged,
        ])
        .prop_map(|next_readiness| ObservedEvent::RecoveryCompleted { next_readiness }),
    ]
}

fn arb_non_recovery_event() -> impl Strategy<Value = ObservedEvent> {
    prop_oneof![
        Just(ObservedEvent::Configured),
        Just(ObservedEvent::InitializationCompleted),
        Just(ObservedEvent::ScheduleTriggered),
        Just(ObservedEvent::RunStarted),
        Just(ObservedEvent::VolumeLost),
        Just(ObservedEvent::VolumeAvailable),
        Just(ObservedEvent::AuthenticationFailed),
        Just(ObservedEvent::AuthenticationRestored),
        Just(ObservedEvent::EndpointChanged),
        Just(ObservedEvent::EndpointValidated),
        prop::sample::select(all_outcomes().to_vec()).prop_map(ObservedEvent::RunFinished),
    ]
}

proptest! {
    #[test]
    fn no_non_resume_action_clears_paused(
        readiness in arb_readiness(),
        execution in arb_execution(),
        action in arb_non_resume_action(),
    ) {
        let state = MappingState::new(readiness, execution, Intent::Paused);
        let next = apply_user_action(state, action).state;
        prop_assert_eq!(next.intent, Intent::Paused);
    }

    #[test]
    fn no_observed_event_clears_paused(
        readiness in arb_readiness(),
        execution in arb_execution(),
        event in arb_event(),
    ) {
        let state = MappingState::new(readiness, execution, Intent::Paused);
        let next = apply_event(state, event).state;
        prop_assert_eq!(next.intent, Intent::Paused);
    }

    #[test]
    fn nothing_clears_recovery_required_except_recovery_completed(
        execution in arb_execution(),
        intent in prop::sample::select(all_intent().to_vec()),
        event in arb_non_recovery_event(),
        action in prop::sample::select(vec![
            UserAction::CloseWindow,
            UserAction::Pause,
            UserAction::Resume,
            UserAction::RunNow,
            UserAction::StopRun,
            UserAction::ForceStop,
            UserAction::QuitRequested,
        ]),
    ) {
        let state = MappingState::new(Readiness::RecoveryRequired, execution, intent);
        prop_assert_eq!(apply_event(state, event).state.readiness, Readiness::RecoveryRequired);
        prop_assert_eq!(apply_user_action(state, action).state.readiness, Readiness::RecoveryRequired);
    }
}
